#!/usr/bin/env bash
# Interactive quit-path regression test (Step 4 + interactive-defaults).
#
# The bug class this guards against was "the interactive cmdline was never
# tested". At the serial shell (PID 1's child under busybox `init`), the three
# ways a human leaves the guest must each do the right thing:
#
#   * `exit`     -> busybox init RESPAWNS the shell: a FRESH PROMPT, and the VMM
#                   KEEPS RUNNING. (A plain shell as PID 1 would panic the kernel
#                   here — "Attempted to kill init!".)
#   * `reboot`   -> reboot(RB_AUTOBOOT). With `reboot=t` on the cmdline this is a
#                   triple fault -> KVM_EXIT_SHUTDOWN -> the VMM stops cleanly (0).
#                   (`reboot=k`, the i8042 keyboard-controller reset, never
#                   completes — the VMM does not emulate an i8042 — so the guest
#                   wedges in a halt/re-arm loop that fast-forward advances
#                   forever; case D shows the horizon bounding exactly that.)
#   * `poweroff` -> reboot(RB_POWER_OFF). With no ACPI the kernel finishes in a
#                   HLT with interrupts disabled (IF=0), "System halted" — a
#                   terminal halt that can never wake. The VMM recognizes it as a
#                   clean "guest halted (power off)" stop (0).
#
# It also asserts the always-printed startup MODE LINE is present (so the
# documented fast-forward default is mechanically checked), and case B asserts
# the default reads "ON (default)".
#
# Cases (busybox guest, `exit`/`reboot`/`poweroff` sent over the serial console):
#   A. `exit` RESPAWNS: a fresh shell runs a marker command after `exit` (proves
#      the VMM kept running and gave a fresh prompt). Default cmdline + default FF.
#   B. `reboot` -> clean self-exit (0), "guest-initiated shutdown". Default FF, so
#      the mode line reads "fast-forward: ON (default)" (the documented default).
#   C. `poweroff` -> clean self-exit (0), "guest halted (power off)" (IF=0 HLT).
#   D. reboot=k + --max-virtual-time: the horizon cleanly bounds the wedge — the
#      VMM exits with the distinct horizon status (3), not a spin.
#
# Exits 0 only if A respawns, B exits 0, C exits 0 (power-off halt), and D stops
# at the horizon (3) — and the mode line is present throughout.
#
# Usage: scripts/smoke_test_interactive_exit.sh [self_exit_deadline_s]
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
BIN="$ROOT/target/release/tdvmm"
KERNEL="${KERNEL:-$ROOT/guest/kernel/vmlinux-6.1.128}"
INITRD="${INITRD:-$ROOT/guest/initramfs/initramfs.cpio.gz}"
MEM="${MEM:-256}"

# Seconds to allow the VMM to terminate on its own after the quit command is
# sent. A wedge (reboot=k without a horizon) never exits, so it is detected as
# "still alive at the deadline". Kept small because a clean stop happens in ~1-4s.
DEADLINE="${1:-20}"
BOOT_MARKER="TDVMM_BOOT_OK"
RESPAWN_MARKER="TDVMM_RESPAWN_OK"
MODE_LINE_RE='\[tdvmm\] fast-forward:'
# Total wall budget per case = boot allowance + self-exit deadline.
BOOT_ALLOWANCE="${BOOT_ALLOWANCE:-30}"
TOTAL=$(( BOOT_ALLOWANCE + DEADLINE ))

[ -f "$KERNEL" ] || { echo "REGRESSION FAIL: kernel not found: $KERNEL"; exit 3; }
[ -f "$INITRD" ] || { echo "REGRESSION FAIL: initrd not found: $INITRD"; exit 3; }
if [ ! -x "$BIN" ]; then
  echo "building release binary..."
  ( cd "$ROOT" && cargo build --release ) || { echo "REGRESSION FAIL: build error"; exit 3; }
fi

# run_case boots the guest, waits for the boot marker, feeds $SEND (interpreted
# with printf %b, so use \n for newlines) over the serial console, and records
# how the VMM terminated. Extra args to the VMM are passed as "$@". Sets globals:
#   RUN_RC   — the VMM exit code, or 124 if it had to be killed at the deadline
#   RUN_WALL — wall seconds the VMM ran
#   RUN_LOG  — path to the captured serial log (caller inspects, then removes)
run_case() {
  local log fifo feeder start end rc
  log="$(mktemp)"; fifo="$(mktemp -u)"; mkfifo "$fifo"

  # Feeder: wait for the boot marker, send $SEND, then hold the write end open
  # past the deadline so the VMM never sees an early stdin EOF.
  (
    for _ in $(seq 1 $(( BOOT_ALLOWANCE * 10 ))); do
      grep -q "$BOOT_MARKER" "$log" 2>/dev/null && break
      sleep 0.1
    done
    sleep 1
    printf '%b' "$SEND"
    sleep $(( TOTAL + 5 ))
  ) > "$fifo" &
  feeder=$!

  start=$(date +%s)
  # `timeout` returns the VMM's own exit code if it self-exits before TOTAL, or
  # 124 if it had to be killed (a wedge / still-running).
  timeout "$TOTAL" "$BIN" boot --kernel "$KERNEL" --initrd "$INITRD" --mem "$MEM" "$@" \
    < "$fifo" > "$log" 2>&1
  rc=$?
  end=$(date +%s)

  kill "$feeder" 2>/dev/null
  RUN_RC="$rc"; RUN_WALL=$(( end - start )); RUN_LOG="$log"
  rm -f "$fifo"
}

# assert_mode_line: every run must print the startup mode line (mechanically
# checks that the documented default is stated). Bumps `ok=0` on failure.
assert_mode_line() {
  if grep -qE "$MODE_LINE_RE" "$RUN_LOG"; then
    grep -m1 -E "$MODE_LINE_RE" "$RUN_LOG" | sed 's/^/    mode line: /'
  else
    echo "    FAIL: startup mode line (\"$MODE_LINE_RE\") missing"
    ok=0
  fi
}

ok=1

# ---- Case A: `exit` RESPAWNS the shell (VMM keeps running) --------------------
echo "== Case A: \`exit\` respawns the shell (fresh prompt; VMM keeps running) =="
# After `exit`, a fresh shell must run and echo the respawn marker. Terminate the
# case with `poweroff` afterwards so it does not have to wait out the deadline.
# The respawn marker is the discriminator: a VMM killed by `exit` could not echo.
SEND="exit\nsleep 1; echo $RESPAWN_MARKER\nsleep 1; poweroff\n"
run_case
echo "  VMM exit code=$RUN_RC after ${RUN_WALL}s"
if grep -q "$RESPAWN_MARKER" "$RUN_LOG"; then
  echo "  PASS: \`exit\` yielded a fresh prompt — the respawned shell ran a command."
  grep -m1 "$RESPAWN_MARKER" "$RUN_LOG" | sed 's/^/    /'
else
  echo "  FAIL: no fresh prompt after \`exit\` (respawn marker absent) — PID 1 died."
  tail -6 "$RUN_LOG" | sed 's/^/    /'
  ok=0
fi
assert_mode_line
rm -f "$RUN_LOG"
echo

# ---- Case B: `reboot` -> clean self-exit (0) ---------------------------------
echo "== Case B: \`reboot\` -> clean self-exit (default FF; mode line ON (default)) =="
SEND="reboot\n"
run_case
echo "  VMM exit code=$RUN_RC after ${RUN_WALL}s (want 0 = guest-initiated stop)"
if [ "$RUN_RC" -eq 0 ] && grep -q 'guest-initiated shutdown/reboot' "$RUN_LOG"; then
  echo "  PASS: VMM exited on its own after \`reboot\` (triple fault -> KVM_EXIT_SHUTDOWN)."
  grep -m1 'STOP: guest-initiated' "$RUN_LOG" | sed 's/^/    /'
else
  echo "  FAIL: VMM did not cleanly self-exit on \`reboot\`."
  tail -6 "$RUN_LOG" | sed 's/^/    /'
  ok=0
fi
assert_mode_line
# The default (no --ff) must state the documented default explicitly.
if grep -q 'fast-forward: ON (default)' "$RUN_LOG"; then
  echo "    PASS: mode line states the documented binary default (ON (default))."
else
  echo "    FAIL: expected 'fast-forward: ON (default)' with no --ff override."
  ok=0
fi
rm -f "$RUN_LOG"
echo

# ---- Case C: `poweroff` -> clean self-exit (0), IF=0 terminal halt -----------
echo "== Case C: \`poweroff\` -> clean self-exit, 'guest halted (power off)' =="
SEND="poweroff\n"
run_case --ff off
echo "  VMM exit code=$RUN_RC after ${RUN_WALL}s (want 0 = guest-initiated stop)"
if [ "$RUN_RC" -eq 0 ] && grep -q 'guest halted (power off)' "$RUN_LOG"; then
  echo "  PASS: VMM exited on its own after \`poweroff\` (IF=0 HLT terminal halt)."
  grep -m1 'guest halted (power off)' "$RUN_LOG" | sed 's/^/    /'
else
  echo "  FAIL: VMM did not cleanly self-exit with a power-off halt on \`poweroff\`."
  tail -6 "$RUN_LOG" | sed 's/^/    /'
  ok=0
fi
assert_mode_line
rm -f "$RUN_LOG"
echo

# ---- Case D: reboot=k + --max-virtual-time -> horizon bounds the wedge (3) ---
echo "== Case D: reboot=k + --max-virtual-time 60s (horizon bounds the wedge) =="
# reboot=k never completes (no i8042), so the guest wedges; fast-forward (default
# ON) advances it to the horizon in seconds of real time. The horizon must stop
# the run with its distinct status (3), not spin forever.
SEND="reboot\n"
run_case --max-virtual-time 60s \
  --cmdline "console=ttyS0 reboot=k panic=1 pci=off no_timer_check tsc=reliable"
echo "  VMM exit code=$RUN_RC after ${RUN_WALL}s (want 3 = horizon stop, NOT 124 = spin)"
if [ "$RUN_RC" -eq 3 ] && grep -q 'max-virtual-time horizon reached' "$RUN_LOG"; then
  echo "  PASS: the horizon cleanly terminated the reboot=k wedge instead of spinning."
  grep -m1 'HORIZON DIAGNOSTIC' "$RUN_LOG" | sed 's/^/    /'
else
  echo "  FAIL: expected a horizon stop (3) but VMM exited with $RUN_RC."
  tail -8 "$RUN_LOG" | sed 's/^/    /'
  ok=0
fi
assert_mode_line
rm -f "$RUN_LOG"
echo

if [ "$ok" -eq 1 ]; then
  echo "REGRESSION PASS: exit respawns; reboot self-exits (0); poweroff halts clean (0); horizon bounds the wedge (3); mode line present."
  exit 0
fi
echo "REGRESSION FAIL: see cases above."
exit 1
