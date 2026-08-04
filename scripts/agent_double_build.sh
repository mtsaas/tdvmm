#!/usr/bin/env bash
# Agent DOUBLE-BUILD byte-identity gate (Fable §3): build the tdvmm-agent twice in
# FRESH pinned builder containers and require an identical sha256. Proves the musl
# build is reproducible (the property the `.tdvmm` bit-repro guarantee rests on for
# the baked agent).
#
# Usage: scripts/agent_double_build.sh
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"; ROOT="$(cd "$HERE/.." && pwd)"
BIN="$ROOT/target/release/tdvmm"

[ -x "$BIN" ] || { echo "building tdvmm..."; ( cd "$ROOT" && cargo build --release ) || exit 3; }

A="$(mktemp)"; B="$(mktemp)"; trap 'rm -f "$A" "$B"' EXIT
echo "== tdvmm-agent double-build byte-identity gate =="
sha_a="$("$BIN" build-agent -o "$A" | awk '{print $1}')"
sha_b="$("$BIN" build-agent -o "$B" | awk '{print $1}')"
echo "   build 1: $sha_a"
echo "   build 2: $sha_b"
if [ "$sha_a" = "$sha_b" ] && cmp -s "$A" "$B"; then
  echo "PASS: two fresh-container builds are byte-identical"
else
  echo "FAIL: agent builds differ (non-reproducible)"; exit 1
fi
