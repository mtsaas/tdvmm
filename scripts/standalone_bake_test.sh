#!/usr/bin/env bash
# Standalone acceptance: a FULL successful bake from an INSTALLED binary in an
# EMPTY cwd (no checkout, fresh cache).
#
# Simulates "install the binary, bake your compose file": copies the release
# tdvmm somewhere with NO repo around it, cd's into a bare project dir holding
# only the user's compose file (+ its bind sources), points --cache-dir at a
# FRESH dir, and bakes. Everything must resolve from embedded data + the
# sha-pinned fetches: the kernel compiles from the pinned source tarball in the
# pinned container (embedded config), the agent compiles from the EMBEDDED
# source in the pinned container, the minirootfs + compose CLI download against
# embedded pins, and a .tdvmm must come out. Nothing precompiled is downloaded.
#
# The in-container kernel compile is minutes long, so by default the fresh
# cache's kernel is seeded from an existing SHA-VERIFIED copy — the exact
# pinned bytes a cold build produces, nothing faked. FULL_COLD=1 skips the
# seeding and compiles the kernel for real (the true cold path; CI-nightly).
#
# Usage: scripts/standalone_bake_test.sh [stack]     (default: insert-trim)
# Env:   FULL_COLD=1  — true cold cache (in-container kernel compile, ~minutes)
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
STACK="${1:-insert-trim}"
FULL_COLD="${FULL_COLD:-0}"
KERNEL_VER="$(sed -n 's/^KERNEL_VERSION=//p' "$ROOT/testdata/kernel/kernel.lock")"
KERNEL_SHA="$(sed -n 's/^KERNEL_SHA256=//p' "$ROOT/testdata/kernel/kernel.lock")"

BIN="$ROOT/target/release/tdvmm"
[ -x "$BIN" ] || { echo "[standalone] building tdvmm..."; ( cd "$ROOT" && cargo build --release ) || exit 3; }

TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
INSTALL="$TMP/install/bin"          # the "installed binary": no repo above it
PROJECT="$TMP/project"              # the user's bare project dir (the cwd)
CACHE="$TMP/cache"                  # fresh cache: nothing pre-populated
mkdir -p "$INSTALL" "$PROJECT" "$CACHE"
cp "$BIN" "$INSTALL/tdvmm"

# The user's project: the compose file + its relative bind sources, nothing else
# (no lock fixtures, no testdata/ tree).
for f in "$ROOT/testdata/stacks/$STACK"/*; do
  case "$(basename "$f")" in
    compose.lock.yml|stack.lock) ;;
    *) cp -r "$f" "$PROJECT/" ;;
  esac
done
[ -f "$PROJECT/compose.yml" ] || { echo "FAIL: no compose.yml for stack $STACK"; exit 3; }

# Seed the kernel unless FULL_COLD: any existing copy whose sha256 matches the
# embedded pin is byte-identical to what the in-container build produces.
if [ "$FULL_COLD" != "1" ]; then
  for k in "${TDVMM_CACHE_DIR:-$HOME/.tdvmm}/kernel/vmlinux-$KERNEL_VER" \
           "$ROOT/.tdvmm-tmp/tdvmm-cache/kernel/vmlinux-$KERNEL_VER" \
           "$ROOT/testdata/kernel/vmlinux-$KERNEL_VER"; do
    if [ -f "$k" ] && echo "$KERNEL_SHA  $k" | sha256sum -c --quiet - 2>/dev/null; then
      mkdir -p "$CACHE/kernel"
      cp "$k" "$CACHE/kernel/vmlinux-$KERNEL_VER"
      echo "[standalone] seeded sha-verified kernel from $k"
      break
    fi
  done
fi

LOG="$TMP/bake.log"
echo "== standalone bake: installed binary, empty cwd, fresh cache =="
echo "   bin:     $INSTALL/tdvmm"
echo "   project: $PROJECT"
echo "   cache:   $CACHE"
# Snapshot the repo testdata/ state so the untouched-repo check below compares
# against it (uncommitted work in a dev checkout must not fail the test).
GUEST_BEFORE="$(cd "$ROOT" && git status --porcelain testdata/ 2>/dev/null)"
( cd "$PROJECT" && env -u TDVMM_CACHE_DIR \
    "$INSTALL/tdvmm" build --no-cache --cache-dir "$CACHE" "$STACK-standalone" compose.yml \
) >"$LOG" 2>&1
rc=$?

ok=1
if [ $rc -ne 0 ]; then
  echo "FAIL standalone bake failed (rc=$rc); tail of log:"; tail -30 "$LOG"; ok=0
else
  sha="$(sha256sum "$CACHE/artifacts/$STACK-standalone.tdvmm" | awk '{print $1}')"
  echo "OK   full standalone bake succeeded"
  echo "     .tdvmm sha256: $sha"
fi

# Every build input must now be in the fresh cache.
if [ -f "$CACHE/kernel/vmlinux-$KERNEL_VER" ] \
   && echo "$KERNEL_SHA  $CACHE/kernel/vmlinux-$KERNEL_VER" | sha256sum -c --quiet - 2>/dev/null; then
  if [ "$FULL_COLD" = "1" ]; then
    echo "OK   kernel compiled in-container from the embedded config + pinned source"
  else
    echo "OK   kernel present + sha-verified (seeded; FULL_COLD=1 compiles it for real)"
  fi
else
  echo "FAIL kernel missing/mismatched in fresh cache"; ok=0
fi
if ls "$CACHE"/agent/tdvmm-agent-* >/dev/null 2>&1; then
  echo "OK   agent compiled in-container from the embedded source (cached)"
else
  echo "FAIL agent missing from fresh cache"; ok=0
fi
if ls "$CACHE"/downloads/alpine-minirootfs-*.tar.gz >/dev/null 2>&1; then
  echo "OK   alpine minirootfs downloaded (embedded pin)"
else
  echo "FAIL minirootfs missing from fresh cache"; ok=0
fi
if ls "$CACHE"/downloads/docker-compose-* >/dev/null 2>&1; then
  echo "OK   compose CLI downloaded (embedded pin)"
else
  echo "FAIL compose CLI missing from fresh cache"; ok=0
fi

# The repo must be untouched (nothing may write into testdata/) — compared
# against the pre-bake snapshot, so pre-existing uncommitted work doesn't trip.
GUEST_AFTER="$(cd "$ROOT" && git status --porcelain testdata/ 2>/dev/null)"
if [ "$GUEST_AFTER" != "$GUEST_BEFORE" ]; then
  echo "FAIL bake dirtied the repo testdata/ tree:"
  diff <(printf '%s' "$GUEST_BEFORE") <(printf '%s' "$GUEST_AFTER"); ok=0
else
  echo "OK   repo testdata/ tree untouched"
fi

if [ "$ok" = "1" ]; then
  echo "== STANDALONE BAKE TEST PASS ==  (kernel: $([ "$FULL_COLD" = 1 ] && echo compiled-in-container || echo seeded))"
  exit 0
fi
cp "$LOG" /tmp/standalone_bake.log 2>/dev/null
echo "== STANDALONE BAKE TEST FAIL ==  (full bake log: /tmp/standalone_bake.log)"
exit 1
