#!/usr/bin/env bash
# Smoke test: boot the VM and require a known serial marker within N seconds.
# Exits 0 on success, non-zero otherwise.
#
# Usage: scripts/smoke_test.sh [timeout_seconds] [kernel] [initrd]
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"

TIMEOUT="${1:-30}"
KERNEL="${2:-$ROOT/guest/kernel/vmlinux-6.1.128}"
INITRD="${3:-$ROOT/guest/initramfs/initramfs.cpio.gz}"
MARKER="DVMM_BOOT_OK"
BIN="$ROOT/target/release/dvmm"

[ -f "$KERNEL" ] || { echo "SMOKE FAIL: kernel not found: $KERNEL"; exit 3; }
[ -f "$INITRD" ] || { echo "SMOKE FAIL: initrd not found: $INITRD"; exit 3; }

if [ ! -x "$BIN" ]; then
  echo "building release binary..."
  ( cd "$ROOT" && cargo build --release ) || { echo "SMOKE FAIL: build error"; exit 3; }
fi

LOG="$(mktemp)"
cleanup() { kill "$PID" 2>/dev/null; wait "$PID" 2>/dev/null; rm -f "$LOG"; }
trap cleanup EXIT

# Run detached with no interactive input; capture serial output.
"$BIN" boot --kernel "$KERNEL" --initrd "$INITRD" --mem 256 </dev/null >"$LOG" 2>&1 &
PID=$!

deadline=$(( $(date +%s) + TIMEOUT ))
found=0
while [ "$(date +%s)" -lt "$deadline" ]; do
  if grep -q "$MARKER" "$LOG"; then found=1; break; fi
  kill -0 "$PID" 2>/dev/null || break   # VMM exited early
  sleep 0.1
done

if [ "$found" -eq 1 ]; then
  echo "SMOKE PASS: serial marker '$MARKER' seen within ${TIMEOUT}s"
  grep -m1 "$MARKER" "$LOG"
  exit 0
else
  echo "SMOKE FAIL: marker '$MARKER' not seen within ${TIMEOUT}s"
  echo "---- last serial output ----"
  tail -20 "$LOG"
  exit 1
fi
