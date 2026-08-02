#!/usr/bin/env bash
# Pre-bake the BASE guest container store on the HOST (closed world: nothing is
# ever pulled at guest runtime).
#
# Usage: prebake_images.sh <seed-store-dir>
#   <seed-store-dir> receives a "storage/" tree (vfs graph driver) that init
#   copies into the guest's tmpfs at /var/lib/containers, plus a "guest-refs.env"
#   with the busybox reference build_rootfs.sh bakes into /etc/dvmm-image-ref.
#
# As of Phase 2a this bakes ONLY the busybox self-test image (pulled by its pinned
# digest). Workload images are no longer baked here: an arbitrary compose stack's
# images are resolved, squashed, and baked by guest/bake-stack.sh, which builds
# its own seed (busybox + the stack's images). The single-layer squash policy +
# config-equivalence gate now live in bake-stack.sh.
#
# Requires host podman + host network for the one-time pull. A clean
# CONTAINERS_CONF is used so the user's custom OCI runtime is never touched.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
SEED="${1:?usage: prebake_images.sh <seed-store-dir>}"
LOCK="$HERE/images.lock"
DRIVER="${GRAPH_DRIVER:-vfs}"

command -v podman >/dev/null || { echo "ERROR: host podman is required to pre-bake images"; exit 1; }
PODMAN_VERSION="$(podman --version | awk '{print $3}')"

WORK="$(mktemp -d)"
cleanup() { podman unshare rm -rf "$WORK" 2>/dev/null || true; rm -rf "$WORK" 2>/dev/null || true; }
trap cleanup EXIT

CLEAN_CONF="$WORK/containers.conf"
printf '[engine]\n' > "$CLEAN_CONF"
export CONTAINERS_CONF="$CLEAN_CONF"

STORE="$SEED/storage"
RUNROOT="$WORK/run"
rm -rf "$STORE"; mkdir -p "$STORE" "$RUNROOT"

# --- parse images.lock: (policy, ref) --------------------------------------
PLAIN_REFS=()
while read -r policy ref _; do
  case "$policy" in
    ''|\#*)  continue ;;
    plain)   PLAIN_REFS+=("$ref") ;;
    *) echo "ERROR: unknown policy '$policy' in images.lock (base bake supports only 'plain'; squash lives in bake-stack.sh)"; exit 1 ;;
  esac
done < "$LOCK"
[ "${#PLAIN_REFS[@]}" -ge 1 ] || { echo "ERROR: expected at least one 'plain' image (busybox)"; exit 1; }

# --- populate the seed store INSIDE the userns -----------------------------
printf '%s\n' "${PLAIN_REFS[@]}" > "$WORK/plain.txt"
export STORE RUNROOT DRIVER WORK
podman unshare bash <<'UNSHARE'
set -euo pipefail
sp() { podman --root "$STORE" --runroot "$RUNROOT" --storage-driver "$DRIVER" "$@"; }
while IFS= read -r ref; do
  [ -n "$ref" ] || continue
  echo "pulling (plain) $ref into seed"
  sp pull -q "$ref" >/dev/null
done < "$WORK/plain.txt"
echo "--- baked seed store contents ---"
sp images --digests --no-trunc
UNSHARE

# Make the store relocatable: drop libpod state (records an absolute graphroot);
# the guest recreates it fresh at /var/lib/containers/storage on first use.
rm -rf "$STORE/libpod" "$STORE/db.sql"

# --- guest refs consumed by build_rootfs.sh --------------------------------
cat > "$SEED/guest-refs.env" <<EOF
# Guest image reference (baked into /etc/dvmm-image-ref by build_rootfs.sh).
BUSYBOX_REF='${PLAIN_REFS[0]}'
EOF

# --- rewrite the GENERATED PROVENANCE block --------------------------------
BEGIN='# >>> GENERATED PROVENANCE (rewritten by prebake_images.sh on each bake) >>>'
END='# <<< GENERATED PROVENANCE END <<<'
{
  awk -v b="$BEGIN" '{print} $0==b{exit}' "$LOCK"
  echo "# podman-version: $PODMAN_VERSION"
  echo "# baked-at: $(date -u +%Y-%m-%dT%H:%M:%SZ)  (informational only; NOT a repro anchor -- see NOTES)"
  echo "#"
  for r in "${PLAIN_REFS[@]}"; do
    echo "# busybox   policy=plain   squashed=no   guest-ref=$r"
    echo "#   upstream=$r  (content identity = upstream digest)"
  done
  awk -v e="$END" '$0==e{f=1} f{print}' "$LOCK"
} > "$LOCK.new"
mv "$LOCK.new" "$LOCK"

echo "pre-baked base seed ($(du -sh "$STORE" | cut -f1)) into $STORE"
echo "  busybox: ${PLAIN_REFS[0]}"
