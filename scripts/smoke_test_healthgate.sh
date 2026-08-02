#!/usr/bin/env bash
# Phase-2b (item 3) HEALTHCHECK-GATING acceptance test.
#
# Boots the health-gate stack under fast-forward and proves the guest-side
# healthcheck ticker resolves compose's depends_on: {condition: service_healthy}
# gate, i.e. the dependent starts ONLY AFTER the gate is HEALTHY:
#
#   (a) the ticker flips the gate's health starting -> healthy (podman has no
#       systemd auto-runner, so nothing else could), and
#   (b) compose's own ordered lifecycle stream shows the gate reach "Healthy"
#       BEFORE the dependent reaches "Started" (single reliable stream), and the
#       process census confirms the dependent is running, and
#   (c) it all runs under fast-forward (the gate's 30 s readiness sleep is a
#       virtual sleep FF collapses; the FF summary reports the speedup + the
#       per-hop <=500us VMM gate).
#
# Exits 0 only if every property holds.
#
# Usage: scripts/smoke_test_healthgate.sh [wall_timeout_s]
# Env:   MEM (3072)  MAX_VIRTUAL_TIME (120s)  HC_TICK (2)
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
BIN="$ROOT/target/release/dvmm"
KERNEL="$ROOT/guest/kernel/vmlinux-6.1.128"
INITRD="${INITRD:-$ROOT/guest/initramfs-alpine/initramfs-alpine-health-gate.cpio.gz}"

WALL_TIMEOUT="${1:-150}"
MEM="${MEM:-3072}"
HC_TICK="${HC_TICK:-2}"
# Virtual-time horizon: the stack loops forever, so bound its virtual duration in
# the VMM (clean stop -> FF summary). 120 s comfortably clears the 30 s gate
# readiness + the health flip while staying a seconds-long real run.
MAX_VIRTUAL_TIME="${MAX_VIRTUAL_TIME:-120s}"

GATE=dvmm_health-gate-gate-1
DEP=dvmm_health-gate-dependent-1

[ -x "$BIN" ] || { echo "[hg] building..."; ( cd "$ROOT" && cargo build --release ) || exit 3; }
[ -f "$KERNEL" ] && [ -f "$INITRD" ] || { echo "SMOKE FAIL: missing artifacts (bake health-gate?)"; exit 3; }

LOG="$(mktemp)"; trap 'rm -f "$LOG"' EXIT
CMDLINE="console=ttyS0 reboot=t panic=1 pci=off no_timer_check tsc=reliable dvmm.stack=1 dvmm.hc_tick=$HC_TICK"
echo "[hg] boot: mem=${MEM}MiB ff=ON hc_tick=${HC_TICK}s horizon=${MAX_VIRTUAL_TIME} wall_timeout=${WALL_TIMEOUT}s"
START=$(date +%s.%N)
timeout "$WALL_TIMEOUT" "$BIN" boot --kernel "$KERNEL" --initrd "$INITRD" --mem "$MEM" --ff on \
  --max-virtual-time "$MAX_VIRTUAL_TIME" --cmdline "$CMDLINE" </dev/null >"$LOG" 2>&1
rc=$?
WALL=$(awk "BEGIN{printf \"%.1f\", $(date +%s.%N)-$START}")
echo "[hg] vmm exit=$rc (3=horizon, expected) wall=${WALL}s"

echo
echo "==== compose ordered lifecycle (single reliable stream) ===="
grep -E "\[stack\]\[up\].*(${GATE}|${DEP}).*(Started|Waiting|Healthy)" "$LOG" | sed 's/^/  /'
echo "==== healthcheck ticker (starting -> healthy) ===="
grep -E 'DVMM_HC_TICKER_START|DVMM_HC_RUN|DVMM_HC_HEALTHY' "$LOG" | sed -n '1,6p;$p' | sed 's/^/  /'
echo "==== process census ===="
grep -E "\[census\] (${GATE}|${DEP})" "$LOG" | sed 's/^/  /'
echo "==== fast-forward summary ===="
grep -E 'FAST-FORWARD SUMMARY' "$LOG" | sed 's/^/  /'
echo "============================================================"

ok=1
note() { echo "  $*"; }

# (a) the ticker resolved health (nothing else could -- no systemd).
grep -q 'DVMM_HC_HEALTHY' "$LOG" || { echo "ASSERT FAIL: ticker never reported the gate healthy"; ok=0; }
# the gate was "starting" before it was "healthy" (a real transition, not born healthy).
first_starting=$(grep -nE "DVMM_HC_RUN container=${GATE} status=starting" "$LOG" | head -1 | cut -d: -f1)
first_hc_healthy=$(grep -nE "DVMM_HC_RUN container=${GATE} status=healthy" "$LOG" | head -1 | cut -d: -f1)
if [ -n "$first_starting" ] && [ -n "$first_hc_healthy" ] && [ "$first_starting" -lt "$first_hc_healthy" ]; then
  note "ticker transition: status=starting (line $first_starting) -> status=healthy (line $first_hc_healthy) OK"
else
  echo "ASSERT FAIL: no starting->healthy transition from the ticker"; ok=0
fi

# (b) THE GATE: dependent Started only AFTER gate Healthy (compose's own stream).
healthy_ln=$(grep -nE "\[stack\]\[up\].*${GATE} Healthy" "$LOG" | head -1 | cut -d: -f1)
dep_started_ln=$(grep -nE "\[stack\]\[up\].*${DEP} (Starting|Started)" "$LOG" | head -1 | cut -d: -f1)
if [ -n "$healthy_ln" ] && [ -n "$dep_started_ln" ] && [ "$healthy_ln" -lt "$dep_started_ln" ]; then
  note "ORDERING OK: gate 'Healthy' (line $healthy_ln) precedes dependent start (line $dep_started_ln)"
else
  echo "ASSERT FAIL: dependent did not start strictly after gate became healthy (healthy=$healthy_ln dep=$dep_started_ln)"; ok=0
fi
# census confirms the dependent actually came up.
grep -qE "\[census\] ${DEP} Up" "$LOG" || { echo "ASSERT FAIL: dependent not Up in the process census"; ok=0; }

# (c) ran under fast-forward + per-hop <=500us VMM gate.
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
  echo "HEALTHGATE SMOKE PASS: ticker flipped the gate healthy; dependent started only after; ran under FF (${speed}x)."
  exit 0
fi
echo "HEALTHGATE SMOKE FAIL (see above)."
echo "---- last serial output ----"; tail -30 "$LOG"
exit 1
