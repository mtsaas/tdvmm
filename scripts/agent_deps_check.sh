#!/usr/bin/env bash
# Assert `cargo tree -p dvmm-agent` matches the committed allowlist (Fable §1a).
#
# The guest agent's dependency closure must stay minimal: dvmm-proto + the
# serde/serde_json ecosystem ONLY. It must NEVER pull the heavy VMM crates
# (kvm-ioctls/vm-memory/linux-loader/vm-superio) or extras like libc/regex/clap.
# The allowlist is the sorted unique set of crate NAMES (version-independent, so a
# routine `cargo update` doesn't flap the gate; a NEW crate leaking in does).
#
# Usage: scripts/agent_deps_check.sh
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"; ROOT="$(cd "$HERE/.." && pwd)"
ALLOW="$ROOT/dvmm-agent/deps.allow"

[ -f "$ALLOW" ] || { echo "FAIL: missing allowlist $ALLOW"; exit 1; }

got="$(cd "$ROOT" && cargo tree -p dvmm-agent --prefix none 2>/dev/null \
        | sed -E 's/ v[0-9].*//; s/ \(.*//' | awk 'NF' | sort -u)"
want="$(sort -u "$ALLOW")"

echo "== dvmm-agent dependency allowlist gate =="
if ! diff <(printf '%s\n' "$want") <(printf '%s\n' "$got"); then
  echo "FAIL: dvmm-agent dep tree deviates from $ALLOW (diff above:"
  echo "      '<' = allowed-but-absent, '>' = present-but-not-allowed)."
  echo "      If intended, regenerate: cargo tree -p dvmm-agent --prefix none \\"
  echo "        | sed -E 's/ v[0-9].*//; s/ (.*//' | awk 'NF' | sort -u > $ALLOW"
  exit 1
fi

# Belt-and-suspenders: the heavy crates must be provably absent.
forbidden='kvm-ioctls|vm-memory|linux-loader|vm-superio|kvm-bindings|libc|regex|clap|sha2|serde_yaml|vmm-sys-util'
if printf '%s\n' "$got" | grep -Eq "^($forbidden)$"; then
  echo "FAIL: a forbidden crate leaked into dvmm-agent's dep tree"; exit 1
fi

echo "PASS: dvmm-agent dep tree == allowlist ($(printf '%s\n' "$got" | wc -l) crates; no heavy deps)"
