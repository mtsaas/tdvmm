#!/usr/bin/env bash
# Build the pinned minimal microvm guest kernel.
#
# Produces an uncompressed ELF `vmlinux` from a pinned Linux source tree plus
# the in-repo config (microvm-kernel-x86_64-6.1.config: Firecracker's published
# microvm config with HPET disabled, everything built-in, no modules).
#
# Host build (per project convention we build Yocto/kernels directly on the
# Linux host, not in a container). Requires: gcc, make, bc, flex, bison,
# libelf/elfutils, openssl headers. On Arch: `pacman -S bc flex bison` etc.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
KVER="${KVER:-6.1.128}"            # pinned; matches the bootstrap vmlinux
CONFIG="$HERE/microvm-kernel-x86_64-6.1.config"
JOBS="${JOBS:-$(nproc)}"
SRC="$HERE/linux-$KVER"

command -v bc >/dev/null || { echo "ERROR: 'bc' is required to build the kernel"; exit 1; }

if [ ! -d "$SRC" ]; then
  TARBALL="linux-$KVER.tar.xz"
  echo "downloading $TARBALL ..."
  curl -sSL -o "$HERE/$TARBALL" \
    "https://cdn.kernel.org/pub/linux/kernel/v6.x/$TARBALL"
  tar -C "$HERE" -xf "$HERE/$TARBALL"
  rm -f "$HERE/$TARBALL"
fi

# Modern GCC defaults to the C23 standard, where bool/true/false are keywords;
# Linux 6.1's real-mode boot stub predates that and won't compile. The main
# kernel already builds with -std=gnu11 — extend that to every translation unit
# (incl. real-mode) via a CC wrapper so the build works on a C23-default GCC.
CCWRAP="$SRC/.cc-gnu11"
cat > "$CCWRAP" <<'EOF'
#!/bin/sh
exec gcc -std=gnu11 "$@"
EOF
chmod +x "$CCWRAP"

cp "$CONFIG" "$SRC/.config"
make -C "$SRC" CC="$CCWRAP" olddefconfig
make -C "$SRC" -j"$JOBS" CC="$CCWRAP" vmlinux

cp "$SRC/vmlinux" "$HERE/vmlinux-$KVER"
echo "built $HERE/vmlinux-$KVER"
