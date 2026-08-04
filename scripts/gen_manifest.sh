#!/usr/bin/env bash
# Generate (or verify) the artifact manifest.
#
# The manifest records the Phase-1 reproducibility anchors together in one place:
#   - the boot kernel:     vmlinux sha256
#   - the effective guest CPUID profile the VMM presents (from `tdvmm dump-cpuid`)
#
# (The per-stack initramfs/artifact hashes live in each stack's stack.lock, gated
# by bake-repeat/artifact-test; there is no longer a standalone base initramfs.)
#
# Why the CPUID belongs here: the guest's declared LAPIC-timer / TSC frequency now
# hangs off the passed-through CPUID leaf 0x15 crystal (Step 3a/3b/4). Recording the
# exact leaves the guest sees means a host/CPU change surfaces as a manifest
# deviation, not a silent timing difference.
#
# Usage:
#   scripts/gen_manifest.sh           # write guest/manifest.txt
#   scripts/gen_manifest.sh --check   # regenerate + diff against the committed
#                                     # manifest; exit nonzero on ANY deviation.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"

VMLINUX="$ROOT/guest/kernel/vmlinux-6.1.128"
BIN="$ROOT/target/release/tdvmm"
MANIFEST="$ROOT/guest/manifest.txt"
COMPOSE_LOCK="$ROOT/guest/initramfs-alpine/compose-engine.lock"

CHECK=0
[ "${1:-}" = "--check" ] && CHECK=1

[ -f "$VMLINUX" ] || { echo "gen_manifest: kernel not found: $VMLINUX" >&2; exit 3; }
if [ ! -x "$BIN" ]; then
  echo "gen_manifest: building release binary..." >&2
  ( cd "$ROOT" && cargo build --release ) >&2
fi

sha() { sha256sum "$1" | cut -d' ' -f1; }

VM_SHA="$(sha "$VMLINUX")"
CPUID="$("$BIN" dump-cpuid 2>/dev/null)"
CPUID_SHA="$(printf '%s\n' "$CPUID" | sha256sum | cut -d' ' -f1)"

# Compose engine (owed from 2a): the pinned Docker Compose v2 CLI version + the
# sha256 of the static binary the guest bakes in (source: compose-engine.lock).
COMPOSE_VERSION=""; COMPOSE_SHA256=""
# shellcheck disable=SC1090
[ -f "$COMPOSE_LOCK" ] && source "$COMPOSE_LOCK"

GEN="$(cat <<EOF
# tdvmm artifact manifest (Phase 1)
#
# The unit of run-to-run reproducibility: the boot kernel (vmlinux) AND the
# effective guest CPUID profile the VMM presents. The declared LAPIC-timer
# frequency hangs off the passed-through CPUID 0x15 crystal, so a host/CPU change
# shows up here as a changed CPUID block instead of a silent timing difference.
#
# Regenerate:  scripts/gen_manifest.sh
# Verify:      scripts/gen_manifest.sh --check   (nonzero exit on any deviation)

vmlinux    sha256=$VM_SHA  guest/kernel/vmlinux-6.1.128
cpuid      sha256=$CPUID_SHA  (effective guest CPUID, userspace backend; block below)
compose    version=$COMPOSE_VERSION  sha256=$COMPOSE_SHA256  (Docker Compose v2 CLI baked into the guest; source guest/initramfs-alpine/compose-engine.lock)

# ===== effective guest CPUID (userspace backend; tdvmm dump-cpuid) =====
$CPUID
EOF
)"

if [ "$CHECK" -eq 1 ]; then
  [ -f "$MANIFEST" ] || { echo "MANIFEST CHECK FAIL: $MANIFEST does not exist (run gen_manifest.sh)"; exit 1; }
  if diff -u "$MANIFEST" <(printf '%s\n' "$GEN"); then
    echo "MANIFEST CHECK OK: live artifacts + effective CPUID match $MANIFEST"
    exit 0
  else
    echo "MANIFEST CHECK FAIL: live profile deviates from the committed manifest (see diff above)"
    exit 1
  fi
else
  printf '%s\n' "$GEN" > "$MANIFEST"
  echo "wrote $MANIFEST"
  echo "  vmlinux   sha256=$VM_SHA"
  echo "  cpuid     sha256=$CPUID_SHA"
fi
