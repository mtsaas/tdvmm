#!/usr/bin/env bash
# deterministic-vmm Phase-2b comparison harness (PERMANENT tooling).
#
# Runs ANY TWO baked stacks under fast-forward and emits a STABLE, side-by-side
# report of how each behaves. Built to compare the shell insert/trim service
# (CONTROL) against the Go insert/trim service (TREATMENT) — same workload, the
# runtime is the only variable — but it is generic: pass any two stack names.
#
# It consumes the VMM's EXISTING per-run fast-forward metrics (dvmm --metrics-out:
# the jump/speedup accounting + the Δvtsc histogram + the real-vs-virtual
# accounting) rather than re-deriving anything, and prints, side by side:
#   - hops (jumps) and hops per virtual-hour
#   - speedup (virtual-s / real-s)
#   - per-hop real cost: mean + p99 + max
#   - the Δvtsc histogram (the attribution instrument: ~10-20ms=Go sysmon,
#     ~200ms=Postgres writers, ~2min=Go forced GC)
#   - real-vs-virtual accounting: fraction executing vs jumping, and the real
#     execution ms per virtual-hour — a BUSY-WAIT TRIPWIRE (a runtime that spins
#     instead of parking burns real execution time during nominal idle).
#
# It also checks each stack's functional correctness under FF (inserts at the
# interval, cap holds) and the per-hop <=500us mean gate (the VMM property).
#
# Usage: scripts/compare_stacks.sh [stackA] [stackB] [target_virtual_hours]
#   stackA/stackB: stack names (default: dogfood go-ab) -> initramfs-alpine-<name>.cpio.gz
# Env: INTERVAL (3600) MAX_ROWS (1000) MEM (3072) MAX_JUMP_SECS (300)
#      WALL_TIMEOUT (400)  GATE_HOP_US (500)
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
BIN="$ROOT/target/release/dvmm"
KERNEL="$ROOT/guest/kernel/vmlinux-6.1.128"
ALPINE="$ROOT/guest/initramfs-alpine"

STACK_A="${1:-dogfood}"
STACK_B="${2:-go-ab}"
TARGET_HOURS="${3:-6}"
INTERVAL="${INTERVAL:-3600}"
MAX_ROWS="${MAX_ROWS:-1000}"
MEM="${MEM:-3072}"
MAX_JUMP_SECS="${MAX_JUMP_SECS:-300}"
WALL_TIMEOUT="${WALL_TIMEOUT:-400}"
GATE_HOP_US="${GATE_HOP_US:-500}"
# The VMM stops ITSELF at this virtual-time horizon (exit 3), which runs the stop
# site that flushes --metrics-out. Set it a couple of intervals past the rows we
# need (target+1), so we get the rows AND a clean, metrics-flushing stop — never a
# SIGTERM kill (which would skip the flush). Boot-to-first-insert virtual time is
# << INTERVAL, so this comfortably yields >= target+1 inserts.
MAX_VIRTUAL_TIME="${MAX_VIRTUAL_TIME:-$(( (TARGET_HOURS + 2) * INTERVAL ))s}"

[ -x "$BIN" ] || { echo "compare: building dvmm..."; ( cd "$ROOT" && cargo build --release ) || exit 3; }
[ -f "$KERNEL" ] || { echo "compare: kernel missing: $KERNEL"; exit 3; }

TMP="$(mktemp -d)"
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT

# run_stack <name> <label>  -> writes $TMP/<label>.metrics, $TMP/<label>.rows,
# $TMP/<label>.log ; echoes "pass"/"fail:<reason>" on stdout.
run_stack() {
  local name="$1" label="$2"
  local initrd="$ALPINE/initramfs-alpine-${name}.cpio.gz"
  local log="$TMP/$label.log" metrics="$TMP/$label.metrics"
  if [ ! -f "$initrd" ]; then echo "fail:no initramfs for '$name' ($initrd) -- bake it first"; return; fi

  local cmdline="console=ttyS0 reboot=t panic=1 pci=off no_timer_check tsc=reliable dvmm.stack=1 dvmm.interval=$INTERVAL dvmm.maxrows=$MAX_ROWS"
  local start_wall end_wall pid
  start_wall=$(date +%s.%N)
  "$BIN" boot --kernel "$KERNEL" --initrd "$initrd" --mem "$MEM" --ff on \
    --max-jump-secs "$MAX_JUMP_SECS" --max-virtual-time "$MAX_VIRTUAL_TIME" \
    --metrics-out "$metrics" --cmdline "$cmdline" </dev/null >"$log" 2>&1 &
  pid=$!

  local need=$(( TARGET_HOURS + 1 ))   # need TARGET_HOURS full intervals -> +1 rows
  local deadline=$(( $(date +%s) + WALL_TIMEOUT )) result="" reason=""
  # Wait for the VMM to stop ITSELF at the virtual-time horizon (which flushes
  # --metrics-out at the stop site). Only SIGTERM it as a last resort on timeout.
  while [ "$(date +%s)" -lt "$deadline" ]; do
    if grep -qE 'exceeds the sanity bound|panicked|DVMM_SVC_FAIL|DVMM_STACK_FAIL|dvmm: fatal' "$log"; then
      result="fail"; reason="vmm/guest error"; break
    fi
    kill -0 "$pid" 2>/dev/null || break   # exited on its own (horizon) -> metrics flushed
    sleep 2
  done
  if kill -0 "$pid" 2>/dev/null; then
    if [ -z "$result" ]; then result="fail"; reason="timeout after ${WALL_TIMEOUT}s ($(grep -c 'DVMM_ROWCOUNT=' "$log") rows)"; fi
    kill "$pid" 2>/dev/null
  fi
  wait "$pid" 2>/dev/null
  end_wall=$(date +%s.%N)

  local rows; rows=$(grep -c 'DVMM_ROWCOUNT=' "$log" 2>/dev/null); rows=${rows:-0}
  if [ -z "$result" ]; then
    if [ -f "$metrics" ] && [ "$rows" -ge "$need" ]; then
      result="pass"
    else
      result="fail"; reason="stopped with rows=$rows (need $need) / metrics $( [ -f "$metrics" ] && echo present || echo MISSING )"
    fi
  fi
  awk "BEGIN{printf \"%.1f\", $end_wall-$start_wall}" > "$TMP/$label.wall"

  grep -oE 'DVMM_ROWCOUNT=[0-9]+' "$log" | cut -d= -f2 > "$TMP/$label.rows"
  grep -oE 'DVMM_ROWCOUNT=[0-9]+ iter=[0-9]+ max=[0-9]+ ts=[0-9T:-]+Z' "$log" \
    | sed -E 's/.*ts=([0-9T:-]+)Z/\1/' > "$TMP/$label.ts"
  [ "$result" = "pass" ] && echo "pass" || echo "fail:$reason"
}

# m <label> <key> : read a value from a stack's metrics file (empty if absent).
m() { awk -v k="$2" '$1==k{print $2}' "$TMP/$1.metrics" 2>/dev/null; }

# functional gates over a stack's row sequence (same properties as the dogfood
# ff_demo): started low, non-decreasing, capped at MAX_ROWS, cadence ~INTERVAL.
functional_gate() {
  local label="$1"; local ok=1 mx=0 prev=-1 nondec=1 low=0 over=0 v
  while read -r v; do
    [ -z "$v" ] && continue
    [ "$v" -gt "$MAX_ROWS" ] && over=1
    [ "$v" -gt "$mx" ] && mx="$v"
    [ "$v" -lt "$prev" ] && nondec=0
    [ "$prev" -eq -1 ] && [ "$v" -le 2 ] && low=1
    prev="$v"
  done < "$TMP/$label.rows"
  local min_d=999999 max_d=0 bad=0 a b d i=0 prevts=""
  while read -r ts; do
    [ -z "$ts" ] && continue
    if [ -n "$prevts" ]; then
      a=$(date -u -d "${prevts}Z" +%s 2>/dev/null || echo 0)
      b=$(date -u -d "${ts}Z" +%s 2>/dev/null || echo 0)
      d=$(( b - a ))
      [ "$d" -lt "$min_d" ] && min_d="$d"
      [ "$d" -gt "$max_d" ] && max_d="$d"
      { [ "$d" -lt "$INTERVAL" ] || [ "$d" -gt "$(( INTERVAL + 2 ))" ]; } && bad=$(( bad + 1 ))
    fi
    prevts="$ts"
  done < "$TMP/$label.ts"
  echo "  $label: started_low=$low non_decreasing=$nondec max=$mx cap=$MAX_ROWS over_cap=$over  cadence min=${min_d}s max=${max_d}s out_of_tol=$bad"
  [ "$over" -eq 0 ] && [ "$nondec" -eq 1 ] && [ "$low" -eq 1 ] && [ "$bad" -eq 0 ] && [ "$mx" -ge 1 ] && return 0
  return 1
}

echo "==================================================================="
echo " deterministic-vmm  stack comparison under fast-forward"
echo "   A (control)   = $STACK_A"
echo "   B (treatment) = $STACK_B"
echo "   interval=${INTERVAL}s  max_rows=$MAX_ROWS  target=${TARGET_HOURS} virtual-hours  mem=${MEM}MiB"
echo "==================================================================="

echo "[compare] running A ($STACK_A) ..."
RA="$(run_stack "$STACK_A" A)"
echo "[compare] running B ($STACK_B) ..."
RB="$(run_stack "$STACK_B" B)"

WA="$(cat "$TMP/A.wall" 2>/dev/null || echo '?')"; WB="$(cat "$TMP/B.wall" 2>/dev/null || echo '?')"

# ---- side-by-side report ---------------------------------------------------
row() { printf '  %-30s %18s %18s\n' "$1" "$2" "$3"; }
echo
echo "================= SIDE-BY-SIDE (A=$STACK_A  vs  B=$STACK_B) ================="
printf '  %-30s %18s %18s\n' "metric" "A:$STACK_A" "B:$STACK_B"
printf '  %-30s %18s %18s\n' "------------------------------" "------------------" "------------------"
row "run status"            "$RA"                       "$RB"
row "wall time (s)"         "$WA"                       "$WB"
row "virtual time (s)"      "$(m A virtual_secs)"       "$(m B virtual_secs)"
row "hops (jumps)"          "$(m A jumps)"              "$(m B jumps)"
row "hops / virtual-hour"   "$(m A hops_per_virtual_hour)" "$(m B hops_per_virtual_hour)"
row "speedup (virt-s/real-s)" "$(m A speedup)"          "$(m B speedup)"
row "per-hop mean (ns)"     "$(m A hop_ns_mean)"        "$(m B hop_ns_mean)"
row "per-hop p99 (ns)"      "$(m A hop_ns_p99)"         "$(m B hop_ns_p99)"
row "per-hop max (ns)"      "$(m A hop_ns_max)"         "$(m B hop_ns_max)"
row "max single Δ (s)"      "$(m A max_delta_secs)"     "$(m B max_delta_secs)"
row "HLT / virtual-hour"    "$(m A hlt_per_virtual_hour)" "$(m B hlt_per_virtual_hour)"
echo "  --- real-vs-virtual accounting (busy-wait tripwire) ---"
row "executing fraction"    "$(m A executing_fraction)" "$(m B executing_fraction)"
row "jumping fraction"      "$(m A jumping_fraction)"   "$(m B jumping_fraction)"
row "real-exec ms / v-hour" "$(m A exec_real_ms_per_vhour)" "$(m B exec_real_ms_per_vhour)"

echo
echo "================= Δvtsc HISTOGRAM (jumps by advance) ================="
LABELS="$(m A hist_labels)"; [ -z "$LABELS" ] && LABELS="$(m B hist_labels)"
IFS=',' read -r -a LB <<< "$LABELS"
IFS=',' read -r -a CA <<< "$(m A hist_counts)"
IFS=',' read -r -a CB <<< "$(m B hist_counts)"
printf '  %-10s %18s %18s\n' "bucket" "A:$STACK_A" "B:$STACK_B"
printf '  %-10s %18s %18s\n' "--------" "------------------" "------------------"
for i in "${!LB[@]}"; do
  printf '  %-10s %18s %18s\n' "${LB[$i]}" "${CA[$i]:-0}" "${CB[$i]:-0}"
done
printf '  %s\n' "  (attribution: ~<100ms=Go sysmon | ~<1s=Postgres bg-writers | >=10s=Go forced-GC / deep idle)"

echo
echo "================= GATES ================="
gok=1
echo "[functional correctness under FF]"
functional_gate A || { echo "  GATE FAIL: A ($STACK_A) functional"; gok=0; }
functional_gate B || { echo "  GATE FAIL: B ($STACK_B) functional"; gok=0; }
echo "[per-hop <= ${GATE_HOP_US}us mean (VMM property)]"
for L in A B; do
  hm="$(m $L hop_ns_mean)"; [ -z "$hm" ] && hm=0
  us=$(awk "BEGIN{printf \"%.3f\", $hm/1000}")
  if awk "BEGIN{exit !($hm/1000 <= $GATE_HOP_US)}"; then
    echo "  OK: $L per-hop mean ${us}us <= ${GATE_HOP_US}us"
  else
    echo "  GATE FAIL: $L per-hop mean ${us}us > ${GATE_HOP_US}us  (VMM finding, NOT a stack finding)"; gok=0
  fi
done

echo
if [ "$RA" = "pass" ] && [ "$RB" = "pass" ] && [ "$gok" -eq 1 ]; then
  echo "COMPARE PASS: both stacks ran correctly under FF; per-hop mean within ${GATE_HOP_US}us; report above."
  exit 0
fi
echo "COMPARE FAIL: see run status / gates above."
echo "---- A tail ----"; tail -15 "$TMP/A.log" 2>/dev/null
echo "---- B tail ----"; tail -15 "$TMP/B.log" 2>/dev/null
exit 1
