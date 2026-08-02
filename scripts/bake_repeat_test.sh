#!/usr/bin/env bash
# Phase-2a bake-repeatability test (acceptance gate 4).
#
# Bakes the same compose input TWICE and requires an IDENTICAL result: the emitted
# compose.lock.yml must be byte-identical, and the stack manifest's COMPARED lines
# (pinned image digests + the compose.lock hash + the per-image ledger) must match.
# The built initramfs artifact hash is reported (it embeds apk/build metadata that
# is NOT bit-reproducible in Phase 1 -- see images.lock NOTES -- so it is shown,
# not gated).
#
# Squashed images are pinned via --timestamp, so their digests are stable; plain
# images pin to their immutable upstream digest. Hence the lock + digests repeat.
#
# Usage: scripts/bake_repeat_test.sh [compose.yml]   (default: dogfood)
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
COMPOSE="${1:-$ROOT/guest/stacks/dogfood/compose.yml}"
NAME="$(basename "$(dirname "$COMPOSE")")"
LOCK="$ROOT/guest/stacks/$NAME/compose.lock.yml"
MAN="$ROOT/guest/stacks/$NAME/stack.lock"

# compare only the reproducible portion of the manifest: the pinned image digests
# + compose.lock hash + sizing. Drop the informational tail (podman-version +
# baked-at) AND the initramfs_sha256 line -- the built ARTIFACT embeds apk/build
# metadata that is not bit-reproducible in Phase 1 (see images.lock NOTES), so it
# is reported, not gated.
compared() { sed '/informational (NOT compared/,$d' "$1" | grep -v '^initramfs_sha256'; }

echo "== bake #1 =="
"$ROOT/target/release/dvmm" build "$COMPOSE" >/tmp/bake1.log 2>&1 || { echo "BAKE1 FAILED"; tail -30 /tmp/bake1.log; exit 1; }
L1="$(sha256sum "$LOCK" | awk '{print $1}')"; M1="$(compared "$MAN")"; A1="$(awk '/^initramfs_sha256/{print $2}' "$MAN")"
cp "$LOCK" /tmp/lock1.yml

echo "== bake #2 =="
"$ROOT/target/release/dvmm" build "$COMPOSE" >/tmp/bake2.log 2>&1 || { echo "BAKE2 FAILED"; tail -30 /tmp/bake2.log; exit 1; }
L2="$(sha256sum "$LOCK" | awk '{print $1}')"; M2="$(compared "$MAN")"; A2="$(awk '/^initramfs_sha256/{print $2}' "$MAN")"

echo
echo "compose.lock.yml sha256:  bake1=$L1  bake2=$L2"
echo "initramfs sha256 (reported, not gated):  bake1=$A1  bake2=$A2"
ok=1
[ "$L1" = "$L2" ] || { echo "FAIL: compose.lock.yml differs between bakes"; ok=0; }
if [ "$M1" = "$M2" ]; then echo "manifest (compared portion): IDENTICAL"; else
  echo "FAIL: manifest digests/ledger differ between bakes:"; diff <(printf '%s' "$M1") <(printf '%s' "$M2"); ok=0
fi
echo
if [ "$ok" -eq 1 ]; then
  echo "BAKE REPEAT PASS: identical compose.lock.yml + identical pinned digests across two bakes."
  exit 0
fi
echo "BAKE REPEAT FAIL."
exit 1
