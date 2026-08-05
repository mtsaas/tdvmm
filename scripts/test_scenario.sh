#!/usr/bin/env bash
# tdvmm TEST-1a acceptance + negative gates for `tdvmm test`.
#
# Runs the insert-trim-as-scenario acceptance and the exit-code contract:
#   - insert-trim.tdvmm + insert-trim.yml    -> PASS, exit 0 (JSONL + report produced)
#   - a deliberately WRONG assertion         -> exit 1 (assertion failure)
#   - static validation (unknown service /   -> exit 2, sub-second, BEFORE boot
#     unknown key / bad duration)
#   - a runtime infrastructure error         -> exit 2 (distinct from 1)
#
# Usage: scripts/test_scenario.sh
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
BIN="$ROOT/target/release/tdvmm"
# Self-contained: bake into a gitignored test dir (NOT the repo / ~/.tdvmm/artifacts).
OUTDIR="${TDVMM_OUT_DIR:-$ROOT/.tdvmm-test-results}"; mkdir -p "$OUTDIR"
TDVMM="${TDVMM_ARTIFACT:-$OUTDIR/insert-trim.tdvmm}"
SCN="$ROOT/guest/stacks/insert-trim/insert-trim.yml"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

[ -x "$BIN" ] || { echo "building tdvmm..."; ( cd "$ROOT" && cargo build --release ) || exit 3; }
if [ ! -f "$TDVMM" ]; then
  echo "== insert-trim.tdvmm missing — baking it (tdvmm build -> $TDVMM) =="
  "$BIN" build insert-trim "$ROOT/guest/stacks/insert-trim/compose.yml" -o "$TDVMM" || {
    echo "FATAL: bake failed" >&2; exit 3; }
fi

PASS=0; FAIL=0
ok()   { echo "  PASS: $*"; PASS=$((PASS+1)); }
bad()  { echo "  FAIL: $*"; FAIL=$((FAIL+1)); }

# ---- Gate 1: insert-trim acceptance -> PASS, exit 0 ------------------------
echo "== Gate 1: insert-trim-as-scenario (expect PASS, exit 0) =="
JSONL="$TMP/insert-trim.jsonl"; REPORT="$TMP/insert-trim.report.json"
"$BIN" test "$TDVMM" --scenario "$SCN" --jsonl "$JSONL" --report "$REPORT" \
  --wall-timeout 300 >"$TMP/g1.out" 2>&1
code=$?
sed 's/^/    /' "$TMP/g1.out" | grep -E 'VERDICT|steps:|assertion|rowcount|ready|effective-config|speedup' | tail -20
[ "$code" -eq 0 ] && ok "exit 0" || bad "expected exit 0, got $code"
[ -f "$JSONL" ] && grep -q '"type":"run_end"' "$JSONL" && ok "JSONL run log produced" || bad "no JSONL run log"
[ -f "$REPORT" ] && grep -q '"verdict": "pass"' "$REPORT" && ok "JSON report verdict=pass" || bad "report not pass"
grep '"type":"assertion"' "$JSONL" | grep -q '"passed":true' && ok "exec assertions evaluated" || bad "no assertion events"

# ---- Gate 2: a WRONG assertion -> exit 1 -----------------------------------
echo "== Gate 2: wrong assertion (expect exit 1) =="
cat > "$TMP/wrong.yml" <<'YML'
name: wrong-on-purpose
run:
  cmdline: "console=ttyS0 reboot=t panic=1 pci=off no_timer_check tsc=reliable tdvmm.stack=1 tdvmm.interval=3600 tdvmm.maxrows=5 tdvmm.hc_tick=2"
steps:
  - at: 0s
    wait_for:
      probe: { exec: { container: service, cmd: "pg_isready -q" } }
      until: exit_zero
      every: 15s
      timeout: 5m
  - name: impossible-count
    at: 1h
    exec: { container: service, cmd: "psql -tAc 'select count(*) from events;'" }
    expect: { exit: 0, output_matches: '^9999$' }
YML
"$BIN" test "$TDVMM" --scenario "$TMP/wrong.yml" --jsonl "$TMP/w.jsonl" \
  --report "$TMP/w.report.json" --wall-timeout 200 >"$TMP/g2.out" 2>&1
code=$?
grep -E 'VERDICT|assertion' "$TMP/g2.out" | tail -4 | sed 's/^/    /'
[ "$code" -eq 1 ] && ok "exit 1 (assertion failure)" || bad "expected exit 1, got $code"
grep -q '"verdict": "fail"' "$TMP/w.report.json" 2>/dev/null && ok "report verdict=fail" || bad "report not fail"

# ---- Gate 3: static validation -> exit 2, sub-second, BEFORE boot ----------
echo "== Gate 3: static validation (expect exit 2, sub-second) =="
mk() { printf '%s\n' "$2" > "$TMP/$1"; }
mk badsvc.yml 'steps:
  - at: 1h
    exec: { container: nonexistent, cmd: "true" }'
mk badkey.yml 'steps:
  - at: 1h
    bogus_key: 1
    exec: { container: service, cmd: "true" }'
mk baddur.yml 'steps:
  - at: 5furlongs
    exec: { container: service, cmd: "true" }'
for f in badsvc.yml badkey.yml baddur.yml; do
  t0=$(date +%s.%N)
  "$BIN" test "$TDVMM" --scenario "$TMP/$f" >"$TMP/$f.out" 2>&1
  code=$?
  t1=$(date +%s.%N)
  dt=$(awk "BEGIN{printf \"%.2f\", $t1-$t0}")
  reason=$(grep -oE 'scenario rejected.*' "$TMP/$f.out" | head -1)
  if [ "$code" -eq 2 ]; then ok "$f -> exit 2 in ${dt}s ($reason)"; else bad "$f -> expected exit 2, got $code"; fi
  awk "BEGIN{exit !($dt < 1.0)}" && ok "$f rejected sub-second (${dt}s, before boot)" || bad "$f took ${dt}s (not sub-second)"
done

# ---- Gate 4: runtime infrastructure error -> exit 2 (distinct from 1) ------
echo "== Gate 4: runtime infra error, agent absent (expect exit 2) =="
cat > "$TMP/infra.yml" <<'YML'
name: agent-absent
run:
  # tdvmm.noagent=1 tells guest init NOT to start the control agent, so no agent
  # ever reports ready -> the harness cannot reach the control channel -> infra.
  cmdline: "console=ttyS0 reboot=t panic=1 pci=off no_timer_check tsc=reliable tdvmm.stack=1 tdvmm.noagent=1"
steps:
  - at: 0s
    exec: { container: service, cmd: "true" }
    expect: { exit: 0 }
YML
"$BIN" test "$TDVMM" --scenario "$TMP/infra.yml" --jsonl "$TMP/i.jsonl" \
  --report "$TMP/i.report.json" --wall-timeout 200 >"$TMP/g4.out" 2>&1
code=$?
grep -E 'VERDICT|FAILURE|agent' "$TMP/g4.out" | tail -4 | sed 's/^/    /'
[ "$code" -eq 2 ] && ok "exit 2 (infrastructure error, distinct from 1)" || bad "expected exit 2, got $code"
grep -q '"verdict": "error"' "$TMP/i.report.json" 2>/dev/null && ok "report verdict=error" || bad "report not error"

echo
echo "==== TEST-1a gates: $PASS passed, $FAIL failed ===="
[ "$FAIL" -eq 0 ]
