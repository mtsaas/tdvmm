#!/usr/bin/env bash
# deterministic-vmm Phase-2a: bake a docker-compose stack into a closed-world,
# fast-forwardable guest initramfs.
#
#   bake-stack.sh <compose.yml> [--name <stack>] [--mem <MiB>] [--working-set <MiB>]
#
# Runs on the HOST at build time. This is the loud boundary of 2a: a well-defined
# SUPPORTED SUBSET in, a per-stack baked initramfs out, and a clear bake-time
# REJECT for anything outside the subset (never a silent hang at runtime).
#
# Pipeline:
#   1. VALIDATE the compose file against the supported subset (bake_compose.py).
#      warn+strip published ports; REJECT absolute host binds, external networks,
#      pull_policy: always, network_mode: host, and unpinned build: bases.
#      Supported: image: + build: services, relative ro/rw binds, named volumes,
#      healthchecks + depends_on: service_healthy (resolved by the guest ticker).
#   2. For each image:, resolve to a digest and pull BY digest; squash large images
#      to a single vfs layer (reproducibly, --timestamp) with the config-equivalence
#      GATE; bake into the seed store. Small images are baked plain.
#   3. Emit compose.lock.yml -- the ONLY compose file the guest sees: images pinned
#      by digest, ports stripped, relative RO binds materialized into the guest and
#      rewritten to in-guest absolute paths, COMPOSE_PROJECT_NAME pinned, named
#      volumes kept.
#   4. Estimate guest RAM and WARN if the configured RAM looks short.
#   5. Assemble the per-stack initramfs (build_rootfs.sh in stack mode) and record
#      a manifest entry (stack.lock: image digests + artifact hashes).
#
# Digest-pinning note: a podman save/load roundtrip does NOT preserve an image's
# RepoDigest, so PLAIN images are pulled straight into the seed (upstream digest
# preserved -> lock pins the real upstream @sha256), while SQUASHED images are
# pinned to their post-load SEED digest, which --timestamp makes reproducible
# across bakes. Both forms resolve under the guest's --pull=never.
#
# Requires host podman (+ subuid/subgid) and python3-pyyaml. Network only for the
# one-time image pulls. Closed world at guest runtime (nothing pulled).
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ALPINE_DIR="$HERE/initramfs-alpine"
PY="$HERE/bake_compose.py"

# --- args -------------------------------------------------------------------
COMPOSE=""
STACK_NAME=""
DVMM_OUT=""
# 2a spec default is 4 GiB, but the current VMM maps guest RAM only BELOW the
# 32-bit MMIO gap (arch.rs MMIO_MEM_START = 0xc000_0000 = 3 GiB), so 4 GiB is not
# bootable without a VMM high-memory region (VMM-core work, outside 2a's
# guest-image+host-tooling scope -- flagged for an overseer ruling). We default to
# the VMM max, 3072 MiB; the dogfood peaks ~1.2 GiB, so this is ample headroom.
VMM_MAX_MEM_MIB=3072
MEM_MIB=3072
WORKING_SET_MIB=512       # workload working-set allowance for the RAM estimate
SQUASH_THRESHOLD_MIB=100  # images larger than this are squashed to one vfs layer
while [ $# -gt 0 ]; do
  case "$1" in
    --name)        STACK_NAME="$2"; shift 2 ;;
    -o|--out)      DVMM_OUT="$2"; shift 2 ;;
    --mem)         MEM_MIB="$2"; shift 2 ;;
    --working-set) WORKING_SET_MIB="$2"; shift 2 ;;
    --squash-threshold) SQUASH_THRESHOLD_MIB="$2"; shift 2 ;;
    -h|--help) sed -n '2,26p' "$0"; exit 0 ;;
    -*) echo "bake-stack: unknown flag $1" >&2; exit 2 ;;
    *) COMPOSE="$1"; shift ;;
  esac
done
[ -n "$COMPOSE" ] || { echo "usage: bake-stack.sh <compose.yml> [--name n] [--mem MiB] [--working-set MiB]" >&2; exit 2; }
[ -f "$COMPOSE" ] || { echo "bake-stack: compose file not found: $COMPOSE" >&2; exit 2; }
COMPOSE="$(cd "$(dirname "$COMPOSE")" && pwd)/$(basename "$COMPOSE")"
COMPOSE_DIR="$(dirname "$COMPOSE")"
[ -n "$STACK_NAME" ] || STACK_NAME="$(basename "$COMPOSE_DIR")"
PROJECT="dvmm_${STACK_NAME}"
# The fixed guest wall-clock epoch (also used to make squashed images reproducible).
BUILD_EPOCH="1785542400"
# Busybox: the plain image the 2a container self-test runs (baked into every guest).
BUSYBOX_REF="docker.io/library/busybox@sha256:dc2d74b28e4cf8984fa52af1f39bc7c3d9c73760b41a74d629f5d11b1ab28616"

command -v podman  >/dev/null || { echo "bake-stack: host podman required" >&2; exit 2; }
command -v python3 >/dev/null || { echo "bake-stack: python3 required" >&2; exit 2; }

echo "== bake-stack: stack=$STACK_NAME project=$PROJECT mem=${MEM_MIB}MiB =="
echo "   compose: $COMPOSE"

WORK="$(mktemp -d)"
cleanup() { podman unshare rm -rf "$WORK" 2>/dev/null || true; rm -rf "$WORK" 2>/dev/null || true; }
trap cleanup EXIT

CLEAN_CONF="$WORK/containers.conf"; printf '[engine]\n' > "$CLEAN_CONF"
export CONTAINERS_CONF="$CLEAN_CONF"
PODMAN_VERSION="$(podman --version | awk '{print $3}')"

# --- 1. VALIDATE (fail fast, before any pull) -------------------------------
echo "== validate =="
VJSON="$WORK/validate.json"
if ! python3 "$PY" validate "$COMPOSE" > "$VJSON" 2>"$WORK/validate.err"; then
  cat "$WORK/validate.err" >&2
  echo "bake-stack: REJECTED at validation (see DVMM_BAKE_REJECT above)" >&2
  exit 3
fi
cat "$WORK/validate.err" >&2 || true   # surface any DVMM_BAKE_WARN (ports stripped)
mapfile -t IMAGES < <(python3 -c 'import json,sys;[print(i) for i in json.load(open(sys.argv[1]))["images"]]' "$VJSON")
echo "   images: ${IMAGES[*]}"
# builds (2b): one line per build: service -- service<TAB>context<TAB>dockerfile<TAB>image_tag<TAB>base,base
BUILDS_TSV="$WORK/builds.tsv"
python3 -c '
import json,sys
for b in json.load(open(sys.argv[1])).get("builds",[]):
    print("\t".join([b["service"], b["context"], b["dockerfile"], b["image_tag"], ",".join(b["bases"])]))
' "$VJSON" > "$BUILDS_TSV"
if [ -s "$BUILDS_TSV" ]; then
  echo "   builds: $(cut -f4 "$BUILDS_TSV" | tr '\n' ' ')"
fi

# --- 2. bake each image -----------------------------------------------------
# Scratch build store: measure sizes, squash heavy images; discarded afterwards.
BSTORE="$WORK/build-storage"; BRUN="$WORK/build-run"; mkdir -p "$BSTORE" "$BRUN"
bp() { podman --root "$BSTORE" --runroot "$BRUN" --storage-driver vfs "$@"; }

PLAIN_REFS=()          # pulled straight into the seed (upstream digest preserved)
SQUASH_TARS=()         # loaded into the seed
SQUASH_TAGS=()         # local baked tag, parallel to SQUASH_TARS / SQUASH_UPSTREAM
SQUASH_UPSTREAM=()
PROV="$WORK/provenance.txt"; : > "$PROV"   # per-image ledger for stack.lock
declare -A PLAIN_PIN
TOTAL_IMG_MIB=0

record_prov() { printf '%s\n' "$1" >> "$PROV"; }

bake_one() {  # <ref>
  local ref="$1" bytes mib diffid
  bp pull -q "$ref" >/dev/null
  bytes="$(bp image inspect "$ref" --format '{{.Size}}')"
  mib=$(( bytes / 1048576 )); TOTAL_IMG_MIB=$(( TOTAL_IMG_MIB + mib ))
  diffid="$(bp image inspect "$ref" --format '{{range .RootFS.Layers}}{{println .}}{{end}}' | tr -d '[:space:]')"
  if [ "$mib" -le "$SQUASH_THRESHOLD_MIB" ]; then
    # Prefer the requested digest (a ref given as repo@sha256 already IS the pin
    # that resolves in the seed); fall back to RepoDigests[0] for tag-only refs.
    local canon
    case "$ref" in
      *@sha256:*) canon="$ref" ;;
      *) canon="$(bp image inspect "$ref" --format '{{if .RepoDigests}}{{index .RepoDigests 0}}{{else}}{{.Id}}{{end}}')" ;;
    esac
    PLAIN_REFS+=("$ref"); PLAIN_PIN["$ref"]="$canon"
    record_prov "image  policy=plain   upstream=$ref  diffid=$diffid  size_mib=$mib"
    echo "   [plain]  $ref  (${mib} MiB)"
    return
  fi
  # squash: reproducible single-layer repackage + config-equivalence gate
  local base short tag ctx
  base="$(echo "$ref" | sed -E 's#[@:].*$##; s#.*/##')"
  short="$(echo "$ref" | grep -oE '[0-9a-f]{64}' | head -1 | cut -c1-12)"
  [ -n "$short" ] || short="$(echo "$ref" | sha256sum | cut -c1-12)"
  tag="localhost/dvmm-${base}-${short}:baked"
  ctx="$WORK/ctx-${#SQUASH_TARS[@]}"; mkdir -p "$ctx"
  printf 'FROM %s\n' "$ref" > "$ctx/Containerfile"
  local nonblank fromlines
  nonblank="$(grep -cvE '^[[:space:]]*(#|$)' "$ctx/Containerfile")"
  fromlines="$(grep -ciE '^[[:space:]]*FROM[[:space:]]' "$ctx/Containerfile")"
  [ "$nonblank" -eq 1 ] && [ "$fromlines" -eq 1 ] || { echo "bake-stack: squash Containerfile not a pure single-FROM"; exit 2; }
  bp build --squash-all --pull=never --timestamp "$BUILD_EPOCH" -t "$tag" -f "$ctx/Containerfile" "$ctx" >/dev/null
  local gate_ok=1 f up sq
  for f in Entrypoint Cmd Env Volumes WorkingDir; do
    up="$(bp image inspect "$ref" --format "{{json .Config.$f}}")"
    sq="$(bp image inspect "$tag" --format "{{json .Config.$f}}")"
    if [ "$up" != "$sq" ]; then
      echo "GATE FAIL: Config.$f drifted during squash of $ref"; echo "  upstream: $up"; echo "  squashed: $sq"; gate_ok=0
    fi
  done
  [ "$gate_ok" -eq 1 ] || { echo "bake-stack: config-equivalence gate failed for $ref" >&2; exit 3; }
  local sq_diffid tar
  sq_diffid="$(bp image inspect "$tag" --format '{{range .RootFS.Layers}}{{println .}}{{end}}' | tr -d '[:space:]')"
  tar="$WORK/squash-${#SQUASH_TARS[@]}.tar"
  bp save -o "$tar" "$tag" >/dev/null
  SQUASH_TARS+=("$tar"); SQUASH_TAGS+=("$tag"); SQUASH_UPSTREAM+=("$ref")
  # pinned digest is read from the SEED after load (below); record provenance now.
  record_prov "image  policy=squash  upstream=$ref  seed_tag=$tag  src_diffid=$sq_diffid  size_mib=$mib  GATE=ok"
  echo "   [squash] $ref  (${mib} MiB)  -> $tag  (GATE ok)"
}

# build a build: service HOST-SIDE at bake time (2b). The base images are
# digest-pinned (bake_compose enforced it), the build is reproducible
# (--squash-all single layer + --timestamp mtime normalization + the Dockerfile's
# own -trimpath/-buildid=/CGO_ENABLED=0 for Go), and the RESULT flows through the
# SAME seed-load / pin / compose.lock path as a squashed image. Content-identity =
# the squashed layer's DiffID (image IDs are not reproducible; filesystem is).
build_one() {  # <service> <context> <dockerfile> <image_tag> <bases-csv>
  local svc="$1" ctx="$2" df="$3" tag="$4" bases="$5"
  local bytes mib diffid tar toolchain="" b repo
  echo "   [build]  service=$svc  context=$ctx  dockerfile=$df  -> $tag"
  # Network is allowed at build time (pull pinned bases + apk/go). NOT --pull=never.
  bp build --squash-all --timestamp "$BUILD_EPOCH" -t "$tag" -f "$df" "$ctx" >/dev/null
  bytes="$(bp image inspect "$tag" --format '{{.Size}}')"
  mib=$(( bytes / 1048576 )); TOTAL_IMG_MIB=$(( TOTAL_IMG_MIB + mib ))
  diffid="$(bp image inspect "$tag" --format '{{range .RootFS.Layers}}{{println .}}{{end}}' | tr -d '[:space:]')"
  tar="$WORK/squash-${#SQUASH_TARS[@]}.tar"
  bp save -o "$tar" "$tag" >/dev/null
  # Reuse the squash seed-load path: keyed by the BUILD TAG (emit-lock keys build
  # outputs by that tag), pinned to its post-load seed digest (reproducible).
  SQUASH_TARS+=("$tar"); SQUASH_TAGS+=("$tag"); SQUASH_UPSTREAM+=("$tag")
  # provenance: base pins (the reproducibility anchor incl. the Go toolchain image)
  # + a toolchain= line captured from a base's GOLANG_VERSION (no container run).
  IFS=',' read -r -a barr <<< "$bases"
  for b in "${barr[@]}"; do
    [ -n "$b" ] || continue
    record_prov "build_base  service=$svc  base=$b"
    if [ -z "$toolchain" ]; then
      local gv
      # Best-effort Go-toolchain capture: a NON-Go base (e.g. a plain Alpine
      # build context) simply has no GOLANG_VERSION, so `grep` matches nothing and
      # exits 1 -- `|| true` keeps that expected no-match from tripping
      # `set -e`/`pipefail` and aborting the bake.
      gv="$(bp image inspect "$b" --format '{{range .Config.Env}}{{println .}}{{end}}' 2>/dev/null | grep -E '^GOLANG_VERSION=' | head -1 | cut -d= -f2 || true)"
      [ -n "$gv" ] && toolchain="go$gv"
    fi
  done
  [ -n "$toolchain" ] && record_prov "toolchain   service=$svc  $toolchain"
  record_prov "image  policy=build   service=$svc  tag=$tag  content_id=$diffid  size_mib=$mib"
  echo "   [build]  $tag  (${mib} MiB)  content_id=$diffid  ${toolchain:+toolchain=$toolchain}"
}

for ref in "${IMAGES[@]}"; do bake_one "$ref"; done
# build: services (host-side build at bake time).
if [ -s "$BUILDS_TSV" ]; then
  echo "== build build: services (host-side) =="
  while IFS=$'\t' read -r svc ctx df tag bases; do
    [ -n "$svc" ] || continue
    build_one "$svc" "$ctx" "$df" "$tag" "$bases"
  done < "$BUILDS_TSV"
fi
# Busybox for the 2a container self-test (always baked; plain).
echo "== bake self-test image (busybox, plain) =="
bake_one "$BUSYBOX_REF"
SELFTEST_PIN="${PLAIN_PIN[$BUSYBOX_REF]}"

# --- 3. build the seed store (userns): pull plains, load squashes -----------
echo "== build seed store =="
SEED="$WORK/seed"; STORE="$SEED/storage"; RUNROOT="$WORK/seedrun"
mkdir -p "$STORE" "$RUNROOT"
printf '%s\n' "${PLAIN_REFS[@]}" > "$WORK/plain.txt"
printf '%s\n' "${SQUASH_TARS[@]}" > "$WORK/squashtars.txt"
: > "$WORK/squashmap.txt"          # "<upstream>\t<local-tag>" per squashed image
for i in "${!SQUASH_TAGS[@]}"; do printf '%s\t%s\n' "${SQUASH_UPSTREAM[$i]}" "${SQUASH_TAGS[$i]}" >> "$WORK/squashmap.txt"; done
export STORE RUNROOT WORK
podman unshare bash <<'UNSHARE'
set -euo pipefail
sp() { podman --root "$STORE" --runroot "$RUNROOT" --storage-driver vfs "$@"; }
while IFS= read -r ref; do [ -n "$ref" ] || continue; sp pull -q "$ref" >/dev/null; done < "$WORK/plain.txt"
while IFS= read -r tar; do [ -n "$tar" ] || continue; sp load -q -i "$tar" >/dev/null; done < "$WORK/squashtars.txt"
# resolve the post-load SEED digest of each squashed image (reproducible pin).
: > "$WORK/seedpins.txt"
while IFS=$'\t' read -r upstream tag; do
  [ -n "$tag" ] || continue
  repo="${tag%:*}"
  pin="$(sp image inspect "$tag" --format '{{range .RepoDigests}}{{println .}}{{end}}' | grep "^${repo}@" | head -1)"
  printf '%s\t%s\n' "$upstream" "$pin" >> "$WORK/seedpins.txt"
done < "$WORK/squashmap.txt"
echo "--- baked seed store contents ---"
sp images --digests --no-trunc
UNSHARE
# Relocatable store: drop libpod state (records an absolute graphroot); the guest
# recreates it fresh at /var/lib/containers/storage on first use.
rm -rf "$STORE/libpod" "$STORE/db.sql"

# --- 4. emit compose.lock.yml + materialize binds ---------------------------
echo "== emit compose.lock.yml =="
# assemble the original-ref -> pinned-ref map (plains: upstream digest; squashes:
# seed digest read above) and append the resolved squash pins to the provenance.
: > "$WORK/digests.txt"
for ref in "${PLAIN_REFS[@]}"; do printf '%s\t%s\n' "$ref" "${PLAIN_PIN[$ref]}" >> "$WORK/digests.txt"; done
if [ -s "$WORK/seedpins.txt" ]; then
  cat "$WORK/seedpins.txt" >> "$WORK/digests.txt"
  while IFS=$'\t' read -r upstream pin; do
    [ -n "$pin" ] || continue
    record_prov "pin    upstream=$upstream  pinned=$pin"
  done < "$WORK/seedpins.txt"
fi
DIGESTS_JSON="$(python3 -c '
import json,sys
m={}
for line in open(sys.argv[1]):
    line=line.rstrip("\n")
    if not line.strip(): continue
    k,v=line.split("\t",1); m[k]=v
print(json.dumps(m))' "$WORK/digests.txt")"

BINDS_BASE="/var/lib/dvmm-stack/binds"
LOCK="$WORK/compose.lock.yml"
BMAN="$WORK/binds.manifest"
python3 "$PY" emit-lock "$COMPOSE" --digests "$DIGESTS_JSON" \
  --binds-base "$BINDS_BASE" --project "$PROJECT" --out "$LOCK" --binds-manifest "$BMAN"

# materialize relative RO binds into a staging tree (baked into the guest).
BINDS_STAGE="$WORK/binds"; mkdir -p "$BINDS_STAGE"
if [ -s "$BMAN" ]; then
  while IFS=$'\t' read -r src dest; do
    [ -n "$src" ] || continue
    mkdir -p "$BINDS_STAGE/$(dirname "$dest")"
    cp -a "$src" "$BINDS_STAGE/$dest"
    echo "   materialized  $src  ->  $BINDS_BASE/$dest"
  done < "$BMAN"
fi

# --- 5. RAM estimate --------------------------------------------------------
# guest RAM >= 2.5x total uncompressed image size + workload working set + 512 base
EST_MIB=$(python3 -c "import math;print(int(math.ceil(2.5*$TOTAL_IMG_MIB + $WORKING_SET_MIB + 512)))")
echo "== RAM estimate =="
echo "   total image size: ${TOTAL_IMG_MIB} MiB;  estimate >= ${EST_MIB} MiB (2.5x img + ${WORKING_SET_MIB} ws + 512 base)"
if [ "$MEM_MIB" -lt "$EST_MIB" ]; then
  echo "DVMM_BAKE_WARN: configured guest RAM ${MEM_MIB} MiB is below the estimate ${EST_MIB} MiB." >&2
  echo "DVMM_BAKE_WARN: the stack may OOM; raise --mem (no virtio-blk in 2a -- RAM-bound stacks report, not spill)." >&2
else
  echo "   configured ${MEM_MIB} MiB >= estimate ${EST_MIB} MiB (OK)"
fi
if [ "$MEM_MIB" -gt "$VMM_MAX_MEM_MIB" ]; then
  echo "DVMM_BAKE_WARN: ${MEM_MIB} MiB exceeds the current VMM cap ${VMM_MAX_MEM_MIB} MiB (32-bit MMIO gap);" >&2
  echo "DVMM_BAKE_WARN: the guest will NOT boot above ${VMM_MAX_MEM_MIB} MiB until a VMM high-memory region lands." >&2
fi

# --- 6. assemble the per-stack initramfs ------------------------------------
echo "== assemble initramfs (build_rootfs.sh, stack mode) =="
OUT="$ALPINE_DIR/initramfs-alpine-${STACK_NAME}.cpio.gz"
LOCK_SHA="$(sha256sum "$LOCK" | awk '{print $1}')"
SEED_OVERRIDE="$SEED" \
SELFTEST_IMAGE_REF="$SELFTEST_PIN" \
STACK_NAME="$STACK_NAME" \
STACK_PROJECT="$PROJECT" \
STACK_LOCK="$LOCK" \
STACK_BINDS="$BINDS_STAGE" \
STACK_MEM="$MEM_MIB" \
OUT="$OUT" \
  "$ALPINE_DIR/build_rootfs.sh"

ART_SHA="$(sha256sum "$OUT" | awk '{print $1}')"

# --- 7. write the stack manifest (stack.lock) -------------------------------
STACK_LOCK_FILE="$HERE/stacks/${STACK_NAME}/stack.lock"
{
  echo "# deterministic-vmm Phase-2a stack manifest (generated by bake-stack.sh)."
  echo "# The reproducibility ledger for this stack: pinned image digests + the"
  echo "# compose.lock.yml hash + the built initramfs artifact hash. Re-baking the"
  echo "# same compose input reproduces the COMPARED lines below byte-for-byte"
  echo "# (squashed images are pinned via --timestamp, so their digests are stable)."
  echo "#"
  echo "# build: services (2b) are built HOST-SIDE and judged by CONTENT-IDENTITY,"
  echo "# not image ID: 'policy=build' lines carry content_id=<squashed-layer DiffID>"
  echo "# (a SOURCE_DATE_EPOCH-normalized, reproducible filesystem hash) + the pinned"
  echo "# build_base digests + the Go toolchain. Same source -> same content_id."
  echo "#"
  echo "stack     $STACK_NAME"
  echo "project   $PROJECT"
  echo "mem_mib   $MEM_MIB"
  echo "ram_estimate_mib  $EST_MIB"
  echo "compose_lock_sha256  $LOCK_SHA"
  echo "initramfs_sha256     $ART_SHA  $(basename "$OUT")"
  sort "$PROV" | sed 's/^/  /'
  echo "# --- informational (NOT compared for repeatability) ---"
  echo "# podman-version: $PODMAN_VERSION"
  echo "# baked-at: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
} > "$STACK_LOCK_FILE"

# stash the emitted lock next to the manifest for inspection / diffing.
cp "$LOCK" "$HERE/stacks/${STACK_NAME}/compose.lock.yml"

# --- 8. pack the single-file .dvmm artifact (OP-1a) -------------------------
# Fold the just-built boot artifacts + provenance into ONE self-contained,
# deterministic .dvmm via the SAME canonical encoder `dvmm run/inspect/verify`
# read (guest/pack-dvmm.sh -> `dvmm pack`). No volatile fields, so re-baking the
# same inputs yields a BYTE-IDENTICAL .dvmm.
echo "== pack .dvmm artifact =="
[ -n "$DVMM_OUT" ] || DVMM_OUT="$ALPINE_DIR/${STACK_NAME}.dvmm"
"$HERE/pack-dvmm.sh" "$STACK_NAME" -o "$DVMM_OUT"
DVMM_SHA="$(sha256sum "$DVMM_OUT" | awk '{print $1}')"
# record the artifact identity in the stack ledger (stable; comparable across bakes).
{
  echo "dvmm_sha256          $DVMM_SHA  $(basename "$DVMM_OUT")"
} >> "$STACK_LOCK_FILE"

echo
echo "== bake-stack DONE =="
echo "   initramfs: $OUT"
echo "   sha256:    $ART_SHA"
echo "   lock:      $HERE/stacks/${STACK_NAME}/compose.lock.yml (sha256 $LOCK_SHA)"
echo "   manifest:  $STACK_LOCK_FILE"
echo "   .dvmm:     $DVMM_OUT (sha256 $DVMM_SHA)  <- the single-file artifact"
