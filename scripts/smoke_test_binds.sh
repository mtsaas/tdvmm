#!/usr/bin/env bash
# Phase-2b (item 4) RW-BIND + NAMED-VOLUME acceptance test.
#
# Boots the rwbind stack under fast-forward and proves:
#
#   (a) the relative RW bind was MATERIALIZED into the guest image: the baked
#       ./data/seed.txt content is visible in the container (DVMM_RW_SEED),
#   (b) a WRITE to the rw bind lands and is readable within the run
#       (DVMM_RWBIND_OK = the value the container just wrote + read back),
#   (c) a WRITE to the NAMED VOLUME lands and is readable within the run
#       (DVMM_VOLUME_OK), and
#   (d) it boots + runs under fast-forward (FF summary: speedup + the per-hop
#       <=500us VMM gate).
#
# Exits 0 only if every property holds.
#
# Usage: scripts/smoke_test_binds.sh [wall_timeout_s]
# Env:   MEM (3072)  MAX_VIRTUAL_TIME (7200s)
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
BIN="$ROOT/target/release/dvmm"
KERNEL="$ROOT/guest/kernel/vmlinux-6.1.128"
INITRD="${INITRD:-$ROOT/guest/initramfs-alpine/initramfs-alpine-rwbind.cpio.gz}"

WALL_TIMEOUT="${1:-150}"
MEM="${MEM:-3072}"
# Virtual-time horizon (the stack loops forever): 2 virtual hours -> clean FF stop
# + summary, in seconds of real time.
MAX_VIRTUAL_TIME="${MAX_VIRTUAL_TIME:-7200s}"
SEED_EXPECT="baked-seed-content"

[ -x "$BIN" ] || { echo "[rw] building..."; ( cd "$ROOT" && cargo build --release ) || exit 3; }
[ -f "$KERNEL" ] && [ -f "$INITRD" ] || { echo "SMOKE FAIL: missing artifacts (bake rwbind?)"; exit 3; }

LOG="$(mktemp)"; trap 'rm -f "$LOG"' EXIT
CMDLINE="console=ttyS0 reboot=t panic=1 pci=off no_timer_check tsc=reliable dvmm.stack=1"
echo "[rw] boot: mem=${MEM}MiB ff=ON horizon=${MAX_VIRTUAL_TIME} wall_timeout=${WALL_TIMEOUT}s"
START=$(date +%s.%N)
timeout "$WALL_TIMEOUT" "$BIN" boot --kernel "$KERNEL" --initrd "$INITRD" --mem "$MEM" --ff on \
  --max-virtual-time "$MAX_VIRTUAL_TIME" --cmdline "$CMDLINE" </dev/null >"$LOG" 2>&1
rc=$?
WALL=$(awk "BEGIN{printf \"%.1f\", $(date +%s.%N)-$START}")
echo "[rw] vmm exit=$rc (3=horizon, expected) wall=${WALL}s"

# Container-emitted lines only (drop the serial dump of the lockfile, whose
# "[stack][lock] ... DVMM_RW_SEED=$$(cat ..." echoes would otherwise match).
# Strip CR: the serial console emits CRLF, which would break exact-match compares.
emitted() { grep -E "$1" "$LOG" | grep -v '\[stack\]\[lock\]' | tr -d '\r'; }

echo
echo "==== write proof (materialized seed + rw-bind write + named-volume write) ===="
emitted 'DVMM_RW_SEED=|DVMM_RWBIND_OK=|DVMM_VOLUME_OK=|DVMM_RW_HEARTBEAT' | head -6 | sed 's/^/  /'
echo "==== fast-forward summary ===="
grep -E 'FAST-FORWARD SUMMARY' "$LOG" | sed 's/^/  /'
echo "==========================================================================="

ok=1
note() { echo "  $*"; }

# (a) materialization: the baked seed content is visible in the container.
seed="$(emitted 'DVMM_RW_SEED=' | grep -oE 'DVMM_RW_SEED=[^ ]*' | head -1 | cut -d= -f2-)"
if [ "$seed" = "$SEED_EXPECT" ]; then
  note "materialized rw bind: DVMM_RW_SEED='$seed' (== baked ./data/seed.txt) OK"
else
  echo "ASSERT FAIL: baked seed not visible (got '$seed', want '$SEED_EXPECT')"; ok=0
fi

# (b) rw bind write landed + read back.
rwok="$(emitted 'DVMM_RWBIND_OK=' | grep -oE 'DVMM_RWBIND_OK=[^ ]*' | head -1 | cut -d= -f2-)"
if echo "$rwok" | grep -qE '^run-[0-9]+$'; then
  note "rw bind write: DVMM_RWBIND_OK='$rwok' (written then read back) OK"
else
  echo "ASSERT FAIL: rw bind write not visible (got '$rwok')"; ok=0
fi

# (c) named volume write landed + read back.
volok="$(emitted 'DVMM_VOLUME_OK=' | grep -oE 'DVMM_VOLUME_OK=[^ ]*' | head -1 | cut -d= -f2-)"
if [ "$volok" = "hello-volume" ]; then
  note "named volume write: DVMM_VOLUME_OK='$volok' (written then read back) OK"
else
  echo "ASSERT FAIL: named volume write not visible (got '$volok')"; ok=0
fi

# (d) ran under fast-forward + per-hop <=500us VMM gate.
summary=$(grep -E 'FAST-FORWARD SUMMARY' "$LOG" | tail -1)
if [ -n "$summary" ]; then
  speed=$(echo "$summary" | grep -oE '= [0-9.]+x speedup' | grep -oE '[0-9.]+' | head -1)
  hopmean=$(echo "$summary" | grep -oE 'mean [0-9.]+us' | grep -oE '[0-9.]+' | head -1)
  note "fast-forward: ${speed}x speedup; per-hop mean ${hopmean}us (<=500 HARD)"
  awk "BEGIN{exit !($hopmean <= 500)}" || { echo "ASSERT FAIL: per-hop mean > 500us"; ok=0; }
else
  echo "ASSERT FAIL: no fast-forward summary (did it run under FF / reach the horizon?)"; ok=0
fi

echo
if [ "$ok" -eq 1 ]; then
  echo "BINDS SMOKE PASS: rw bind materialized + writable, named volume writable, ran under FF (${speed}x)."
  exit 0
fi
echo "BINDS SMOKE FAIL (see above)."
echo "---- last serial output ----"; tail -30 "$LOG"
exit 1
