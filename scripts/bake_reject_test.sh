#!/usr/bin/env bash
# Phase-2a rejection tests: prove the bake-time boundary is LOUD.
#
# Each corpus compose file exercises exactly one thing outside the supported
# subset; bake-stack's validator (bake_compose.py, the same gate bake-stack runs
# FIRST, before any image pull) must reject it -- or, for published ports, warn
# and strip it. A silent hang at guest runtime is the failure this prevents.
#
# Fast + hermetic: no image pulls, no VM boot -- just the static validator.
# Exits 0 only if every case produces its expected, greppable diagnostic.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
PY="$ROOT/guest/bake_compose.py"
CORPUS="$ROOT/guest/stacks/rejects"

# case: <file> <expect: reject|warn> <substring the diagnostic must contain>
#
# NOTE (2b items 3&4): healthchecks + depends_on: service_healthy are now
# SUPPORTED (resolved by the guest-side healthcheck ticker), and RELATIVE
# read-write binds are now materialized -- so those are no longer rejections.
# The closed-world boundary is unchanged: ABSOLUTE host binds stay rejected, ro
# OR rw (absbind.yml / absbind_rw.yml).
CASES="
absbind.yml     reject  absolute host path
absbind_rw.yml  reject  absolute host path
extnet.yml      reject  external
pullalways.yml  reject  pull_policy: always
buildunpinned.yml reject NOT digest-pinned
ports.yml       warn    published ports
"

ok=1
printf '%-18s %-8s %-6s %s\n' "CASE" "EXPECT" "RESULT" "DIAGNOSTIC"
echo "-------------------------------------------------------------------------------"
while read -r file expect needle; do
  [ -n "${file:-}" ] || continue
  out="$(python3 "$PY" validate "$CORPUS/$file" 2>&1 >/dev/null)"; rc=$?
  diag="$(printf '%s' "$out" | grep -E 'DVMM_BAKE_(REJECT|WARN)' | head -1)"
  pass=0
  if [ "$expect" = "reject" ]; then
    [ "$rc" -eq 3 ] && printf '%s' "$diag" | grep -qi "$needle" && pass=1
  else # warn: validator still exits 0, but must emit a WARN mentioning the needle
    [ "$rc" -eq 0 ] && printf '%s' "$diag" | grep -qi "$needle" && pass=1
  fi
  if [ "$pass" -eq 1 ]; then res="PASS"; else res="FAIL"; ok=0; fi
  printf '%-18s %-8s %-6s %s\n' "$file" "$expect" "$res" "${diag:-<none> (rc=$rc)}"
done <<< "$CASES"

echo
if [ "$ok" -eq 1 ]; then
  echo "REJECT TESTS PASS: every out-of-subset compose produced its loud bake-time diagnostic."
  exit 0
fi
echo "REJECT TESTS FAIL: a case did not reject/warn as expected (see above)."
exit 1
