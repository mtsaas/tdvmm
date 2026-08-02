#!/usr/bin/env bash
# Container smoke test: boot the Alpine guest and require that it creates a
# podman bridge network and runs a digest-pinned image over serial, fully
# offline. Exits 0 on success, non-zero otherwise.
#
# The guest's /init runs the self-test automatically and prints markers; with
# `dvmm.autotest=1` on the cmdline it powers off cleanly when done, so this
# script just watches the serial log for the pass/fail markers.
#
# Usage: scripts/smoke_test_container.sh [timeout_seconds] [kernel] [initrd]
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"

TIMEOUT="${1:-120}"
KERNEL="${2:-$ROOT/guest/kernel/vmlinux-6.1.128}"
INITRD="${3:-$ROOT/guest/initramfs-alpine/initramfs-alpine.cpio.gz}"
MEM="${MEM:-2048}"
# Virtual-time horizon (safety net): the guest boots, runs its self-test, then
# `reboot=t`-shuts-down (autotest) well within this budget. Bounds a wedge.
MAX_VIRTUAL_TIME="${MAX_VIRTUAL_TIME:-3600s}"
BIN="$ROOT/target/release/dvmm"

PASS="DVMM_SELFTEST_PASS"
FAILMARK="DVMM_SELFTEST_FAIL"
NETOK="DVMM_NET_CREATE_OK"
RUNOK="DVMM_PODMAN_RUN_OK"
HELLO="DVMM_CONTAINER_HELLO"

[ -f "$KERNEL" ] || { echo "SMOKE FAIL: kernel not found: $KERNEL"; exit 3; }
[ -f "$INITRD" ] || { echo "SMOKE FAIL: initrd not found: $INITRD"; exit 3; }

if [ ! -x "$BIN" ]; then
  echo "building release binary..."
  ( cd "$ROOT" && cargo build --release ) || { echo "SMOKE FAIL: build error"; exit 3; }
fi

LOG="$(mktemp)"
cleanup() { kill "$PID" 2>/dev/null; wait "$PID" 2>/dev/null; rm -f "$LOG"; }
trap cleanup EXIT

# Run detached, no interactive input; capture serial output. dvmm.autotest=1
# makes the guest power off after the self-test.
CMDLINE="console=ttyS0 reboot=t panic=1 pci=off no_timer_check tsc=reliable dvmm.autotest=1"
"$BIN" boot --kernel "$KERNEL" --initrd "$INITRD" --mem "$MEM" \
  --max-virtual-time "$MAX_VIRTUAL_TIME" --cmdline "$CMDLINE" \
  </dev/null >"$LOG" 2>&1 &
PID=$!

deadline=$(( $(date +%s) + TIMEOUT ))
result=""
while [ "$(date +%s)" -lt "$deadline" ]; do
  if grep -q "$PASS" "$LOG"; then result="pass"; break; fi
  if grep -q "$FAILMARK" "$LOG"; then result="fail"; break; fi
  kill -0 "$PID" 2>/dev/null || { sleep 0.3; break; }   # VMM exited; final grep below
  sleep 0.2
done

# Final check in case the marker landed as the VMM was exiting.
[ -z "$result" ] && grep -q "$PASS" "$LOG" && result="pass"
[ -z "$result" ] && grep -q "$FAILMARK" "$LOG" && result="fail"

if [ "$result" = "pass" ]; then
  echo "SMOKE PASS: guest ran podman on a bridge network offline."
  for m in "$NETOK" "$HELLO" "$RUNOK" "$PASS"; do grep -m1 "$m" "$LOG"; done
  exit 0
else
  echo "SMOKE FAIL: '$PASS' not seen within ${TIMEOUT}s (result='${result:-timeout}')"
  echo "---- last serial output ----"
  tail -40 "$LOG"
  exit 1
fi
