#!/usr/bin/env bash
# Step 4 fast-forward acceptance demo (the headline).
#
# Boots the 2b Postgres workload with fast-forward ON and shows the guest living
# through many VIRTUAL hours in seconds-to-minutes of WALL time: the service
# inserts a row every INTERVAL_SECONDS of *guest* time, and FF collapses the idle
# sleeps by jumping the TSC offset. Asserts the Step-4 acceptance gates:
#
#   - guest row timestamps are INTERVAL_SECONDS apart (tolerance +0/+2s, since a
#     row's active insert/trim work runs at wall rate),
#   - >= TARGET_HOURS virtual hours elapse,
#   - per-hop real cost mean <= 500us       (HARD gate -- a VMM property),
#   - speedup (virtual-s/real-s) is REPORTED (a stack property, NOT gated),
#   - largest single jump Δ <= the sanity bound (never trips here),
#   - guest log free of clocksource/TSC-unstable/hrtimer warnings,
#   - row count never exceeds MAX_ROWS (cap/trim holds).
#
# Runs the DOGFOOD stack through the generic 2a compose path (bake-stack ->
# compose.lock.yml -> guest -> docker compose up), not the retired workload.sh.
#
# Prints a canonical `DVMM_DEMO_SEQ=<counts>` line so a caller can check
# repeatability (gate 6). Exits 0 only if every assertion holds.
#
# Usage: scripts/ff_demo.sh [target_virtual_hours] [wall_timeout_s]
# Env:   INTERVAL (3600)  MAX_ROWS (1000)  MEM (3072)  MAX_JUMP_SECS (300)
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
BIN="$ROOT/target/release/dvmm"
KERNEL="$ROOT/guest/kernel/vmlinux-6.1.128"
INITRD="${INITRD:-$ROOT/guest/initramfs-alpine/initramfs-alpine-dogfood.cpio.gz}"

TARGET_HOURS="${1:-24}"
WALL_TIMEOUT="${2:-300}"
INTERVAL="${INTERVAL:-3600}"
MAX_ROWS="${MAX_ROWS:-1000}"
MEM="${MEM:-3072}"
MAX_JUMP_SECS="${MAX_JUMP_SECS:-300}"
# Virtual-time horizon: bound the run's virtual duration in the VMM itself, so a
# wedged/idle guest can't fast-forward forever. Set generously above this run's
# known virtual budget (TARGET_HOURS + margin) so a healthy run is never cut off,
# but a wedge is stopped in seconds of wall time with the horizon diagnostic dump.
MAX_VIRTUAL_TIME="${MAX_VIRTUAL_TIME:-$(( (TARGET_HOURS + 12) * 3600 ))s}"

[ -x "$BIN" ] || { echo "ff_demo: building..."; ( cd "$ROOT" && cargo build --release ) || exit 3; }
[ -f "$KERNEL" ] && [ -f "$INITRD" ] || { echo "ff_demo: missing artifacts"; exit 3; }

LOG="$(mktemp)"; PID=""
cleanup() { [ -n "$PID" ] && kill "$PID" 2>/dev/null; [ -n "$PID" ] && { sleep 1; kill -9 "$PID" 2>/dev/null; }; rm -f "$LOG"; }
trap cleanup EXIT

CMDLINE="console=ttyS0 reboot=t panic=1 pci=off no_timer_check tsc=reliable dvmm.stack=1 dvmm.interval=$INTERVAL dvmm.maxrows=$MAX_ROWS"
echo "[ff_demo] boot: interval=${INTERVAL}s max_rows=$MAX_ROWS target=${TARGET_HOURS} virtual-hours mem=${MEM}MiB ff=ON max-virtual-time=${MAX_VIRTUAL_TIME}"
START_WALL=$(date +%s.%N)
"$BIN" --kernel "$KERNEL" --initrd "$INITRD" --mem "$MEM" --ff on --max-jump-secs "$MAX_JUMP_SECS" \
  --max-virtual-time "$MAX_VIRTUAL_TIME" \
  --cmdline "$CMDLINE" </dev/null >"$LOG" 2>&1 &
PID=$!

# Need TARGET_HOURS+1 rows so TARGET_HOURS full intervals have elapsed.
NEED=$(( TARGET_HOURS + 1 ))
deadline=$(( $(date +%s) + WALL_TIMEOUT )); result=""; reason=""
while [ "$(date +%s)" -lt "$deadline" ]; do
  if grep -qE 'exceeds the sanity bound|panicked|DVMM_WORKLOAD_FAIL|DVMM_SVC_FAIL|dvmm: fatal' "$LOG"; then
    result="fail"; reason="vmm/guest error"; break
  fi
  n=$(grep -c 'DVMM_ROWCOUNT=' "$LOG" 2>/dev/null); n=${n:-0}
  [ "$n" -ge "$NEED" ] && { result="pass"; break; }
  kill -0 "$PID" 2>/dev/null || { result="fail"; reason="vmm exited early"; break; }
  sleep 2
done
END_WALL=$(date +%s.%N)
[ -z "$result" ] && { result="fail"; reason="timeout after ${WALL_TIMEOUT}s (only $(grep -c 'DVMM_ROWCOUNT=' "$LOG") rows)"; }

WALL=$(awk "BEGIN{printf \"%.1f\", $END_WALL-$START_WALL}")

# ---- gather evidence -------------------------------------------------------
mapfile -t C  < <(grep -oE 'DVMM_ROWCOUNT=[0-9]+' "$LOG" | cut -d= -f2)
mapfile -t TS < <(grep -oE 'DVMM_ROWCOUNT=[0-9]+ iter=[0-9]+ max=[0-9]+ ts=[0-9T:-]+Z' "$LOG" | sed -E 's/.*ts=([0-9T:-]+)Z/\1/')
METRIC="$(grep -E '\[dvmm\] fast-forward:' "$LOG" | tail -1)"

echo
echo "==== DVMM_ROWCOUNT sequence (${#C[@]} rows; wall ${WALL}s) ===="
grep -E 'DVMM_ROWCOUNT=' "$LOG" | tail -30 | sed 's/^/  /'
echo "==== last fast-forward metric line ===="
echo "  $METRIC"
echo "======================================="
echo "DVMM_DEMO_SEQ=$(IFS=,; echo "${C[*]}")"

if [ "$result" != "pass" ]; then
  echo "FF DEMO FAIL: $reason"
  tail -25 "$LOG"
  exit 1
fi

ok=1
note() { echo "  $*"; }

# (a) cap invariant + non-decreasing + started low.
mx=0; prev=-1; nondec=1; started_low=0; over=0
for v in "${C[@]}"; do
  [ "$v" -gt "$MAX_ROWS" ] && over=1
  [ "$v" -gt "$mx" ] && mx="$v"
  [ "$v" -lt "$prev" ] && nondec=0
  [ "$prev" -eq -1 ] && [ "$v" -le 2 ] && started_low=1
  prev="$v"
done
note "rows: count max=$mx cap=$MAX_ROWS started_low=$started_low non_decreasing=$nondec over_cap=$over"
[ "$over" -eq 0 ]      || { echo "ASSERT FAIL: row count exceeded MAX_ROWS"; ok=0; }
[ "$nondec" -eq 1 ]    || { echo "ASSERT FAIL: sequence not non-decreasing"; ok=0; }
[ "$started_low" -eq 1 ] || { echo "ASSERT FAIL: did not start low"; ok=0; }

# (b) virtual hours elapsed (iter of last row minus first).
first_iter=$(grep -oE 'iter=[0-9]+' "$LOG" | head -1 | cut -d= -f2)
last_iter=$(grep -oE 'iter=[0-9]+' "$LOG" | tail -1 | cut -d= -f2)
vhours=$(( last_iter - first_iter ))
note "virtual hours elapsed: $vhours (target >= $TARGET_HOURS)"
[ "$vhours" -ge "$TARGET_HOURS" ] || { echo "ASSERT FAIL: fewer than $TARGET_HOURS virtual hours"; ok=0; }

# (c) cadence: consecutive guest-timestamp deltas ~ INTERVAL (+0/+2s tolerance).
min_d=999999; max_d=0; bad=0
for ((i=1; i<${#TS[@]}; i++)); do
  a=$(date -u -d "${TS[$((i-1))]}Z" +%s 2>/dev/null || echo 0)
  b=$(date -u -d "${TS[$i]}Z" +%s 2>/dev/null || echo 0)
  d=$(( b - a ))
  [ "$d" -lt "$min_d" ] && min_d="$d"
  [ "$d" -gt "$max_d" ] && max_d="$d"
  { [ "$d" -lt "$INTERVAL" ] || [ "$d" -gt "$(( INTERVAL + 2 ))" ]; } && bad=$(( bad + 1 ))
done
note "inter-insert delta (guest CLOCK_REALTIME): min=${min_d}s max=${max_d}s target=${INTERVAL} (+0/+2) out_of_tolerance=$bad"
[ "$bad" -eq 0 ] || { echo "ASSERT FAIL: $bad inter-insert gaps outside [$INTERVAL, $((INTERVAL+2))]s"; ok=0; }

# (d) speedup + per-hop + max-jump from the metric line.
if [ -n "$METRIC" ]; then
  speed=$(echo "$METRIC"   | grep -oE '= [0-9]+x'          | grep -oE '[0-9]+' | head -1)
  hopmean=$(echo "$METRIC" | grep -oE 'mean [0-9.]+us'     | grep -oE '[0-9.]+' | head -1)
  maxdelta=$(echo "$METRIC" | grep -oE 'max Δ [0-9.]+s'    | grep -oE '[0-9.]+' | head -1)
  note "speedup=${speed}x (REPORTED, not gated -- a stack property)  per-hop mean=${hopmean}us (<=500, HARD)  max Δ=${maxdelta}s (<=$MAX_JUMP_SECS)"
  awk "BEGIN{exit !($hopmean <= 500)}" || { echo "ASSERT FAIL: per-hop mean > 500us"; ok=0; }
  awk "BEGIN{exit !($maxdelta <= $MAX_JUMP_SECS)}" || { echo "ASSERT FAIL: max Δ > ${MAX_JUMP_SECS}s"; ok=0; }
else
  echo "ASSERT FAIL: no fast-forward metric line found"; ok=0
fi

# (e) timekeeping cleanliness: no bad clocksource/TSC/hrtimer warnings.
badtk=$(grep -icE 'unstable|marked unstable|hrtimer: interrupt took|clocksource watchdog|time went backwards|softlockup|rcu.*stall|watchdog: BUG' "$LOG")
note "timekeeping warnings (unstable/hrtimer/stall/...): $badtk"
[ "$badtk" -eq 0 ] || { echo "ASSERT FAIL: timekeeping warnings present"; ok=0; grep -iE 'unstable|hrtimer: interrupt took|stall' "$LOG" | head; }

echo
if [ "$ok" -eq 1 ]; then
  echo "FF DEMO PASS: $vhours virtual hours in ${WALL}s wall; cadence ${INTERVAL}s; ${speed}x; hop mean ${hopmean}us; maxΔ ${maxdelta}s; cap<=$MAX_ROWS."
  exit 0
fi
echo "FF DEMO FAIL: an assertion failed (see above)."
exit 1
