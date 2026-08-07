#!/usr/bin/env bash
# tdvmm Phase-2b CORPUS runner (PERMANENT tooling).
#
# Proves the supported compose subset on a set of realistic, real-world-shaped
# stacks -- each exercising SEVERAL features together (multi-service, health
# gating, build: contexts, ro/rw binds, named volumes, service-name DNS), all
# closed-world and fast-forwardable. For every corpus stack it:
#
#   1. BAKES it (`tdvmm build`) if the initramfs is missing or BAKE=1;
#   2. BOOTS it under fast-forward with a virtual-time horizon + --metrics-out
#      (the VMM stops ITSELF at the horizon, flushing metrics -- never a SIGTERM);
#   3. asserts FUNCTIONAL CORRECTNESS from the serial markers (services come up;
#      where present, health gating orders correctly -- the dependent starts only
#      AFTER its gate is healthy, checked against compose's own ordered stream);
#   4. asserts the VMM per-hop <=500us MEAN gate from the metrics file.
#
# Exits 0 only if every stack passes every gate.
#
# Usage: scripts/corpus_test.sh [stack ...]      (default: demo)
# Env:   BAKE=1 (force re-bake)  INTERVAL(60) MAX_ROWS(1000) MEM(3072)
#        TARGET_ROWS(4)  HC_TICK(2)  GATE_HOP_US(500)  WALL_TIMEOUT(300)
#
# NOTE on the virtual-time window: the guest-side healthcheck ticker runs
# `podman healthcheck run` every HC_TICK VIRTUAL seconds, and fast-forward
# collapses those sleeps, so its REAL cost scales with (horizon / HC_TICK) x
# (#healthcheck containers). We therefore keep the corpus window SMALL (a short
# INTERVAL + a horizon a few intervals out) -- enough to resolve the health gates
# and produce several inserts, without a multi-hour ticker flood. Cadence/speedup
# at a realistic 3600s interval is already gated by ff_demo / compare_stacks; the
# corpus gates functional correctness + gating order + the per-hop VMM property.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
BIN="$ROOT/target/release/tdvmm"
STACKS_DIR="$ROOT/testdata/stacks"
# Self-contained: bake each stack into a gitignored test dir, then `tdvmm run` it
# (kernel + initramfs come from the .tdvmm; no repo / ~/.tdvmm/artifacts dependency).
OUTDIR="${TDVMM_OUT_DIR:-$ROOT/.tdvmm-test-results}"; mkdir -p "$OUTDIR"

STACKS=("$@"); [ "${#STACKS[@]}" -eq 0 ] && STACKS=(demo)
BAKE="${BAKE:-0}"
INTERVAL="${INTERVAL:-60}"
MAX_ROWS="${MAX_ROWS:-1000}"
MEM="${MEM:-3072}"
TARGET_ROWS="${TARGET_ROWS:-4}"
HC_TICK="${HC_TICK:-2}"
GATE_HOP_US="${GATE_HOP_US:-500}"
WALL_TIMEOUT="${WALL_TIMEOUT:-300}"
# The VMM stops itself at this virtual-time horizon (exit 3), running the stop
# site that flushes --metrics-out. A couple intervals past the target rows -- kept
# small so the healthcheck ticker's real cost stays bounded (see the NOTE above).
# The health gates resolve within the first few virtual seconds, so a small
# horizon still clears them comfortably.
MAX_VIRTUAL_TIME="${MAX_VIRTUAL_TIME:-$(( (TARGET_ROWS + 2) * INTERVAL ))s}"

[ -x "$BIN" ] || { echo "[corpus] building tdvmm..."; ( cd "$ROOT" && cargo build --release ) || exit 3; }

TMP="$(mktemp -d)"; trap 'rm -f "$TMP"/*.log "$TMP"/*.metrics 2>/dev/null; rmdir "$TMP" 2>/dev/null' EXIT

# m <log-basename> <key> : read a value from a metrics file.
m() { awk -v k="$2" '$1==k{print $2}' "$TMP/$1.metrics" 2>/dev/null; }

# order_ok <log> <label> <first-regex> <second-regex> : true iff the first match
# line precedes the second (compose's own ordered [stack][up] lifecycle stream).
order_ok() {
  local log="$1" label="$2" a b
  a=$(grep -nE "$3" "$log" | head -1 | cut -d: -f1)
  b=$(grep -nE "$4" "$log" | head -1 | cut -d: -f1)
  if [ -n "$a" ] && [ -n "$b" ] && [ "$a" -lt "$b" ]; then
    echo "    ORDER OK: $label (line $a < $b)"; return 0
  fi
  echo "    ORDER FAIL: $label (first=$a second=$b)"; return 1
}

# rows_ok <log> : TDVMM_ROWCOUNT present, started low, non-decreasing, capped.
rows_ok() {
  local log="$1" prev=-1 mx=0 low=0 over=0 nondec=1 v
  while read -r v; do
    [ -z "$v" ] && continue
    [ "$prev" -eq -1 ] && [ "$v" -le 2 ] && low=1
    [ "$v" -lt "$prev" ] && nondec=0
    [ "$v" -gt "$mx" ] && mx="$v"
    [ "$v" -gt "$MAX_ROWS" ] && over=1
    prev="$v"
  done < <(grep -oE 'TDVMM_ROWCOUNT=[0-9]+' "$log" | cut -d= -f2)
  echo "    rows: started_low=$low non_decreasing=$nondec max=$mx cap=$MAX_ROWS over_cap=$over"
  [ "$low" -eq 1 ] && [ "$nondec" -eq 1 ] && [ "$over" -eq 0 ] && [ "$mx" -ge 1 ]
}

# have <log> <regex> <human> : assert a marker is present.
have() {
  if grep -qE "$2" "$1"; then echo "    OK: $3"; return 0; fi
  echo "    MISSING: $3 (/$2/)"; return 1
}

# ---- per-stack functional gates -------------------------------------------
gate_demo() {
  local log="$1" p=1
  local P=tdvmm_demo-postgres-1 R=tdvmm_demo-redis-1 A=tdvmm_demo-api-1
  have "$log" 'TDVMM_STACK_UP' 'compose brought the stack up' || p=0
  have "$log" "TDVMM_HC_HEALTHY container=$P" 'postgres reached healthy (ticker)' || p=0
  have "$log" "TDVMM_HC_HEALTHY container=$R" 'redis reached healthy (ticker)' || p=0
  have "$log" 'api gRPC OrderService listening' 'api started (=> both service_healthy gates resolved)' || p=0
  have "$log" 'hour [0-9]+: received [0-9]+ orders' 'client->api->postgres+redis gRPC roundtrip worked' || p=0
  have "$log" 'GET /stats  pg\+redis OK' 'api served the read-side gRPC (pg+redis)' || p=0
  have "$log" 'rollup h[0-9]+: [0-9]+ orders -> summary' 'worker rolled up orders into a summary' || p=0
  # ordering: both backends Healthy before the api Started (compose stream).
  order_ok "$log" "postgres Healthy < api Started" \
    "\[stack\]\[up\].*Container $P Healthy" "\[stack\]\[up\].*Container $A Started" || p=0
  order_ok "$log" "redis Healthy < api Started" \
    "\[stack\]\[up\].*Container $R Healthy" "\[stack\]\[up\].*Container $A Started" || p=0
  return $((1 - p))
}

overall=0
declare -A RESULT

for stack in "${STACKS[@]}"; do
  echo "==================================================================="
  echo " CORPUS STACK: $stack"
  echo "==================================================================="
  compose="$STACKS_DIR/$stack/compose.yml"
  tdvmm="$OUTDIR/${stack}.tdvmm"
  if [ ! -f "$compose" ]; then echo "  FAIL: no compose.yml at $compose"; RESULT[$stack]="fail:no-compose"; overall=1; continue; fi

  if [ "$BAKE" = "1" ] || [ ! -f "$tdvmm" ]; then
    echo "[corpus] baking $stack -> $tdvmm ..."
    if ! "$BIN" build "$stack" "$compose" -o "$tdvmm" >"$TMP/$stack.bake.log" 2>&1; then
      echo "  FAIL: bake error (tail):"; tail -20 "$TMP/$stack.bake.log" | sed 's/^/    /'
      RESULT[$stack]="fail:bake"; overall=1; continue
    fi
    echo "  baked OK: $(grep -E 'sha256:' "$TMP/$stack.bake.log" | tail -1 | sed 's/^ *//')"
  else
    echo "[corpus] using existing artifact ($(basename "$tdvmm")); set BAKE=1 to re-bake"
  fi
  [ -f "$tdvmm" ] || { echo "  FAIL: artifact missing after bake"; RESULT[$stack]="fail:no-tdvmm"; overall=1; continue; }

  log="$TMP/$stack.log"; metrics="$TMP/$stack.metrics"
  cmdline="console=ttyS0 reboot=t panic=1 pci=off no_timer_check tsc=reliable tdvmm.stack=1 tdvmm.hc_tick=$HC_TICK tdvmm.interval=$INTERVAL tdvmm.maxrows=$MAX_ROWS"
  echo "[corpus] boot: mem=${MEM}MiB ff=ON horizon=${MAX_VIRTUAL_TIME} hc_tick=${HC_TICK}s wall_timeout=${WALL_TIMEOUT}s"
  start=$(date +%s.%N)
  timeout "$WALL_TIMEOUT" "$BIN" run "$tdvmm" --mem "$MEM" --ff on \
    --max-virtual-time "$MAX_VIRTUAL_TIME" --metrics-out "$metrics" --cmdline "$cmdline" \
    </dev/null >"$log" 2>&1
  rc=$?
  wall=$(awk "BEGIN{printf \"%.1f\", $(date +%s.%N)-$start}")
  echo "[corpus] vmm exit=$rc (3=horizon, expected) wall=${wall}s"

  # ---- functional gates -----------------------------------------------------
  echo "  [functional correctness under FF]"
  ok=1
  "gate_$stack" "$log" || ok=0

  # ---- FF summary + per-hop <=500us mean ------------------------------------
  echo "  [fast-forward + per-hop <=${GATE_HOP_US}us mean]"
  grep -E 'speedup|idle skipped|per-hop mean' "$log" | sed 's/^/    /'
  hm="$(m "$stack" hop_ns_mean)"; [ -z "$hm" ] && hm=0
  sp="$(m "$stack" speedup)"
  us=$(awk "BEGIN{printf \"%.3f\", $hm/1000}")
  if [ -f "$metrics" ] && awk "BEGIN{exit !($hm>0 && $hm/1000 <= $GATE_HOP_US)}"; then
    echo "    OK: per-hop mean ${us}us <= ${GATE_HOP_US}us ; speedup ${sp}x (metrics flushed)"
  else
    echo "    GATE FAIL: per-hop mean ${us}us (metrics $( [ -f "$metrics" ] && echo present || echo MISSING))"; ok=0
  fi

  if [ "$ok" -eq 1 ]; then echo "  => $stack PASS"; RESULT[$stack]="pass";
  else echo "  => $stack FAIL"; RESULT[$stack]="fail:gates"; overall=1
    echo "  ---- last serial output ----"; tail -20 "$log" | sed 's/^/    /'
  fi
  echo
done

echo "==================================================================="
echo " CORPUS SUMMARY"
echo "==================================================================="
for stack in "${STACKS[@]}"; do printf '  %-16s %s\n' "$stack" "${RESULT[$stack]:-?}"; done
if [ "$overall" -eq 0 ]; then
  echo "CORPUS PASS: every stack baked + booted + ran correctly under FF (functional + health-gating); per-hop mean within ${GATE_HOP_US}us."
  exit 0
fi
echo "CORPUS FAIL: see stacks above."
exit 1
