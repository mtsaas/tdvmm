#!/usr/bin/env bash
# Container smoke test: bake the minimal `spinner` stack, boot its artifact, and
# require that the guest creates a podman bridge network and runs a digest-pinned
# image over serial, fully offline. Exits 0 on success, non-zero otherwise.
#
# The guest's /init runs the container self-test automatically and prints markers;
# `tdvmm.autotest=1` powers off cleanly when done, and omitting `tdvmm.stack=1`
# means the guest runs the self-test rather than the spinner compose. This script
# just watches the serial log for the pass/fail markers.
#
# The base container guest (the retired build_rootfs.sh) is gone: every baked
# stack ships the same container self-test, so we bake+run the smallest one.
#
# Usage: scripts/smoke_test_container.sh [timeout_seconds]
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"

TIMEOUT="${1:-120}"
# Virtual-time horizon (safety net): the guest boots, runs its self-test, then
# `reboot=t`-shuts-down (autotest) well within this budget. Bounds a wedge.
MAX_VIRTUAL_TIME="${MAX_VIRTUAL_TIME:-3600s}"
BIN="$ROOT/target/release/tdvmm"
STACK="spinner"

PASS="TDVMM_SELFTEST_PASS"
FAILMARK="TDVMM_SELFTEST_FAIL"
NETOK="TDVMM_NET_CREATE_OK"
RUNOK="TDVMM_PODMAN_RUN_OK"
HELLO="TDVMM_CONTAINER_HELLO"

if [ ! -x "$BIN" ]; then
  echo "building release binary..."
  ( cd "$ROOT" && cargo build --release ) || { echo "SMOKE FAIL: build error"; exit 3; }
fi

# Bake the (minimal) spinner stack into ~/.tdvmm; a warm cache makes this a
# near-instant restore. Writes spinner's committed locks (hence needs-bake).
"$BIN" build "$STACK" "$ROOT/testdata/stacks/$STACK/compose.yml" || { echo "SMOKE FAIL: bake error"; exit 3; }

LOG="$(mktemp)"
cleanup() { kill "$PID" 2>/dev/null; wait "$PID" 2>/dev/null; rm -f "$LOG"; }
trap cleanup EXIT

# Run detached, no interactive input; capture serial output.
CMDLINE="console=ttyS0 reboot=t panic=1 pci=off no_timer_check tsc=reliable tdvmm.autotest=1"
"$BIN" run "$STACK" --max-virtual-time "$MAX_VIRTUAL_TIME" --cmdline "$CMDLINE" \
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
