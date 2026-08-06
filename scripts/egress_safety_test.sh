#!/usr/bin/env bash
# tdvmm --allow-egress SAFETY SUITE (design §5). Proves the phase gate is real:
# the clock never jumps while external state is open, fast-forward re-engages once
# it drains, a jump against an open session aborts, an adversarial dribble is not
# fast-forwarded past the guest's read timeout, the transport is correct under
# --ff off, and the closed world is byte-for-byte unchanged when the flag is off.
#
# CI doctrine: "external" = outside the VM; every endpoint is a test-owned loopback
# server (tdvmm __egress-test-server) on an EPHEMERAL 127.0.0.1 port. NO internet.
#
#   T1  clock does NOT jump while a request is in flight            (--ff on)
#   T2  fast-forward re-engages once the session drains             (--ff on)
#   T3b guest-driven negative control: a jump breach ABORTS the run (unsafe-jumps)
#   T3c adversarial dribble is not fast-forwarded past the timeout  (--ff on)
#   T5  egress transport works under --ff off                       (control)
#   T6  closed-world identity with the flag OFF (no egress metrics)
#
# Verdict per test is the scenario exit code + the guest `always` event's ok bit;
# the numeric EVIDENCE (virtual-elapsed, byte counts) rides that same fast event
# path into the JSONL run-log (the container's serial stdout is async and would
# race the run-ending `done`). Host-side accounting comes from --metrics-out.
#
# Usage: scripts/egress_safety_test.sh
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
BIN="$ROOT/target/release/tdvmm"
OUTDIR="${TDVMM_OUT_DIR:-$ROOT/.tdvmm-test-results}"; mkdir -p "$OUTDIR"
TDVMM="${TDVMM_EGRESS_ARTIFACT:-$OUTDIR/egress-probe.tdvmm}"
SDIR="$ROOT/testdata/stacks/egress-probe"
BASE="console=ttyS0 reboot=t panic=1 pci=off no_timer_check tsc=reliable tdvmm.stack=1"
TMP="$(mktemp -d)"
SRV_PID=""
cleanup() { [ -n "$SRV_PID" ] && kill "$SRV_PID" 2>/dev/null; rm -rf "$TMP"; }
trap cleanup EXIT

[ -x "$BIN" ] || { echo "building tdvmm..."; ( cd "$ROOT" && cargo build --release ) || exit 3; }
if [ ! -f "$TDVMM" ]; then
  echo "== egress-probe.tdvmm missing — baking it =="
  "$BIN" build egress-probe "$SDIR/compose.yml" -o "$TDVMM" || { echo "FATAL: bake failed" >&2; exit 3; }
fi

PASS=0; FAIL=0
ok()  { echo "  PASS: $*"; PASS=$((PASS+1)); }
bad() { echo "  FAIL: $*"; FAIL=$((FAIL+1)); }

# Launch a test server; sets global PORT (its ephemeral loopback port) and SRV_PID.
start_server() {
  : > "$TMP/srv.log"
  "$BIN" __egress-test-server "$@" >"$TMP/srv.log" 2>&1 &
  SRV_PID=$!
  PORT=""
  for _ in $(seq 1 50); do
    PORT="$(sed -n 's/^EGRESS_TEST_SERVER_PORT=//p' "$TMP/srv.log")"
    [ -n "$PORT" ] && break
    sleep 0.1
  done
  [ -n "$PORT" ] || { echo "FATAL: test server never announced its port"; cat "$TMP/srv.log"; exit 3; }
}
stop_server() { [ -n "$SRV_PID" ] && kill "$SRV_PID" 2>/dev/null; SRV_PID=""; }

metric()  { awk -v k="$2" '$1==k{print $2; exit}' "$1" 2>/dev/null; }              # metric <file> <key>
egress_keys() { awk '/^egress_/{c++} END{print c+0}' "$1" 2>/dev/null; }           # count of egress_* lines
ev_ok()   { grep -aq "\"name\":\"$2\",\"ok\":true" "$1"; }                          # ev_ok <jsonl> <name>
ev_num()  { grep -a "\"name\":\"$2\"" "$1" 2>/dev/null | grep -oE "\"$3\":[0-9.]+" | head -1 | cut -d: -f2; }
fcmp()    { awk "BEGIN{exit !($1)}"; }                                              # truth of a float expr

# run_test <name> <scenario> <ff on|off> <cmdline> [extra tdvmm args...]
# Runs `tdvmm test`, writing <name>.jsonl / <name>.metrics / <name>.out under TMP.
run_test() {
  local name="$1" scn="$2" ff="$3" cmd="$4"; shift 4
  "$BIN" test "$TDVMM" --scenario "$SDIR/$scn" --ff "$ff" --cmdline "$cmd" \
    --jsonl "$TMP/$name.jsonl" --metrics-out "$TMP/$name.metrics" \
    "$@" </dev/null >"$TMP/$name.out" 2>&1
}

# ---- T1: the clock does NOT jump while a request is in flight ----------------
echo "== T1: clock does not jump while a request is in flight (--ff on) =="
start_server delay-then-respond 2
CMD="$BASE tdvmm.egress_mode=t1 tdvmm.egress_d=2 tdvmm.egress_url=http://127.0.0.1:$PORT/"
run_test t1 egress-t1.yml on "$CMD" --allow-egress --wall-timeout 120
code=$?
stop_server
VE="$(ev_num "$TMP/t1.jsonl" t1_no_jump virtual_elapsed)"
GATED="$(metric "$TMP/t1.metrics" egress_gated_real_secs)"
echo "    evidence: virtual_elapsed=${VE}s (server held 2s real)  egress_gated_real_secs=${GATED}s"
[ "$code" -eq 0 ] && ok "T1 exit 0 (verdict pass)" || bad "T1 expected exit 0, got $code"
ev_ok "$TMP/t1.jsonl" t1_no_jump && ok "T1 in-guest verdict: no jump (D <= ve <= D+2)" || bad "T1 in-guest verdict failed"
if [ -n "$VE" ] && fcmp "$VE>=1.5 && $VE<=4.5"; then ok "T1 virtual_elapsed ${VE}s tracks the 2s REAL hold (a jump would be orders larger)"; else bad "T1 virtual_elapsed=${VE}s out of band"; fi
if [ -n "$GATED" ] && fcmp "$GATED>=1.5"; then ok "T1 host-side egress_gated_real_secs=${GATED}s >= ~D"; else bad "T1 egress_gated_real_secs=${GATED}s too small"; fi

# ---- T2: fast-forward re-engages once the session drains ---------------------
echo "== T2: fast-forward re-engages once the session drains (--ff on) =="
start_server delay-then-respond 0
CMD="$BASE tdvmm.egress_mode=t2 tdvmm.egress_sleep=600 tdvmm.egress_url=http://127.0.0.1:$PORT/"
run_test t2 egress-t2.yml on "$CMD" --allow-egress --wall-timeout 120
code=$?
stop_server
JUMPS="$(metric "$TMP/t2.metrics" jumps)"; VS="$(metric "$TMP/t2.metrics" virtual_secs)"; RS="$(metric "$TMP/t2.metrics" real_secs)"
echo "    evidence: jumps=$JUMPS virtual_secs=$VS real_secs=$RS (600 virtual s of sleep after the request)"
[ "$code" -eq 0 ] && ok "T2 exit 0 (verdict pass)" || bad "T2 expected exit 0, got $code"
ev_ok "$TMP/t2.jsonl" t2_ff_reengaged && ok "T2 in-guest verdict: the 600 virtual s elapsed" || bad "T2 in-guest verdict failed"
if [ -n "$JUMPS" ] && [ "$JUMPS" -gt 0 ] 2>/dev/null; then ok "T2 jumps=$JUMPS > 0 (FF active)"; else bad "T2 jumps=$JUMPS (FF never re-engaged)"; fi
if [ -n "$VS" ] && fcmp "$VS>=600"; then ok "T2 virtual_secs=$VS >= 600"; else bad "T2 virtual_secs=$VS < 600"; fi
if [ -n "$RS" ] && fcmp "$RS<=60"; then ok "T2 real_secs=$RS <= 60 (600 virtual s collapsed to seconds)"; else bad "T2 real_secs=$RS > 60 (FF did not re-engage)"; fi

# ---- T3b: guest-driven negative control -- a gate breach ABORTS the run ------
echo "== T3b: a jump against an open session aborts (TDVMM_EGRESS_UNSAFE_JUMPS=1) =="
start_server hold-open 25
CMD="$BASE tdvmm.egress_mode=t3b tdvmm.egress_url=http://127.0.0.1:$PORT/"
TDVMM_EGRESS_UNSAFE_JUMPS=1 "$BIN" test "$TDVMM" --scenario "$SDIR/egress-t3b.yml" --allow-egress --ff on \
  --cmdline "$CMD" --wall-timeout 75 </dev/null >"$TMP/t3b.out" 2>&1
code=$?
stop_server
echo "    evidence: exit=$code; $(grep -aoE 'egress gate breached[^)]*\)' "$TMP/t3b.out" | head -1)"
[ "$code" -ne 0 ] && ok "T3b run aborted (nonzero exit $code)" || bad "T3b did NOT abort (exit 0) — the tripwire is dead"
grep -aq 'egress gate breached' "$TMP/t3b.out" && ok "T3b aborted with 'egress gate breached'" || bad "T3b abort message missing"

# ---- T3c: adversarial dribble is not fast-forwarded past the read timeout ----
echo "== T3c: adversarial dribble (1 byte / 500ms real for 3s), curl --max-time 30 (--ff on) =="
start_server dribble 6 500
CMD="$BASE tdvmm.egress_mode=t3c tdvmm.egress_maxtime=30 tdvmm.egress_url=http://127.0.0.1:$PORT/"
run_test t3c egress-t3c.yml on "$CMD" --allow-egress --wall-timeout 90
code=$?
stop_server
VE="$(ev_num "$TMP/t3c.jsonl" t3c_dribble_completed virtual_elapsed)"
BYTES="$(ev_num "$TMP/t3c.jsonl" t3c_dribble_completed bytes)"
echo "    evidence: bytes=${BYTES}/6 virtual_elapsed=${VE}s (dribble spans ~3s real; FF of the gaps would blow past the 30s virtual timeout)"
[ "$code" -eq 0 ] && ok "T3c exit 0 (verdict pass)" || bad "T3c expected exit 0, got $code"
ev_ok "$TMP/t3c.jsonl" t3c_dribble_completed && ok "T3c in-guest verdict: read completed (6/6 within the timeout)" || bad "T3c dribble read did not complete"
if [ -n "$VE" ] && fcmp "$VE>=2 && $VE<=15"; then ok "T3c virtual_elapsed=${VE}s tracks the ~3s real dribble (gaps NOT fast-forwarded)"; else bad "T3c virtual_elapsed=${VE}s out of band"; fi

# ---- T5: transport works under --ff off (control) ---------------------------
echo "== T5: egress transport under --ff off (transport correctness, no gating) =="
start_server delay-then-respond 0
CMD="$BASE tdvmm.egress_mode=t5 tdvmm.egress_url=http://127.0.0.1:$PORT/"
run_test t5 egress-t5.yml off "$CMD" --allow-egress --wall-timeout 90
code=$?
stop_server
echo "    evidence: $(grep -aoE 'TDVMM_EGRESS_T5 rc=[0-9]+ http=[0-9]+ body=[A-Z]*' "$TMP/t5.out" | head -1)"
[ "$code" -eq 0 ] && ok "T5 exit 0 (verdict pass)" || bad "T5 expected exit 0, got $code"
ev_ok "$TMP/t5.jsonl" t5_transport_ok && ok "T5 full response (EGRESSOK) returned through the mux" || bad "T5 transport failed"

# ---- T6: closed-world identity with the flag OFF ----------------------------
echo "== T6: closed-world identity with --allow-egress OFF (no egress metrics) =="
# No server + no --allow-egress: the forwarder is not started, so the proxy connect
# must fail. `--cmdline` never adds tdvmm.egress=1 here, so the closed world holds.
CMD="$BASE tdvmm.egress_mode=t6 tdvmm.egress_url=http://127.0.0.1:1/"
run_test t6 egress-t6.yml on "$CMD" --wall-timeout 90
code=$?
KEYS="$(egress_keys "$TMP/t6.metrics")"
SCHEMA="$(metric "$TMP/t6.metrics" schema)"
echo "    evidence: exit=$code metrics schema=$SCHEMA egress_* keys=$KEYS"
[ "$code" -eq 0 ] && ok "T6 exit 0 (verdict pass)" || bad "T6 expected exit 0, got $code"
ev_ok "$TMP/t6.jsonl" t6_closed_world && ok "T6 proxy connect failed fast (closed world)" || bad "T6 closed-world probe unexpected"
grep -aq 'TDVMM_CLOSED_WORLD_OK' "$TMP/t6.out" && ok "T6 TDVMM_CLOSED_WORLD_OK still printed" || bad "T6 closed-world marker missing"
[ "${KEYS:-0}" -eq 0 ] && ok "T6 no egress_* metrics (schema $SCHEMA — INV-E0 identity)" || bad "T6 leaked $KEYS egress_* metrics with the flag off"

echo
echo "==== egress safety suite: $PASS passed, $FAIL failed ===="
[ "$FAIL" -eq 0 ]
