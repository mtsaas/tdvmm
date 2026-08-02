#!/usr/bin/env bash
# Assert the stripped static-musl dvmm-agent fits the size budget (Fable §1b): 3 MiB.
#
# Builds the agent via the pinned musl builder container (`dvmm build-agent`), so
# this measures the REAL baked artifact (opt-level=z, lto, strip=symbols). Expect
# ~0.5 MiB.
#
# Usage: scripts/agent_size_check.sh
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"; ROOT="$(cd "$HERE/.." && pwd)"
BIN="$ROOT/target/release/dvmm"
BUDGET=$((3 * 1024 * 1024))

[ -x "$BIN" ] || { echo "building dvmm..."; ( cd "$ROOT" && cargo build --release ) || exit 3; }

OUT="$(mktemp)"; trap 'rm -f "$OUT"' EXIT
echo "== dvmm-agent size-budget gate (<= 3 MiB) =="
"$BIN" build-agent -o "$OUT" >/dev/null || { echo "FAIL: agent build failed"; exit 1; }

size=$(stat -c%s "$OUT")
printf '   built agent: %d bytes (%.2f MiB); budget %d bytes (3.00 MiB)\n' \
  "$size" "$(awk "BEGIN{print $size/1048576}")" "$BUDGET"
if [ "$size" -le "$BUDGET" ]; then
  echo "PASS: agent within budget"
else
  echo "FAIL: agent exceeds the 3 MiB budget"; exit 1
fi
