#!/usr/bin/env bash
# Build (release) and boot a guest interactively on the serial console.
# Ctrl-A is passed to the guest; leave the guest with `poweroff` or `reboot`
# (or stop the VMM from another terminal, e.g. `pkill tdvmm`).
#
# Boots a baked stack (default: spinner) via `tdvmm run`. Override the stack:
#   STACK=webstack ./run.sh
# For the minimal busybox clock guest instead:
#   ./target/release/tdvmm boot --kernel testdata/kernel/vmlinux-6.1.128 \
#     --initrd testdata/initramfs/initramfs.cpio.gz --mem 256 --ff off
#
# Fast-forward is FORCED OFF here because this is the human entry point: at an
# interactive console fast-forward races the guest clock and pins a host core.
# The BINARY default stays FF-on (see main.rs); run.sh is the one place that
# picks real-time for a console. Pass `--ff on` through to override (extra args
# are forwarded and win over the flags below). The cmdline omits `tdvmm.stack=1`,
# so the guest runs the container self-test + a respawning serial shell rather
# than the baked compose.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
STACK="${STACK:-spinner}"

cargo build --release --manifest-path "$ROOT/Cargo.toml"
"$ROOT/target/release/tdvmm" build "$STACK" "$ROOT/testdata/stacks/$STACK/compose.yml"
exec "$ROOT/target/release/tdvmm" run "$STACK" \
  --ff off \
  --cmdline "console=ttyS0 reboot=t panic=1 pci=off no_timer_check tsc=reliable" \
  ${MEM:+--mem "$MEM"} \
  "$@"
