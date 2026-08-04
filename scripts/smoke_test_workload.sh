#!/usr/bin/env bash
# Step 2b workload acceptance test.
#
# Boots the Alpine guest in WORKLOAD mode with a small INTERVAL_SECONDS and a
# small MAX_ROWS, watches the serial console, and asserts the two properties of
# the closed-world Postgres + insert/trim workload:
#
#   (a) rows ACCUMULATE at the interval  -- TDVMM_ROWCOUNT rises 1,2,3,... and the
#       wall-time between successive inserts is ~INTERVAL_SECONDS (genuine sleep,
#       so the guest HLTs between inserts), and
#   (b) the row count CAPS at MAX_ROWS and NEVER exceeds it as inserts continue
#       past the cap (the trim holds).
#
# Exits 0 only if both hold; non-zero otherwise. Also reports the measured peak
# guest RAM use (from the guest's TDVMM_MEM samples).
#
# The workload loops forever (never powers off), so this test stops the VMM once
# it has its proof.
#
# Usage: scripts/smoke_test_workload.sh [timeout_seconds]
# Env:   INTERVAL_SECONDS (default 2)  MAX_ROWS (default 5)  PAST_CAP (default 4)
#        MEM (default 3072)  STACK (default insert-trim)
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"

TIMEOUT="${1:-240}"
STACK="${STACK:-insert-trim}"
# Self-contained: bake the stack into a gitignored test dir, then `tdvmm run` it
# (kernel + initramfs come from the .tdvmm; no repo / ~/.tdvmm/artifacts dependency).
OUTDIR="${TDVMM_OUT_DIR:-$ROOT/.tdvmm-test-results}"; mkdir -p "$OUTDIR"
TDVMM="${TDVMM_ARTIFACT:-$OUTDIR/$STACK.tdvmm}"

INTERVAL_SECONDS="${INTERVAL_SECONDS:-2}"
MAX_ROWS="${MAX_ROWS:-5}"
PAST_CAP="${PAST_CAP:-4}"     # inserts required past the cap, all staying capped
MEM="${MEM:-3072}"           # VMM caps guest RAM at 3 GiB (32-bit MMIO gap)
FF="${FF:-off}"              # fast-forward on|off. Default OFF so this stays a
                            # REAL-TIME test (inserts spaced by wall seconds);
                            # the Step-4 fast-forward demo is scripts/ff_demo.sh.
# Virtual-time horizon (safety net): bound the run's virtual duration well above
# this test's known budget (a few interval*rows seconds + boot), so a wedged
# guest can't fast-forward forever. Comfortably clears a healthy run.
MAX_VIRTUAL_TIME="${MAX_VIRTUAL_TIME:-$(( TIMEOUT + 300 ))s}"
BIN="$ROOT/target/release/tdvmm"

if [ ! -x "$BIN" ]; then
  echo "building release binary..."
  ( cd "$ROOT" && cargo build --release ) || { echo "SMOKE FAIL: build error"; exit 3; }
fi
if [ ! -f "$TDVMM" ]; then
  echo "[smoke] baking $STACK -> $TDVMM"
  "$BIN" build "$ROOT/guest/stacks/$STACK/compose.yml" -o "$TDVMM" \
    || { echo "SMOKE FAIL: bake error"; exit 3; }
fi

LOG="$(mktemp)"
PID=""
cleanup() { [ -n "$PID" ] && kill "$PID" 2>/dev/null; [ -n "$PID" ] && wait "$PID" 2>/dev/null; rm -f "$LOG"; }
trap cleanup EXIT

# tdvmm.memsample=1 opts into the guest RAM sampler (the TDVMM_MEM console lines);
# this test measures peak guest RAM, so it enables it (the sampler is OFF by
# default so normal / fast-forward runs are not flooded).
CMDLINE="console=ttyS0 reboot=t panic=1 pci=off no_timer_check tsc=reliable tdvmm.stack=1 tdvmm.interval=$INTERVAL_SECONDS tdvmm.maxrows=$MAX_ROWS tdvmm.memsample=1"
echo "[smoke] run: mem=${MEM}MiB interval=${INTERVAL_SECONDS}s max_rows=$MAX_ROWS past_cap=$PAST_CAP ff=$FF timeout=${TIMEOUT}s max-virtual-time=${MAX_VIRTUAL_TIME}"
"$BIN" run "$TDVMM" --mem "$MEM" --ff "$FF" \
  --max-virtual-time "$MAX_VIRTUAL_TIME" --cmdline "$CMDLINE" \
  </dev/null >"$LOG" 2>&1 &
PID=$!

# Extract the ordered TDVMM_ROWCOUNT counts and their guest timestamps.
counts_of()   { grep -oE 'TDVMM_ROWCOUNT=[0-9]+' "$LOG" | cut -d= -f2; }
result=""; reason=""
deadline=$(( $(date +%s) + TIMEOUT ))
while [ "$(date +%s)" -lt "$deadline" ]; do
  if grep -qE 'TDVMM_WORKLOAD_FAIL|TDVMM_SVC_FAIL' "$LOG"; then result="fail"; reason="guest reported failure"; break; fi

  mapfile -t C < <(counts_of)
  n="${#C[@]}"
  if [ "$n" -ge 1 ]; then
    # (b) cap invariant: no sample may exceed MAX_ROWS.
    over=0; mx=0
    for v in "${C[@]}"; do
      [ "$v" -gt "$MAX_ROWS" ] && over=1
      [ "$v" -gt "$mx" ] && mx="$v"
    done
    if [ "$over" -eq 1 ]; then result="fail"; reason="row count exceeded MAX_ROWS=$MAX_ROWS"; break; fi

    # proof: reached the cap AND observed >= PAST_CAP more capped inserts.
    if [ "$mx" -eq "$MAX_ROWS" ]; then
      capped=0; for v in "${C[@]}"; do [ "$v" -eq "$MAX_ROWS" ] && capped=$((capped+1)); done
      if [ "$capped" -ge "$((PAST_CAP + 1))" ]; then result="pass"; break; fi
    fi
  fi

  kill -0 "$PID" 2>/dev/null || { result="fail"; reason="VMM exited early"; break; }
  sleep 1
done
[ -z "$result" ] && { result="fail"; reason="timeout after ${TIMEOUT}s"; }

# ---- final assertions on the full captured sequence -----------------------
mapfile -t C < <(counts_of)
mapfile -t TS < <(grep -oE 'TDVMM_ROWCOUNT=[0-9]+ iter=[0-9]+ max=[0-9]+ ts=[0-9T:-]+Z' "$LOG" \
                    | sed -E 's/.*ts=([0-9T:-]+)Z/\1/')

echo
echo "==== observed TDVMM_ROWCOUNT sequence (${#C[@]} samples) ===="
grep -E 'TDVMM_ROWCOUNT=' "$LOG" | sed 's/^/  /'
echo "==========================================================="

if [ "$result" = "pass" ]; then
  # growth + non-decreasing + cadence checks over the full sequence.
  prev=-1; nondec=1; started_low=0; mx=0
  for v in "${C[@]}"; do
    [ "$v" -lt "$prev" ] && nondec=0
    [ "$prev" -eq -1 ] && [ "$v" -le 2 ] && started_low=1
    [ "$v" -gt "$mx" ] && mx="$v"
    prev="$v"
  done
  # cadence: seconds between successive inserts should be ~INTERVAL (>=1 => the
  # service genuinely slept, not busy-looped). Uses the guest wall clock.
  min_delta=99999; max_delta=0; deltas=""
  for ((i=1; i<${#TS[@]}; i++)); do
    a=$(date -u -d "${TS[$((i-1))]}Z" +%s 2>/dev/null || echo 0)
    b=$(date -u -d "${TS[$i]}Z" +%s 2>/dev/null || echo 0)
    d=$(( b - a )); [ "$d" -lt 0 ] && d=$(( d + 86400 ))
    deltas="$deltas $d"
    [ "$d" -lt "$min_delta" ] && min_delta="$d"
    [ "$d" -gt "$max_delta" ] && max_delta="$d"
  done

  echo "[smoke] max_count=$mx (cap=$MAX_ROWS)  non_decreasing=$nondec  started_low=$started_low"
  echo "[smoke] inter-insert deltas (s):$deltas  (min=$min_delta max=$max_delta target~${INTERVAL_SECONDS}s)"

  ok=1
  [ "$mx" -eq "$MAX_ROWS" ]      || { echo "ASSERT FAIL: never reached cap"; ok=0; }
  [ "$nondec" -eq 1 ]           || { echo "ASSERT FAIL: sequence not non-decreasing"; ok=0; }
  [ "$started_low" -eq 1 ]      || { echo "ASSERT FAIL: did not start from a low count (no growth)"; ok=0; }
  [ "$min_delta" -ge 1 ]        || { echo "ASSERT FAIL: inserts not spaced by ~interval (guest not sleeping)"; ok=0; }
  [ "$max_delta" -le "$((INTERVAL_SECONDS + 5))" ] || { echo "ASSERT FAIL: inserts stalled (delta $max_delta > interval+5)"; ok=0; }

  # peak guest RAM from TDVMM_MEM samples: peak_used = MemTotal - min(MemAvailable)
  peak_line="$(awk '
    /TDVMM_MEM/ {
      for (i=1;i<=NF;i++){ if ($i ~ /^MemTotal:/) t=$i; if ($i ~ /^MemAvailable:/) a=$i }
      gsub(/[^0-9]/,"",t); gsub(/[^0-9]/,"",a);
      if (t!="") T=t;
      if (a!="" && (min==0 || a<min)) min=a;
    }
    END { if (T>0 && min>0) printf "%d %d", T, min; }' "$LOG")"
  if [ -n "$peak_line" ]; then
    set -- $peak_line; T="$1"; A="$2"
    echo "[smoke] measured peak guest RAM: $(( (T - A) / 1024 )) MiB used of $(( T / 1024 )) MiB (min MemAvailable $(( A / 1024 )) MiB)"
  else
    echo "[smoke] (no TDVMM_MEM samples captured)"
  fi

  if [ "$ok" -eq 1 ]; then
    echo "SMOKE PASS: rows accumulated at the interval and capped at MAX_ROWS=$MAX_ROWS (never exceeded)."
    exit 0
  fi
  echo "SMOKE FAIL: pass condition met but a final assertion failed."
  exit 1
else
  echo "SMOKE FAIL: $reason"
  echo "---- last serial output ----"
  tail -50 "$LOG"
  exit 1
fi
