#!/usr/bin/env bash
# Build a .tdvmm from macOS.
#
# `tdvmm build` runs only on Linux: it bakes the guest inside Linux containers and
# re-execs itself under `podman unshare` (a Linux-only user-namespace primitive),
# and the crate does not even compile off-Linux. But podman on macOS already runs
# a small Linux VM (`podman machine`), which has podman, unshare, and user
# namespaces. So we run a static-musl Linux `tdvmm` INSIDE that VM.
#
# The VM mounts your Mac's home at the SAME absolute path (/Users/<you>) over
# virtiofs, so:
#   - the compose file you point at (under your Mac $HOME) is visible in the VM, and
#   - pointing tdvmm's cache root at your Mac $HOME (step 5) makes the finished
#     artifact land in your Mac's ~/.tdvmm/artifacts/ with no copy step.
# (The VM's own login $HOME is the machine user's — /var/home/core — NOT your Mac
# home, which is why step 5 sets TDVMM_CACHE_DIR explicitly.)
#
# THE BOUNDARY: macOS can BAKE a .tdvmm, but only Linux with /dev/kvm can RUN one.
# Because the build is byte-reproducible, a Mac-baked artifact is identical to a
# Linux-baked one — run `tdvmm verify` to confirm.
#
# Usage:
#   scripts/macos-build.sh <compose.yml> [extra `tdvmm build` args...]
#
# Configuration (env):
#   TDVMM_MACHINE     podman machine name (default: the active machine)
#   TDVMM_LINUX_BIN   path INSIDE the VM to a static-musl linux `tdvmm` binary.
#                    Put it somewhere under $HOME (shared into the VM), e.g.
#                    ~/.tdvmm/bin/tdvmm. Default: $HOME/.tdvmm/bin/tdvmm
#
# TODO(pin): a static linux `tdvmm` is published by .github/workflows/release.yml.
# Pin a specific release by URL + sha256 and fetch it into $HOME/.tdvmm/bin/ once,
# verifying the digest, rather than trusting whatever binary happens to be there.
# Do not hardcode a version in this script — pin it explicitly where you install.
set -euo pipefail

die() { echo "macos-build: $*" >&2; exit 1; }

# 1. macOS only — on Linux, just call `tdvmm build` directly.
[ "$(uname -s)" = "Darwin" ] || die "this wrapper is for macOS; on Linux run 'tdvmm build' directly"

# 2. A compose file is required and must be under $HOME (so the VM can see it).
COMPOSE="${1:-}"
[ -n "$COMPOSE" ] || die "usage: scripts/macos-build.sh <compose.yml> [tdvmm build args...]"
shift
[ -f "$COMPOSE" ] || die "no such compose file: $COMPOSE"
COMPOSE_ABS="$(cd "$(dirname "$COMPOSE")" && pwd)/$(basename "$COMPOSE")"
case "$COMPOSE_ABS" in
  "$HOME"/*) : ;;  # under $HOME → shared into the VM
  *) die "compose file must live under \$HOME so the podman-machine VM can see it: $COMPOSE_ABS" ;;
esac

# 3. podman + a running machine.
command -v podman >/dev/null 2>&1 || die "podman not found (install Podman Desktop or 'brew install podman')"
MACHINE="${TDVMM_MACHINE:-}"
if [ -z "$MACHINE" ]; then
  # Prefer a running machine; fall back to the default one.
  MACHINE="$(podman machine list --format '{{.Name}} {{.Running}}' 2>/dev/null | awk '$2=="true"{print $1; exit}')"
  [ -n "$MACHINE" ] || die "no running podman machine (start one: 'podman machine start')"
fi
podman machine inspect "$MACHINE" >/dev/null 2>&1 || die "podman machine not found: $MACHINE"

# 4. The linux tdvmm binary, visible inside the VM (see TODO(pin) above).
TDVMM_LINUX_BIN="${TDVMM_LINUX_BIN:-$HOME/.tdvmm/bin/tdvmm}"
podman machine ssh "$MACHINE" "test -x '$TDVMM_LINUX_BIN'" \
  || die "no executable linux tdvmm inside the VM at: $TDVMM_LINUX_BIN
        install a pinned static-musl build there (set TDVMM_LINUX_BIN to override)"

# 5. Bake inside the VM. TDVMM_CACHE_DIR is set to your Mac $HOME (expanded here,
#    Mac-side) so the artifact lands in your Mac's ~/.tdvmm/ and not the VM user's
#    home. Extra args pass through to `tdvmm build`, each shell-quoted so an arg
#    with spaces survives the single remote shell parse.
echo "macos-build: baking $COMPOSE_ABS inside podman machine '$MACHINE'..."
passthrough=""
for a in "$@"; do passthrough+=" $(printf '%q' "$a")"; done
podman machine ssh "$MACHINE" "TDVMM_CACHE_DIR='$HOME/.tdvmm' '$TDVMM_LINUX_BIN' build '$COMPOSE_ABS'$passthrough"

# 6. Where it landed (shared back to the Mac) + the reproducibility check.
STACK="$(basename "$(dirname "$COMPOSE_ABS")")"
ART="$HOME/.tdvmm/artifacts/$STACK.tdvmm"
echo "macos-build: done."
if [ -f "$ART" ]; then
  echo "  artifact: $ART"
  echo "  verify byte-identity vs a Linux bake with: tdvmm verify $STACK"
else
  echo "  (artifact under ~/.tdvmm/artifacts/ — a custom -o path lands wherever you set it)"
fi
