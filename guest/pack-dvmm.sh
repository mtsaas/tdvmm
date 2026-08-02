#!/usr/bin/env bash
# deterministic-vmm OP-1a: pack a baked stack into ONE self-contained `.dvmm`.
#
#   pack-dvmm.sh <stack> [-o out.dvmm]
#
# Assembles the stack's already-built boot artifacts + provenance into a single
# `.dvmm` file via `dvmm pack` -- the SAME canonical (deterministic) encoder that
# `dvmm run`/`inspect`/`verify` read, so encode and decode can never drift.
#
# Inputs (all must already exist -- this does NOT bake; bake-stack.sh does):
#   guest/stacks/<stack>/stack.lock          (image pins + hashes + RAM estimate)
#   guest/stacks/<stack>/compose.lock.yml    (the only compose file the guest sees)
#   guest/kernel/vmlinux-6.1.128             (the ELF kernel)
#   guest/initramfs-alpine/initramfs-alpine-<stack>.cpio.gz   (per-stack initramfs)
#
# The manifest we hand to `dvmm pack` contains NO volatile fields (no timestamps),
# so packing the same built inputs twice yields a BYTE-IDENTICAL `.dvmm` (the
# bit-reproducible-artifact gate). `dvmm pack` then fills in the per-member sha256s
# and re-serializes the manifest canonically, and writes the deterministic tar.
#
# Env overrides (run-defaults baked into the artifact):
#   DVMM_CMDLINE          default kernel cmdline (stack mode, interval/maxrows/hc)
#   DVMM_FF               fast-forward default (on|off; default on)
#   DVMM_MAX_VIRTUAL_TIME baked virtual-time horizon (duration; default unset)
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"

STACK=""
OUT=""
while [ $# -gt 0 ]; do
  case "$1" in
    -o|--out) OUT="$2"; shift 2 ;;
    -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
    -*) echo "pack-dvmm: unknown flag $1" >&2; exit 2 ;;
    *) STACK="$1"; shift ;;
  esac
done
[ -n "$STACK" ] || { echo "usage: pack-dvmm.sh <stack> [-o out.dvmm]" >&2; exit 2; }

STACKDIR="$ROOT/guest/stacks/$STACK"
LOCK="$STACKDIR/stack.lock"
COMPOSE_LOCK="$STACKDIR/compose.lock.yml"
KERNEL="$ROOT/guest/kernel/vmlinux-6.1.128"
INITRAMFS="$ROOT/guest/initramfs-alpine/initramfs-alpine-${STACK}.cpio.gz"
BIN="$ROOT/target/release/dvmm"
ENGINE_LOCK="$ROOT/guest/initramfs-alpine/compose-engine.lock"
[ -n "$OUT" ] || OUT="$ROOT/guest/initramfs-alpine/${STACK}.dvmm"

for f in "$LOCK" "$COMPOSE_LOCK" "$KERNEL" "$INITRAMFS"; do
  [ -f "$f" ] || { echo "pack-dvmm: missing input: $f" >&2; exit 3; }
done
if [ ! -x "$BIN" ]; then
  echo "pack-dvmm: building dvmm..." >&2
  ( cd "$ROOT" && cargo build --release ) >&2
fi

# Version anchors (stable, non-volatile).
COMPOSE_VERSION=""; COMPOSE_SHA256=""
# shellcheck disable=SC1090
[ -f "$ENGINE_LOCK" ] && source "$ENGINE_LOCK"
ALPINE_VER="$(grep -E '^ALPINE_VER=' "$ROOT/guest/initramfs-alpine/build_rootfs.sh" | head -1 | cut -d'"' -f2)"

# The baked run-defaults. Stack mode, the demo cadence/cap + a healthcheck tick;
# gates override any of these via CLI flags (baked < flag).
DVMM_CMDLINE="${DVMM_CMDLINE:-console=ttyS0 reboot=t panic=1 pci=off no_timer_check tsc=reliable dvmm.stack=1 dvmm.interval=3600 dvmm.maxrows=1000 dvmm.hc_tick=2}"
DVMM_FF="${DVMM_FF:-on}"
DVMM_MAX_VIRTUAL_TIME="${DVMM_MAX_VIRTUAL_TIME:-}"

WORK="$(mktemp -d)"; trap 'rm -rf "$WORK"' EXIT

# Snapshot the effective guest CPUID profile (the manifest artifact). Stable on
# one host; a host/CPU change surfaces here -> a different manifest -> a different
# .dvmm sha256 (desired: drift is detected, never silent).
"$BIN" dump-cpuid > "$WORK/cpuid.txt"

# Build the (partial) manifest JSON. dvmm pack fills format_version + per-member
# hashes and re-serializes canonically, so python's key order does not matter.
MANIFEST_IN="$WORK/manifest-in.json"
LOCK="$LOCK" CPUID="$WORK/cpuid.txt" \
COMPOSE_VERSION="$COMPOSE_VERSION" COMPOSE_SHA256="$COMPOSE_SHA256" \
ALPINE_VER="$ALPINE_VER" \
DVMM_CMDLINE="$DVMM_CMDLINE" DVMM_FF="$DVMM_FF" DVMM_MAX_VIRTUAL_TIME="$DVMM_MAX_VIRTUAL_TIME" \
python3 - "$MANIFEST_IN" <<'PY'
import json, os, sys, hashlib

out = sys.argv[1]
lock = os.environ["LOCK"]
cpuid_path = os.environ["CPUID"]

stack = project = ""
ram_estimate = 0
mem_mib = 0
podman = ""
images = {}   # key -> dict(upstream, policy, content_id, size_mib, pinned)

def toks(rest):
    d = {}
    for t in rest.split():
        if "=" in t:
            k, v = t.split("=", 1); d[k] = v
    return d

for raw in open(lock):
    line = raw.rstrip("\n")
    s = line.strip()
    if line.startswith("stack "):        stack = s.split(None, 1)[1].strip()
    elif line.startswith("project "):    project = s.split(None, 1)[1].strip()
    elif line.startswith("mem_mib "):    mem_mib = int(s.split()[1])
    elif line.startswith("ram_estimate_mib "): ram_estimate = int(s.split()[1])
    elif s.startswith("# podman-version:"): podman = s.split(":", 1)[1].strip()
    elif s.startswith("image "):
        d = toks(s[len("image"):])
        policy = d.get("policy", "")
        if policy == "build":
            key = d.get("tag", "")
            images[key] = {"upstream": d.get("tag",""), "policy": "build",
                           "content_id": d.get("content_id",""),
                           "size_mib": int(d.get("size_mib","0") or 0), "pinned": ""}
        elif policy == "squash":
            key = d.get("upstream","")
            images[key] = {"upstream": key, "policy": "squash",
                           "content_id": d.get("src_diffid",""),
                           "size_mib": int(d.get("size_mib","0") or 0), "pinned": ""}
        else:  # plain
            key = d.get("upstream","")
            images[key] = {"upstream": key, "policy": "plain",
                           "content_id": d.get("diffid",""),
                           "size_mib": int(d.get("size_mib","0") or 0), "pinned": key}
    elif s.startswith("pin "):
        d = toks(s[len("pin"):])
        up = d.get("upstream",""); pin = d.get("pinned","")
        if up in images: images[up]["pinned"] = pin

with open(cpuid_path, "rb") as f:
    cpuid_bytes = f.read()
cpuid_profile = cpuid_bytes.decode("utf-8", "replace")
cpuid_sha = hashlib.sha256(cpuid_bytes).hexdigest()

mvt = os.environ.get("DVMM_MAX_VIRTUAL_TIME", "").strip()
manifest = {
    "stack": stack,
    "project": project,
    "anchors": {
        "cpuid_sha256": cpuid_sha,
        "cpuid_profile": cpuid_profile,
        "compose_engine": {
            "version": os.environ.get("COMPOSE_VERSION",""),
            "sha256":  os.environ.get("COMPOSE_SHA256",""),
        },
        "images": [images[k] for k in sorted(images)],
        "toolchain": {
            "podman":  podman,
            "alpine":  os.environ.get("ALPINE_VER",""),
            "compose": os.environ.get("COMPOSE_VERSION",""),
        },
        "ram_estimate_mib": ram_estimate,
    },
    "run_defaults": {
        "mem_mib": mem_mib,
        "cmdline": os.environ["DVMM_CMDLINE"],
        "fast_forward": os.environ.get("DVMM_FF","on") == "on",
        "max_virtual_time": (mvt if mvt else None),
    },
}
with open(out, "w") as f:
    json.dump(manifest, f)
PY

"$BIN" pack --manifest-in "$MANIFEST_IN" \
  --kernel "$KERNEL" --initramfs "$INITRAMFS" --compose-lock "$COMPOSE_LOCK" \
  -o "$OUT"

DVMM_SHA="$(sha256sum "$OUT" | awk '{print $1}')"
echo "pack-dvmm: wrote $OUT"
echo "pack-dvmm: sha256 $DVMM_SHA  ($(stat -c%s "$OUT") bytes)"
