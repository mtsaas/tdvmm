#!/usr/bin/env bash
# tdvmm — the DRIVER verdict-contract acceptance gate.
#
# There is no `tdvmm test` verb: a run becomes a test because some container
# called finish() on the control socket. This proves that contract end to end,
# from a real bake and real boots:
#
#   finish(0)                 -> tdvmm run exits 0   (PASS)
#   finish(nonzero)           -> exits 1             (FAIL, collapsed on purpose)
#   fault ops over the socket -> exits 0             (kill/start/reject work)
#   driver never finishes     -> exits 2             (wall-clock safety timeout)
#
# The nonzero case deliberately uses finish(3): 3 is the VMM's own horizon code,
# so this also proves a driver cannot impersonate a VMM outcome.
#
# One bake serves every case; the behavior is picked per run with
# tdvmm.drivermode= on the kernel cmdline.
set -uo pipefail

TDVMM="${TDVMM:-./target/release/tdvmm}"
STACK="${STACK:-testdata/stacks/driverlab/compose.yml}"
OUT="${OUT:-.tdvmm-test-results}"
ART="$OUT/driverlab.tdvmm"
CMDLINE_BASE="console=ttyS0 reboot=t panic=1 pci=off no_timer_check tsc=reliable tdvmm.stack=1"

fail() { echo "FAIL: $*" >&2; exit 1; }

mkdir -p "$OUT"
echo "== bake driverlab =="
"$TDVMM" build driverlab "$STACK" -o "$ART" --no-progress >"$OUT/driverlab-build.log" 2>&1 \
  || { tail -30 "$OUT/driverlab-build.log"; fail "bake failed"; }

# run <mode> <expected-exit> <wall-timeout> [extra grep assertion]
run_case() {
  local mode="$1" want="$2" wall="$3" needle="${4:-}"
  local log="$OUT/driverlab-$mode.log"
  echo "== run mode=$mode (expect exit $want) =="
  "$TDVMM" run "$ART" --wall-timeout "$wall" \
      --cmdline "$CMDLINE_BASE tdvmm.drivermode=$mode" >"$log" 2>&1
  local got=$?
  [ "$got" = "$want" ] || { tail -25 "$log"; fail "mode=$mode exited $got, expected $want"; }
  if [ -n "$needle" ] && ! grep -qF "$needle" "$log"; then
    tail -25 "$log"; fail "mode=$mode: expected output to contain '$needle'"
  fi
  echo "   ok (exit $got)"
}

run_case pass   0 300 "==== tdvmm driver: PASS (finish 0)"
run_case fail   1 300 "==== tdvmm driver: FAIL (finish 3)"
run_case faults 0 300 "driverlab faults ok"
# The driver never calls finish(); only the wall clock can end this one.
run_case hang   2 60  "WALL-CLOCK TIMEOUT"

# A run with NO driver at all must be untouched: no verdict, no summary line.
echo "== run with no driver (the spinner corpus stack) =="
SPIN="$OUT/spinner.tdvmm"
"$TDVMM" build spinner testdata/stacks/spinner/compose.yml -o "$SPIN" --no-progress \
  >"$OUT/spinner-build.log" 2>&1 || { tail -20 "$OUT/spinner-build.log"; fail "spinner bake failed"; }
"$TDVMM" run "$SPIN" --max-virtual-time 30s >"$OUT/spinner-run.log" 2>&1
got=$?
[ "$got" = "3" ] || { tail -20 "$OUT/spinner-run.log"; fail "undriven run exited $got, expected 3 (horizon)"; }
grep -q "tdvmm driver:" "$OUT/spinner-run.log" && fail "an undriven run must not print a driver summary"
echo "   ok (exit 3, no driver summary)"

echo
echo "PASS: the driver verdict contract holds (0 / 1 / 2, and undriven runs are unchanged)"
