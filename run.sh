#!/usr/bin/env bash
# Build (release) and boot the VM interactively on the serial console.
# Ctrl-A is passed to the guest; leave the guest with `poweroff` or `reboot`
# (or stop the VMM from another terminal, e.g. `pkill dvmm`).
#
# Defaults to the Step 2a Alpine container guest (RAM-only rootfs with podman)
# and 2 GiB of RAM. Override for the minimal busybox clock guest, e.g.:
#   INITRD=guest/initramfs/initramfs.cpio.gz MEM=256 ./run.sh
#
# Fast-forward is FORCED OFF here (FF=off) because this is the human entry point:
# at an interactive console fast-forward races the guest clock and pins a host
# core. The BINARY default stays FF-on (see main.rs); run.sh is the one place
# that picks real-time for a console. Override with `FF=on ./run.sh`, or pass
# `--ff on` through (extra args are forwarded and win over the default).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
KERNEL="${KERNEL:-$ROOT/guest/kernel/vmlinux-6.1.128}"
INITRD="${INITRD:-$ROOT/guest/initramfs-alpine/initramfs-alpine.cpio.gz}"
MEM="${MEM:-2048}"
FF="${FF:-off}"

cargo build --release --manifest-path "$ROOT/Cargo.toml"
exec "$ROOT/target/release/dvmm" \
  --kernel "$KERNEL" \
  --initrd "$INITRD" \
  --mem "$MEM" \
  --ff "$FF" \
  "$@"
