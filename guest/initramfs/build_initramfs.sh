#!/usr/bin/env bash
# Build the busybox initramfs used as the test guest rootfs.
#
# Produces initramfs.cpio.gz with no root privileges by using the kernel's
# gen_init_cpio tool (device nodes are described in a manifest, not mknod'd).
#
# Inputs (kept in-repo):
#   busybox          - static x86_64 busybox binary (see README for source)
#   init             - PID 1 script
#   gen_init_cpio.c  - vendored from the Linux kernel (usr/gen_init_cpio.c)
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
OUT="$HERE/initramfs.cpio.gz"
BB="$HERE/busybox"
INIT="$HERE/init"
INITTAB="$HERE/inittab"
GEN="$HERE/gen_init_cpio"

[ -f "$BB" ]      || { echo "missing $BB (static busybox)"; exit 1; }
[ -f "$INIT" ]    || { echo "missing $INIT"; exit 1; }
[ -f "$INITTAB" ] || { echo "missing $INITTAB"; exit 1; }

if [ ! -x "$GEN" ]; then
  echo "building gen_init_cpio..."
  cc -O2 -o "$GEN" "$HERE/gen_init_cpio.c"
fi
chmod +x "$BB" "$INIT"

MANIFEST="$(mktemp)"
trap 'rm -f "$MANIFEST"' EXIT
cat > "$MANIFEST" <<EOF
dir /proc 0755 0 0
dir /sys 0755 0 0
dir /dev 0755 0 0
dir /bin 0755 0 0
dir /sbin 0755 0 0
dir /etc 0755 0 0
nod /dev/console 0600 0 0 c 5 1
nod /dev/null 0666 0 0 c 1 3
nod /dev/ttyS0 0660 0 0 c 4 64
file /bin/busybox $BB 0755 0 0
slink /bin/sh busybox 0777 0 0
slink /sbin/init /bin/busybox 0777 0 0
file /etc/inittab $INITTAB 0644 0 0
file /init $INIT 0755 0 0
EOF

"$GEN" "$MANIFEST" | gzip -9 > "$OUT"
echo "wrote $OUT ($(stat -c%s "$OUT") bytes)"
