#!/usr/bin/env bash
# Build the Alpine container guest rootfs as a large initramfs (rootfs entirely
# in RAM) with podman + crun + conmon + netavark AND the genuine Docker Compose
# v2 CLI installed, plus a baked-in, digest-pinned image store so the guest can
# run a compose stack fully offline (closed world).
#
# Two modes:
#   * BASE (no stack): bakes only the busybox self-test image (via prebake) and
#     produces initramfs-alpine.cpio.gz -- the minimal 2a container guest used by
#     the container/busybox smoke tests and run.sh.
#   * STACK (invoked by bake-stack.sh with SEED_OVERRIDE + STACK_* env): embeds a
#     pre-built seed store (busybox + the stack's images), the emitted
#     compose.lock.yml, the materialized relative RO binds, and the pinned project
#     name, so guest init launches the stack via `docker compose up`.
#
# Reproducibility anchor: everything pinned (Alpine release + minirootfs checksum
# + package versions + compose binary sha256 + image digests + fixed guest epoch).
#
# No root required. All privileged steps run inside `podman unshare` (a user
# namespace). Device nodes are baked via the vendored gen_init_cpio.
#
# Requires: host podman (with subuid/subgid), cc, cpio, gzip, curl, and host
# network for the one-time package + image + compose-binary downloads.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
GEN_SRC="$HERE/../initramfs/gen_init_cpio.c"   # vendored kernel tool (reused)

# --- pins -------------------------------------------------------------------
ALPINE_BRANCH="v3.22"
ALPINE_VER="3.22.5"
MINIROOTFS="alpine-minirootfs-${ALPINE_VER}-x86_64.tar.gz"
MINIROOTFS_SHA256="4b4daa9fe2fc696c4919c4412a4c3d3e770d8fb70292a004a2c72f5096175282"
MIRROR="${ALPINE_MIRROR:-https://dl-cdn.alpinelinux.org/alpine}"

# Fixed guest wall clock: 2026-08-01T00:00:00Z. No RTC exists (that is Step 3),
# so every boot starts here.
BUILD_EPOCH="1785542400"

# Genuine Docker Compose v2 CLI (static Go binary), pinned + sha256-verified.
# shellcheck disable=SC1091
source "$HERE/compose-engine.lock"
COMPOSE_URL="https://github.com/docker/compose/releases/download/${COMPOSE_VERSION}/docker-compose-linux-x86_64"
COMPOSE_CACHE="$HERE/docker-compose-${COMPOSE_VERSION}"

# Top-level packages, pinned. Transitive deps float within the branch and are
# recorded in packages.lock after the build for the full version record.
PKGS=(
  "podman=5.6.2-r3"
  "crun=1.23.1-r0"
  "conmon=2.1.13-r0"
  "netavark=1.16.1-r0"
  "aardvark-dns=1.16.0-r0"
  "nftables=1.1.3-r0"
  "iptables=1.8.11-r1"
  "iproute2=6.15.0-r0"
  "ca-certificates=20260611-r0"
  "fuse-overlayfs=1.15-r0"
)

OUT="${OUT:-$HERE/initramfs-alpine.cpio.gz}"
GRAPH_DRIVER="${GRAPH_DRIVER:-vfs}"

# STACK mode inputs (set by bake-stack.sh). Empty in BASE mode.
SEED_OVERRIDE="${SEED_OVERRIDE:-}"
SELFTEST_IMAGE_REF="${SELFTEST_IMAGE_REF:-}"
STACK_NAME="${STACK_NAME:-}"
STACK_PROJECT="${STACK_PROJECT:-}"
STACK_LOCK="${STACK_LOCK:-}"
STACK_BINDS="${STACK_BINDS:-}"
STACK_MEM="${STACK_MEM:-4096}"

command -v podman >/dev/null || { echo "ERROR: host podman is required"; exit 1; }
command -v cpio   >/dev/null || { echo "ERROR: cpio is required"; exit 1; }
command -v curl   >/dev/null || { echo "ERROR: curl is required"; exit 1; }

WORK="$(mktemp -d)"
cleanup() { podman unshare rm -rf "$WORK" 2>/dev/null || true; rm -rf "$WORK" 2>/dev/null || true; }
trap cleanup EXIT

# Clean containers.conf so we never touch the user's custom host OCI runtime.
CLEAN_CONF="$WORK/containers.conf"
printf '[engine]\n' > "$CLEAN_CONF"
export CONTAINERS_CONF="$CLEAN_CONF"

# --- gen_init_cpio (device-node baker, no root needed) ----------------------
GEN="$HERE/../initramfs/gen_init_cpio"
if [ ! -x "$GEN" ]; then
  echo "building gen_init_cpio..."
  cc -O2 -o "$GEN" "$GEN_SRC"
fi

# --- pinned Alpine minirootfs ----------------------------------------------
TARBALL="$HERE/$MINIROOTFS"
if [ ! -f "$TARBALL" ]; then
  echo "downloading $MINIROOTFS ..."
  curl -sSL -o "$TARBALL" "$MIRROR/$ALPINE_BRANCH/releases/x86_64/$MINIROOTFS"
fi
echo "${MINIROOTFS_SHA256}  ${TARBALL}" | sha256sum -c -

# --- pinned Docker Compose v2 binary (fetch + verify) -----------------------
if [ ! -f "$COMPOSE_CACHE" ]; then
  echo "downloading docker compose $COMPOSE_VERSION ..."
  curl -sSL -o "$COMPOSE_CACHE" "$COMPOSE_URL"
fi
echo "${COMPOSE_SHA256}  ${COMPOSE_CACHE}" | sha256sum -c -

# --- build the TEST-1a control-channel agent (static, reproducible) ----------
# A tiny stdlib-only Go binary baked into EVERY guest. Built host-side with the
# same reproducible flags as the Go A/B service (CGO off, -trimpath, no VCS/build
# id), so its bytes — and thus the initramfs + .dvmm — are identical bake-to-bake.
command -v go >/dev/null || { echo "ERROR: host 'go' is required to build dvmm-agent"; exit 1; }
AGENT_SRC="$HERE/../agent"
AGENT_BIN="$WORK/dvmm-agent"
echo "building dvmm-agent (static, reproducible) ..."
( cd "$AGENT_SRC" && CGO_ENABLED=0 GOTOOLCHAIN=local GOFLAGS=-trimpath \
    go build -trimpath -buildvcs=false -ldflags="-s -w -buildid=" -o "$AGENT_BIN" . )

# --- seed store: BASE bakes busybox via prebake; STACK reuses bake-stack's ---
if [ -n "$SEED_OVERRIDE" ]; then
  SEED="$SEED_OVERRIDE"
  [ -d "$SEED/storage" ] || { echo "ERROR: SEED_OVERRIDE has no storage/: $SEED"; exit 1; }
  [ -n "$SELFTEST_IMAGE_REF" ] || { echo "ERROR: STACK mode needs SELFTEST_IMAGE_REF"; exit 1; }
  echo "using provided seed store: $SEED (selftest image: $SELFTEST_IMAGE_REF)"
else
  SEED="$WORK/seed"
  GRAPH_DRIVER="$GRAPH_DRIVER" "$HERE/prebake_images.sh" "$SEED"
  # shellcheck disable=SC1091
  source "$SEED/guest-refs.env"
  SELFTEST_IMAGE_REF="$BUSYBOX_REF"
fi

# --- build the rootfs + pack the initramfs, all as root-in-userns -----------
export WORK TARBALL MIRROR ALPINE_BRANCH BUILD_EPOCH GEN OUT GRAPH_DRIVER \
       SEED HERE SELFTEST_IMAGE_REF COMPOSE_CACHE AGENT_BIN \
       STACK_NAME STACK_PROJECT STACK_LOCK STACK_BINDS STACK_MEM
printf '%s\n' "${PKGS[@]}" > "$WORK/pkgs.txt"

podman unshare bash <<'UNSHARE'
set -euo pipefail
ROOTFS="$WORK/rootfs"
mkdir -p "$ROOTFS"

# 1. extract the pinned minirootfs
tar -C "$ROOTFS" -xzf "$TARBALL"

# 2. apk config: pinned branch + a resolver for the one-time install
cat > "$ROOTFS/etc/apk/repositories" <<EOF
$MIRROR/$ALPINE_BRANCH/main
$MIRROR/$ALPINE_BRANCH/community
EOF
printf 'nameserver 1.1.1.1\nnameserver 8.8.8.8\n' > "$ROOTFS/etc/resolv.conf"

# 3. install the pinned container stack (chroot works: we hold CAP_SYS_CHROOT)
mapfile -t PKGS < "$WORK/pkgs.txt"
chroot "$ROOTFS" /sbin/apk update
chroot "$ROOTFS" /sbin/apk add --no-progress "${PKGS[@]}"

# record the FULL resolved version set (top-level + deps)
chroot "$ROOTFS" /sbin/apk list -I 2>/dev/null | awk '{print $1}' | sort > "$WORK/packages.lock"

# 4. drop the overlay (init, self-test, compose launcher, podman config)
cp -a "$HERE/overlay/." "$ROOTFS/"
chmod 0755 "$ROOTFS/init" \
           "$ROOTFS/usr/local/bin/container-selftest.sh" \
           "$ROOTFS/usr/local/bin/compose-up.sh" \
           "$ROOTFS/usr/local/bin/healthcheck-ticker.sh"

# 4b. bake the genuine Docker Compose v2 CLI. Invoked directly as `docker-compose`
#     (the v2 binary works standalone), driven against podman's Docker-compat API.
install -D -m 0755 "$COMPOSE_CACHE" "$ROOTFS/usr/local/bin/docker-compose"

# 4c. bake the TEST-1a control-channel agent (blocks on ttyS1; FF-transparent).
install -D -m 0755 "$AGENT_BIN" "$ROOTFS/usr/local/bin/dvmm-agent"

# 5. bake the fixed clock epoch + the self-test image reference
printf '%s\n' "$BUILD_EPOCH"          > "$ROOTFS/etc/dvmm-build-epoch"
printf '%s\n' "$SELFTEST_IMAGE_REF"   > "$ROOTFS/etc/dvmm-image-ref"   # 2a selftest

# 5b. STACK mode: embed compose.lock.yml + materialized binds + pinned project
if [ -n "$STACK_LOCK" ]; then
  mkdir -p "$ROOTFS/var/lib/dvmm-stack/binds"
  cp "$STACK_LOCK" "$ROOTFS/var/lib/dvmm-stack/compose.lock.yml"
  chmod 0644 "$ROOTFS/var/lib/dvmm-stack/compose.lock.yml"
  if [ -n "$STACK_BINDS" ] && [ -d "$STACK_BINDS" ]; then
    cp -a "$STACK_BINDS/." "$ROOTFS/var/lib/dvmm-stack/binds/" 2>/dev/null || true
  fi
  printf '%s\n' "$STACK_NAME"    > "$ROOTFS/etc/dvmm-stack-name"
  printf '%s\n' "$STACK_PROJECT" > "$ROOTFS/etc/dvmm-stack-project"
  printf '%s\n' "$STACK_MEM"     > "$ROOTFS/etc/dvmm-stack-mem"
fi

# 6. seed store: the pre-baked image graph the guest copies into its tmpfs.
mkdir -p "$ROOTFS/var/lib/containers-seed"
cp -a "$SEED/storage" "$ROOTFS/var/lib/containers-seed/storage"

# 7. trim install-time cruft that would only bloat RAM
rm -rf "$ROOTFS/var/cache/apk/"* "$ROOTFS/etc/resolv.conf"
rm -rf "$ROOTFS/root/.config/containers" 2>/dev/null || true
: > "$ROOTFS/etc/resolv.conf"   # empty file present as a mount/overwrite target

# 7a0. Normalize containers/storage "created" timestamps. c/storage records a
# nanosecond bake-time in each layer/image record (layers.json, images.json), the
# deepest seed non-determinism (variable-length -> shifts the whole cpio). Pin
# them to the fixed guest epoch; the DIGESTS (the content identity gated by
# bake_repeat_test.sh) are untouched, and podman only uses `created` for display.
find "$ROOTFS/var/lib/containers-seed" -name '*.json' -type f -exec \
  sed -i -E 's/"created":"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:.]+Z"/"created":"2026-08-01T00:00:00Z"/g' {} + 2>/dev/null || true

# 7a. Zero containers/storage lock files: c/storage writes a random per-writer
# "last-write" token into *.lock, the last CONTENT-level source of seed
# non-determinism. The guest re-initializes them on first use, so emptying them is
# safe (and the seed is read-only baked state anyway).
find "$ROOTFS" -name '*.lock' -type f -exec truncate -s 0 {} + 2>/dev/null || true

# 7b. Normalize ALL mtimes to the fixed build epoch. `cpio --reproducible`
# renumbers inodes + zeroes devno but does NOT touch mtime, and directories +
# freshly-installed files (apk, install, the built agent) otherwise carry the
# current wall time — the last remaining source of initramfs non-determinism.
# -h so symlinks' own mtimes are set too. This is what makes two bakes byte-
# identical (the .dvmm bit-reproducibility gate).
find "$ROOTFS" -exec touch -h -d "@$BUILD_EPOCH" {} + 2>/dev/null || true

# 8. pack: a device-node cpio segment (gen_init_cpio) + the bulk tree
cat > "$WORK/nodes.manifest" <<EOF
dir /dev 0755 0 0
nod /dev/console 0600 0 0 c 5 1
nod /dev/null 0666 0 0 c 1 3
nod /dev/ttyS0 0660 0 0 c 4 64
nod /dev/ttyS1 0660 0 0 c 4 65
EOF
# -t <epoch>: stamp the device-node mtimes with the FIXED build epoch (not the
# current time), so this cpio segment is byte-identical bake-to-bake — required
# for a bit-reproducible initramfs / .dvmm.
"$GEN" -t "$BUILD_EPOCH" "$WORK/nodes.manifest" > "$WORK/nodes.cpio"

CPIO_REPRO=""
if cpio --usage 2>&1 | grep -q -- --reproducible; then CPIO_REPRO="--reproducible"; fi
( cd "$ROOTFS" && \
  find . -mindepth 1 \( -path './dev' -o -path './dev/*' \) -prune -o -print0 \
    | LC_ALL=C sort -z \
    | cpio --null --create --format=newc --owner=0:0 --quiet $CPIO_REPRO ) > "$WORK/rootfs.cpio"

# Zero every entry's c_ino (there are no hardlinks, so it is cosmetic) — cpio's
# --reproducible inode renumbering is not stable for a few store files. This is
# the final step that makes the initramfs byte-identical across bakes.
cat "$WORK/nodes.cpio" "$WORK/rootfs.cpio" > "$WORK/combined.cpio"
python3 "$HERE/zero_cpio_inodes.py" "$WORK/combined.cpio"

# -n: do not store the original name or a timestamp in the gzip header (the input
# is a pipe, but -n makes the deterministic header explicit).
gzip -9 -n < "$WORK/combined.cpio" > "$OUT"
UNSHARE

cp "$WORK/packages.lock" "$HERE/packages.lock"

# Determinism-phase anchor: the BUILT INITRAMFS ARTIFACT is the repro unit.
ART_SHA="$(sha256sum "$OUT" | awk '{print $1}')"
printf '%s  %s\n' "$ART_SHA" "$(basename "$OUT")" > "$OUT.sha256"

echo "wrote $OUT ($(stat -c%s "$OUT") bytes)"
echo "initramfs artifact sha256: $ART_SHA"
echo "selftest image: $SELFTEST_IMAGE_REF"
[ -n "$STACK_LOCK" ] && echo "stack: $STACK_NAME (project $STACK_PROJECT, mem ${STACK_MEM}MiB)"
echo "compose engine: docker-compose $COMPOSE_VERSION"
echo "packages.lock updated ($(wc -l < "$HERE/packages.lock") packages)"
