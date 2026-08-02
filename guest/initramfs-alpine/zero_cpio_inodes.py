#!/usr/bin/env python3
"""Zero the c_ino field of every entry in a newc-format cpio, in place.

The last source of initramfs non-determinism: GNU cpio's `--reproducible`
renumbers inodes, but for a few containers/storage files the renumbering is not
stable bake-to-bake (the underlying real-inode order varies), leaving a couple of
differing c_ino header bytes. There are NO hardlinks in this rootfs (every entry
has nlink=1), so the c_ino field is purely cosmetic — extraction never uses it —
and zeroing it everywhere makes the archive byte-identical across bakes.

newc header (ASCII): "070701" magic, then 13 fixed 8-hex-char fields:
  ino mode uid gid nlink mtime filesize devmajor devminor rdevmajor rdevminor
  namesize check
c_ino is the first field (bytes [6:14]); c_filesize is field 6, c_namesize
field 11. Header+name is padded to 4 bytes; data is padded to 4 bytes.
"""
import sys

MAGIC = b"070701"
ZERO_INO = b"00000000"


def main(path: str) -> int:
    data = bytearray(open(path, "rb").read())
    i = 0
    n = len(data)
    while i + 110 <= n:
        if data[i:i + 6] != MAGIC:
            raise SystemExit(f"not a newc cpio header at offset {i}")

        def field(idx: int) -> int:
            off = i + 6 + idx * 8
            return int(data[off:off + 8], 16)

        namesize = field(11)
        filesize = field(6)
        name = bytes(data[i + 110:i + 110 + namesize - 1])

        # Zero c_ino (field 0).
        data[i + 6:i + 14] = ZERO_INO

        if name == b"TRAILER!!!":
            break

        hdrlen = (110 + namesize + 3) & ~3
        datalen = (filesize + 3) & ~3
        i += hdrlen + datalen

    open(path, "wb").write(data)
    return 0


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit("usage: zero_cpio_inodes.py <cpio>")
    sys.exit(main(sys.argv[1]))
