#!/usr/bin/env bash
# Step 4 repeatability smoke (gate 6): run the fast-forward headline demo TWICE
# from identical artifacts and require both green with IDENTICAL row-count +
# trim behavior (the emitted TDVMM_DEMO_SEQ must match byte-for-byte).
#
# This is a repeatability check, NOT a determinism claim (jump *timing* varies
# run-to-run; the guest-visible row sequence does not).
#
# Usage: scripts/ff_repeat.sh [target_virtual_hours] [wall_timeout_s]
# Env passes through to ff_demo.sh (INTERVAL, MAX_ROWS, MEM, MAX_JUMP_SECS).
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
TARGET="${1:-24}"; TIMEOUT="${2:-300}"

# Runs the demo; streams the transcript to stderr and prints ONLY the canonical
# TDVMM_DEMO_SEQ line on stdout (so the caller compares sequences, not transcripts
# whose jump-timing numbers legitimately vary run-to-run).
run() {
  local tag="$1" out
  out="$("$HERE/ff_demo.sh" "$TARGET" "$TIMEOUT" 2>&1)"
  echo "$out" | sed "s/^/[$tag] /" >&2
  echo "$out" | grep -qE '^FF DEMO PASS' || { echo "REPEAT FAIL: run $tag did not pass" >&2; return 1; }
  echo "$out" | grep -oE 'TDVMM_DEMO_SEQ=.*' | head -1
}

echo "==================== REPEATABILITY RUN 1 ===================="
SEQ1="$(run run1)" || exit 1
echo "==================== REPEATABILITY RUN 2 ===================="
SEQ2="$(run run2)" || exit 1

echo
echo "run1 $SEQ1"
echo "run2 $SEQ2"
if [ "$SEQ1" = "$SEQ2" ] && [ -n "$SEQ1" ]; then
  echo "REPEAT PASS: both runs green with identical row-count + trim behavior."
  exit 0
fi
echo "REPEAT FAIL: row sequences differ between runs."
exit 1
