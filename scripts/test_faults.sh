#!/usr/bin/env bash
# deterministic-vmm TEST-1b acceptance + negative gates for FAULT INJECTION.
#
# The MVP fault set (kill/stop/start + partition/heal), delivered as scheduled
# (vtsc, ScenarioStep) queue entries, exercised end-to-end against the faultlab
# stack (db + an idle client that probes db by name):
#
#   Gate 1  kill-recover.yml     -> PASS exit 0  (kill db, dependent fails, start db, recovers)
#   Gate 2  partition-heal.yml   -> PASS exit 0  (drop client<->db, connectivity lost, heal, restored)
#   Gate 3  unexpected-death.yml -> FAIL exit 1  (an UNDECLARED container death is caught)
#   Gate 4  faults are SCHEDULED at their vtsc + logged in the JSONL (replayable)
#   Gate 5  a fault referencing an UNKNOWN service -> exit 2, sub-second, BEFORE boot
#
# Usage: scripts/test_faults.sh
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
BIN="$ROOT/target/release/dvmm"
DVMM="${DVMM_FAULT_ARTIFACT:-$ROOT/faultlab.dvmm}"
SDIR="$ROOT/guest/stacks/faultlab"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

[ -x "$BIN" ] || { echo "building dvmm..."; ( cd "$ROOT" && cargo build --release ) || exit 3; }
if [ ! -f "$DVMM" ]; then
  echo "== faultlab.dvmm missing — baking it =="
  "$BIN" build "$SDIR/compose.yml" -o "$DVMM" || {
    echo "FATAL: bake failed" >&2; exit 3; }
fi

PASS=0; FAIL=0
ok()  { echo "  PASS: $*"; PASS=$((PASS+1)); }
bad() { echo "  FAIL: $*"; FAIL=$((FAIL+1)); }

# ---- Gate 1: kill + recover -> exit 0 --------------------------------------
echo "== Gate 1: kill + recover (expect PASS, exit 0) =="
KR="$TMP/kr.jsonl"; KRR="$TMP/kr.report.json"
"$BIN" test "$DVMM" --scenario "$SDIR/kill-recover.yml" --jsonl "$KR" --report "$KRR" \
  --wall-timeout 360 >"$TMP/g1.out" 2>&1
code=$?
sed 's/\r//g' "$TMP/g1.out" | sed -n '/==== dvmm test/,/VERDICT/p' | sed 's/^/    /'
[ "$code" -eq 0 ] && ok "kill+recover exit 0" || bad "expected exit 0, got $code"
grep -q '"verdict": "pass"' "$KRR" 2>/dev/null && ok "report verdict=pass" || bad "report not pass"
# the kill and start faults were applied ok
grep -q '"stdout":"kill db' "$KR" && ok "kill applied" || bad "no kill result"
grep -q '"stdout":"start db' "$KR" && ok "start applied (recovery)" || bad "no start result"

# ---- Gate 2: partition + heal -> exit 0 ------------------------------------
echo "== Gate 2: partition + heal (expect PASS, exit 0) =="
PH="$TMP/ph.jsonl"; PHR="$TMP/ph.report.json"
"$BIN" test "$DVMM" --scenario "$SDIR/partition-heal.yml" --jsonl "$PH" --report "$PHR" \
  --wall-timeout 360 >"$TMP/g2.out" 2>&1
code=$?
sed 's/\r//g' "$TMP/g2.out" | sed -n '/==== dvmm test/,/VERDICT/p' | sed 's/^/    /'
[ "$code" -eq 0 ] && ok "partition+heal exit 0" || bad "expected exit 0, got $code"
grep -q '"verdict": "pass"' "$PHR" 2>/dev/null && ok "report verdict=pass" || bad "report not pass"
grep -q '"stdout":"partition ' "$PH" && ok "partition applied (nft drop rules)" || bad "no partition result"
grep -q '"stdout":"heal ' "$PH" && ok "heal applied (rules removed)" || bad "no heal result"

# ---- Gate 3: an UNEXPECTED death -> exit 1 ---------------------------------
echo "== Gate 3: unexpected death (expect FAIL, exit 1) =="
UD="$TMP/ud.jsonl"; UDR="$TMP/ud.report.json"
"$BIN" test "$DVMM" --scenario "$SDIR/unexpected-death.yml" --jsonl "$UD" --report "$UDR" \
  --wall-timeout 300 >"$TMP/g3.out" 2>&1
code=$?
sed 's/\r//g' "$TMP/g3.out" | grep -aE 'VERDICT|FAILURE' | sed 's/^/    /'
[ "$code" -eq 1 ] && ok "unexpected death -> exit 1" || bad "expected exit 1, got $code"
grep -q '"verdict": "fail"' "$UDR" 2>/dev/null && ok "report verdict=fail" || bad "report not fail"
grep -q 'db=exit137' "$TMP/g3.out" && ok "the SIGKILLed db (exit137) was flagged unexpected" \
  || bad "did not flag the unexpected death"

# ---- Gate 4: faults are SCHEDULED at their vtsc, logged (replayable) --------
echo "== Gate 4: faults are scheduled (vtsc + command + result in the JSONL) =="
# The kill fault is a (vtsc, ScenarioStep) queue entry: it appears in the JSONL as
# a fault command carrying a ts_vtsc, near its scheduled 1h (~3600 virtual s).
python3 - "$KR" <<'PY'
import sys, json
faults=[json.loads(l) for l in open(sys.argv[1]) if '"fault":true' in l]
kill=[f for f in faults if f.get("op")=="kill"]
start=[f for f in faults if f.get("op")=="start"]
def near(f, secs, tol=60): return abs(f["t_s"]-secs) <= tol and f["ts_vtsc"] > 0
ok = bool(kill) and bool(start) and near(kill[0],3600) and near(start[0],7200)
print("    kill  :", kill[0]["t_s"] if kill else None, "vtsc", kill[0]["ts_vtsc"] if kill else None)
print("    start :", start[0]["t_s"] if start else None, "vtsc", start[0]["ts_vtsc"] if start else None)
sys.exit(0 if ok else 1)
PY
[ $? -eq 0 ] && ok "kill@~1h and start@~2h each logged with a ts_vtsc (replayable)" \
  || bad "faults not logged at their scheduled vtsc"

# ---- Gate 5: unknown-service fault -> exit 2, sub-second, BEFORE boot -------
echo "== Gate 5: fault referencing an unknown service (expect exit 2, sub-second) =="
cat > "$TMP/badfault.yml" <<'YML'
steps:
  - at: 1h
    kill: nonexistent-service
YML
t0=$(date +%s.%N)
"$BIN" test "$DVMM" --scenario "$TMP/badfault.yml" >"$TMP/g5.out" 2>&1
code=$?
t1=$(date +%s.%N)
dt=$(awk "BEGIN{printf \"%.2f\", $t1-$t0}")
grep -aoE 'scenario rejected.*' "$TMP/g5.out" | head -1 | sed 's/^/    /'
[ "$code" -eq 2 ] && ok "unknown-service fault -> exit 2 (${dt}s)" || bad "expected exit 2, got $code"
awk "BEGIN{exit !($dt < 1.0)}" && ok "rejected sub-second (${dt}s, before boot)" \
  || bad "took ${dt}s (not sub-second)"

echo
echo "==== TEST-1b fault gates: $PASS passed, $FAIL failed ===="
[ "$FAIL" -eq 0 ]
