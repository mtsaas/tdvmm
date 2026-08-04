#!/usr/bin/env bash
# deterministic-vmm — Phase-C guest-assertion bridge, end-to-end gate.
#
# Proves the full path: a workload container writes a JSON event to the guest
# FIFO -> the agent forwards it over ttyS1 -> the host records a vtsc-stamped
# `guest_event` in the JSONL and folds it into the 0/1/2 verdict.
#
#   - a satisfied `sometimes` + `done`  -> PASS, exit 0, guest_event recorded
#   - an `always` ok:false + `done`     -> FAIL, exit 1
#
# Usage: scripts/test_events.sh
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
BIN="$ROOT/target/release/dvmm"
PASS_SCN="$ROOT/guest/stacks/insert-trim/events.yml"
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT

[ -x "$BIN" ] || { echo "building dvmm..."; ( cd "$ROOT" && cargo build --release ) || exit 3; }

# Bake insert-trim into the store under its canonical name (the bake rebuilds the
# agent with the poll/FIFO loop and embeds the FIFO volume). A canonical name keeps
# the committed lock ledger clean; the run then resolves the stack by name.
echo "== bake insert-trim (embeds the bridge) =="
"$BIN" build "$ROOT/guest/stacks/insert-trim/compose.yml" || { echo "FATAL: bake failed" >&2; exit 3; }

PASS=0; FAIL=0
ok()  { echo "  PASS: $*"; PASS=$((PASS+1)); }
bad() { echo "  FAIL: $*"; FAIL=$((FAIL+1)); }

# ---- Gate 1: satisfied assertion + done -> PASS, exit 0 --------------------
echo "== Gate 1: bridge round-trip (expect PASS, exit 0) =="
J1="$TMP/pass.jsonl"; R1="$TMP/pass.report.json"
"$BIN" test insert-trim --scenario "$PASS_SCN" --jsonl "$J1" --report "$R1" \
  --wall-timeout 300 >"$TMP/g1.out" 2>&1
code=$?
[ "$code" -eq 0 ] && ok "exit 0" || bad "expected exit 0, got $code"
grep -q '"type":"guest_event"' "$J1" && ok "guest_event recorded in JSONL" || bad "no guest_event in JSONL"
grep -q '"verdict": "pass"' "$R1" && ok "report verdict=pass" || bad "report not pass"
grep -q '"assertions"' "$R1" && ok "assertions summary in report" || bad "no assertions summary"
echo "  --- guest_event lines ---"; grep '"type":"guest_event"' "$J1" | sed 's/^/    /'

# ---- Gate 2: always ok:false + done -> FAIL, exit 1 -----------------------
echo "== Gate 2: failing assertion (expect FAIL, exit 1) =="
cat > "$TMP/fail.yml" <<'YML'
name: bridge-roundtrip-fail
run:
  cmdline: "console=ttyS0 reboot=t panic=1 pci=off no_timer_check tsc=reliable dvmm.stack=1 dvmm.interval=3600 dvmm.maxrows=5 dvmm.hc_tick=2"
  until: done
steps:
  - at: 0s
    wait_for:
      probe: { exec: { container: service, cmd: "true" } }
      until: exit_zero
      every: 10s
      timeout: 5m
  - name: emit-failing
    at: 0s
    exec:
      container: service
      cmd: |
        echo '{"kind":"always","name":"inv","ok":false}' > /run/dvmm/events
        echo '{"kind":"done"}' > /run/dvmm/events
    expect: { exit: 0 }
YML
J2="$TMP/fail.jsonl"; R2="$TMP/fail.report.json"
"$BIN" test insert-trim --scenario "$TMP/fail.yml" --jsonl "$J2" --report "$R2" \
  --wall-timeout 300 >"$TMP/g2.out" 2>&1
code=$?
[ "$code" -eq 1 ] && ok "exit 1 (assertion failure)" || bad "expected exit 1, got $code"
grep -q '"verdict": "fail"' "$R2" && ok "report verdict=fail" || bad "report not fail"

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" -eq 0 ] || exit 1
