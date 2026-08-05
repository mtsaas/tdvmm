//! Deterministic newc (SVR4) cpio emitter — the `initramfs` payload of a `.tdvmm`.
//!
//! ## Byte layout
//!
//! An archive is a run of entries followed by a `TRAILER!!!` entry. Each entry is a
//! 110-byte header — the magic `070701` and thirteen 8-char uppercase-hex fields
//! (ino, mode, uid, gid, nlink, mtime, filesize, devmajor, devminor, rdevmajor,
//! rdevminor, namesize, check) — then the NUL-terminated name, then the file data.
//! The header-plus-name and the data are each zero-padded to a 4-byte boundary.
//!
//! The kernel's initramfs unpacker reads *concatenated* newc archives, so an
//! initramfs is assembled from several independent segments joined end to end. This
//! module emits two kinds, each self-contained (its own trailer, then padding to a
//! 512-byte boundary):
//!
//!   1. [`nodes_segment`] — the device nodes `/dev`, `/dev/console`, `/dev/null`,
//!      `/dev/ttyS0`, `/dev/ttyS1`. Every entry carries a fixed containing device
//!      `3:1`; the character devices carry their real `rdev`.
//!   2. [`rootfs_segment`] — one on-disk directory tree, everything except `/dev`
//!      (which the node segment supplies).
//!
//! [`gzip_to`] compresses the concatenated segments into the final `initramfs`.
//!
//! ## Determinism
//!
//! Identical inputs produce byte-identical output, which is what makes the whole
//! `.tdvmm` byte-reproducible. Every field that would otherwise vary is normalized:
//!
//!   * entries are emitted in `LC_ALL=C` byte-sorted order of their relative names;
//!   * uid/gid and the containing device numbers are zeroed, and mtime is pinned to
//!     [`BUILD_EPOCH`];
//!   * `c_ino` is renumbered by first appearance in sorted order, so it reflects the
//!     tree's shape rather than the host's actual inode numbers;
//!   * directories are written with `nlink = 2` (the on-disk subdirectory count is
//!     not preserved);
//!   * a hardlink group is emitted as GNU `cpio --reproducible` does: the non-last
//!     links are written first (in reverse of their sorted order) with `filesize = 0`
//!     and no data, and only the last link in sorted order carries the file content.
//!
//! [`gzip_to`] zeroes the gzip member's mtime and omits the original filename, so the
//! compression layer is deterministic too. `cpio_tests` pins the format against a
//! byte-identity golden and against GNU `cpio --reproducible` on synthetic trees.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use rayon::prelude::*;

/// The fixed guest wall-clock epoch (2026-08-01T00:00:00Z), written as the mtime of
/// every archived entry so timestamps never vary between builds.
const BUILD_EPOCH: u32 = 1785542400;

// libc file-type bits (S_IFMT masking).
const S_IFMT: u32 = 0o170000;
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;
const S_IFLNK: u32 = 0o120000;
const S_IFCHR: u32 = 0o020000;
const S_IFBLK: u32 = 0o060000;

/// The error type for cpio emission: an I/O failure (tagged with the operation and
/// keeping the underlying [`std::io::Error`] as its `source`), or a file too large
/// for the newc format's 32-bit size field.
#[derive(Debug)]
pub enum CpioError {
    /// An I/O failure, tagged with the operation (e.g. `"reading /…/bin/sh"`).
    Io { what: String, source: std::io::Error },
    /// A file larger than the newc `filesize` field (32-bit) can encode. Names the
    /// entry and its byte length.
    TooLarge { name: String, size: u64 },
}

impl CpioError {
    /// An [`Io`](CpioError::Io) with `what` context attached.
    pub(crate) fn io(what: impl Into<String>, source: std::io::Error) -> Self {
        CpioError::Io { what: what.into(), source }
    }
    /// A [`TooLarge`](CpioError::TooLarge) naming the entry and its byte length.
    pub(crate) fn too_large(name: impl Into<String>, size: u64) -> Self {
        CpioError::TooLarge { name: name.into(), size }
    }
}

impl fmt::Display for CpioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CpioError::Io { what, source } => write!(f, "{what}: {source}"),
            CpioError::TooLarge { name, size } => {
                write!(f, "{name} is {size} bytes, over the newc 32-bit filesize limit")
            }
        }
    }
}

impl std::error::Error for CpioError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CpioError::Io { source, .. } => Some(source),
            CpioError::TooLarge { .. } => None,
        }
    }
}

/// The per-entry fields of a newc header. `uid`, `gid`, and the unused `check` field
/// are always zero and are emitted by [`Cpio::header`], not carried here.
struct NewcHeader<'a> {
    ino: u32,
    mode: u32,
    nlink: u32,
    mtime: u32,
    filesize: u32,
    devmajor: u32,
    devminor: u32,
    rdevmajor: u32,
    rdevminor: u32,
    name: &'a [u8],
}

/// An archive under construction: the output buffer plus the framing operations
/// that keep it a valid, block-aligned newc stream.
///
/// This self-buffers into an owned `Vec<u8>` rather than being generic over
/// `impl Write` like `artifact::tar`: `pad_to_512` needs the running archive
/// length, and every caller materializes a whole segment before concatenating and
/// gzipping, so streaming to a sink would buy nothing.
struct Cpio {
    out: Vec<u8>,
}

impl Cpio {
    fn new() -> Cpio {
        Cpio { out: Vec::new() }
    }

    /// Append `n` zero bytes — the padding used at 4-byte and 512-byte boundaries.
    fn pad_zeros(&mut self, n: usize) {
        self.out.resize(self.out.len() + n, 0);
    }

    /// Write one newc header (magic + 13 uppercase-hex fields) followed by the
    /// NUL-terminated name, padded to a 4-byte boundary.
    fn header(&mut self, h: NewcHeader<'_>) {
        let namesize = h.name.len() as u32 + 1; // includes the trailing NUL
        self.out.extend_from_slice(b"070701");
        let fields = [
            h.ino, h.mode, 0, // uid
            0, // gid
            h.nlink, h.mtime, h.filesize, h.devmajor, h.devminor, h.rdevmajor, h.rdevminor,
            namesize, 0, // check
        ];
        for v in fields {
            self.out.extend_from_slice(format!("{v:08X}").as_bytes());
        }
        self.out.extend_from_slice(h.name);
        self.out.push(0);
        let hdrlen = 110 + namesize as usize;
        self.pad_zeros((4 - (hdrlen % 4)) % 4);
    }

    /// Write member data, padded to a 4-byte boundary.
    fn data(&mut self, d: &[u8]) {
        self.out.extend_from_slice(d);
        self.pad_zeros((4 - (d.len() % 4)) % 4);
    }

    /// Write the `TRAILER!!!` entry that marks end-of-archive.
    fn trailer(&mut self) {
        self.header(NewcHeader {
            ino: 0,
            mode: 0,
            nlink: 1,
            mtime: 0,
            filesize: 0,
            devmajor: 0,
            devminor: 0,
            rdevmajor: 0,
            rdevminor: 0,
            name: b"TRAILER!!!",
        });
    }

    /// Pad the archive-so-far up to a 512-byte boundary (the cpio block size, so a
    /// segment can be concatenated with the next).
    fn pad_to_512(&mut self) {
        self.pad_zeros((512 - (self.out.len() % 512)) % 512);
    }
}

/// Emit the device-node segment: the fixed set of `/dev` entries, then a trailer and
/// 512-byte padding. Every entry's containing device is `3:1`; the character devices
/// carry their real `rdev`.
fn write_nodes_segment(c: &mut Cpio) {
    // (name, mode, nlink, rdevmajor, rdevminor)
    let nodes: &[(&[u8], u32, u32, u32, u32)] = &[
        (b"dev", S_IFDIR | 0o755, 2, 0, 0),
        (b"dev/console", S_IFCHR | 0o600, 1, 5, 1),
        (b"dev/null", S_IFCHR | 0o666, 1, 1, 3),
        (b"dev/ttyS0", S_IFCHR | 0o660, 1, 4, 64),
        (b"dev/ttyS1", S_IFCHR | 0o660, 1, 4, 65),
    ];
    for &(name, mode, nlink, rmaj, rmin) in nodes {
        c.header(NewcHeader {
            ino: 0,
            mode,
            nlink,
            mtime: BUILD_EPOCH,
            filesize: 0,
            devmajor: 3,
            devminor: 1,
            rdevmajor: rmaj,
            rdevminor: rmin,
            name,
        });
    }
    c.trailer();
    c.pad_to_512();
}

/// One collected rootfs entry, from an `lstat` that does not follow symlinks.
struct Entry {
    name: Vec<u8>, // relative path, no leading "./"
    path: PathBuf,
    dev: u64,
    ino: u64,
    rdev: u64,
    nlink: u32,
    mode: u32,
    is_dir: bool,
}

/// Recursively collect every entry under `root`, excluding `dev` and `dev/*` (the
/// node segment supplies those). Names are relative to `root`, with no leading "./",
/// and returned in `LC_ALL=C` byte-sorted order.
fn collect(root: &Path) -> Result<Vec<Entry>, CpioError> {
    let mut entries = Vec::new();
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    // Symlinks are never followed; recurse only into real directories.
    while let Some(dir) = stack.pop() {
        let rd = fs::read_dir(&dir)
            .map_err(|err| CpioError::io(format!("reading directory {}", dir.display()), err))?;
        for de in rd {
            let de = de
                .map_err(|err| CpioError::io(format!("reading directory {}", dir.display()), err))?;
            let path = de.path();
            let rel = path
                .strip_prefix(root)
                .expect("read_dir walks paths rooted at `root`, so each is prefixed by it");
            let relbytes = rel.as_os_str().as_bytes().to_vec();
            // Exclude dev and everything under it.
            if relbytes == b"dev" || relbytes.starts_with(b"dev/") {
                continue;
            }
            let meta = fs::symlink_metadata(&path)
                .map_err(|err| CpioError::io(format!("stat {}", path.display()), err))?;
            let mode = meta.mode();
            let is_dir = (mode & S_IFMT) == S_IFDIR;
            entries.push(Entry {
                name: relbytes,
                path: path.clone(),
                dev: meta.dev(),
                ino: meta.ino(),
                rdev: meta.rdev(),
                nlink: meta.nlink() as u32,
                mode,
                is_dir,
            });
            if is_dir {
                stack.push(path);
            }
        }
    }
    // The relative names sort identically to the "./"-prefixed names under LC_ALL=C.
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
}

/// The `(rdevmajor, rdevminor)` for a device node, else `(0, 0)`. Uses the same
/// `dev_t` split as GNU cpio.
fn rdev_of(meta_rdev: u64, mode: u32) -> (u32, u32) {
    let t = mode & S_IFMT;
    if t == S_IFCHR || t == S_IFBLK {
        let major = ((meta_rdev >> 8) & 0xfff) as u32;
        let minor = (meta_rdev & 0xff) as u32 | (((meta_rdev >> 12) & !0xffu64) as u32);
        (major, minor)
    } else {
        (0, 0)
    }
}

/// Emit the rootfs tree under `root` as one segment: byte-sorted, uid/gid 0,
/// containing device zeroed, mtime pinned, `c_ino` renumbered, hardlink groups
/// handled as GNU cpio does; then a trailer and 512-byte padding.
fn write_rootfs_segment(c: &mut Cpio, root: &Path) -> Result<(), CpioError> {
    let entries = collect(root)?;

    // Pre-read every entry's payload in parallel, indexed by the SORTED entry
    // order. Only the reads are parallel: the emit passes below stay sequential
    // over `entries`, pulling `payloads[idx]`, so thread completion order can
    // never reach the archive bytes.
    let payloads = entries
        .par_iter()
        .map(read_payload)
        .collect::<Result<Vec<_>, CpioError>>()?;

    // Pass 1: renumber inodes by first appearance in sorted order.
    let mut ino_map: HashMap<(u64, u64), u32> = HashMap::new();
    let mut counter: u32 = 0;
    for e in &entries {
        ino_map.entry((e.dev, e.ino)).or_insert_with(|| {
            let v = counter;
            counter += 1;
            v
        });
    }

    // Pass 1b: count the archived members of each hardlink group (non-dir, nlink>1).
    let mut group_total: HashMap<(u64, u64), usize> = HashMap::new();
    for e in &entries {
        if !e.is_dir && e.nlink > 1 {
            *group_total.entry((e.dev, e.ino)).or_insert(0) += 1;
        }
    }

    // Pass 2: emit. A hardlink group is held back until its last archived member, at
    // which point the earlier links are written (reversed, data-less) and then this
    // one with data — matching GNU cpio --reproducible.
    let mut deferred: HashMap<(u64, u64), Vec<usize>> = HashMap::new();
    let mut seen: HashMap<(u64, u64), usize> = HashMap::new();

    for (idx, e) in entries.iter().enumerate() {
        let key = (e.dev, e.ino);
        if !e.is_dir && e.nlink > 1 {
            let total = group_total[&key];
            let s = seen.entry(key).or_insert(0);
            *s += 1;
            if *s < total {
                deferred.entry(key).or_default().push(idx);
                continue;
            }
            if let Some(list) = deferred.remove(&key) {
                for &di in list.iter().rev() {
                    write_entry(c, &entries[di], ino_map[&key], &[])?;
                }
            }
            write_entry(c, e, ino_map[&key], &payloads[idx])?;
        } else {
            write_entry(c, e, ino_map[&key], &payloads[idx])?;
        }
    }

    // Every hardlink group drains at its last link; a leftover would mean a link
    // points outside the archived tree, which a closed rootfs never contains.
    debug_assert!(deferred.is_empty(), "cpio: undrained hardlink deferral");

    c.trailer();
    c.pad_to_512();
    Ok(())
}

/// Read the payload bytes an entry's data section carries: the file contents of
/// a regular file, the target path of a symlink, empty for everything else.
fn read_payload(e: &Entry) -> Result<Vec<u8>, CpioError> {
    match e.mode & S_IFMT {
        S_IFREG => {
            fs::read(&e.path).map_err(|err| CpioError::io(format!("reading {}", e.path.display()), err))
        }
        S_IFLNK => Ok(fs::read_link(&e.path)
            .map_err(|err| CpioError::io(format!("reading symlink {}", e.path.display()), err))?
            .as_os_str()
            .as_bytes()
            .to_vec()),
        _ => Ok(Vec::new()),
    }
}

/// Write a single rootfs entry with its pre-read `payload` bytes. A non-last
/// hardlink passes `&[]`: its header records `filesize = 0` and carries no
/// content.
fn write_entry(c: &mut Cpio, e: &Entry, new_ino: u32, payload: &[u8]) -> Result<(), CpioError> {
    let (rmaj, rmin) = rdev_of(e.rdev, e.mode);

    // GNU cpio --reproducible writes nlink=2 for every directory (it does not
    // preserve the subdirectory-dependent on-disk count); non-directories keep their
    // real link count, so a hardlink group carries the true count.
    let nlink = if e.is_dir { 2 } else { e.nlink };

    let filesize = u32::try_from(payload.len())
        .map_err(|_| CpioError::too_large(String::from_utf8_lossy(&e.name), payload.len() as u64))?;

    c.header(NewcHeader {
        ino: new_ino,
        mode: e.mode,
        nlink,
        mtime: BUILD_EPOCH,
        filesize,
        devmajor: 0,
        devminor: 0,
        rdevmajor: rmaj,
        rdevminor: rmin,
        name: &e.name,
    });
    if !payload.is_empty() {
        c.data(payload);
    }
    Ok(())
}

/// The device-nodes cpio segment (with its trailer and 512-byte padding), on its
/// own. Because the kernel unpacker reads concatenated archives, a full initramfs is
/// assembled as `nodes_segment() + <base segment> + <stack segment>`.
pub fn nodes_segment() -> Vec<u8> {
    let mut c = Cpio::new();
    write_nodes_segment(&mut c);
    c.out
}

/// Emit one rootfs tree as a standalone cpio segment (byte-sorted, uid/gid 0,
/// containing device zeroed, mtime pinned, `c_ino` renumbered, GNU-cpio hardlink
/// handling, trailer and 512-byte padding). Identical trees produce identical bytes;
/// concatenate segments to form the final initramfs.
///
/// # Errors
///
/// Returns [`CpioError::Io`] if walking `root` or reading any file, symlink, or
/// directory under it fails.
pub fn rootfs_segment(root: &Path) -> Result<Vec<u8>, CpioError> {
    let mut c = Cpio::new();
    write_rootfs_segment(&mut c, root)?;
    Ok(c.out)
}

/// gzip a concatenated cpio buffer to `out_path` at level 9, with the gzip member's
/// mtime zeroed and no original filename — the deterministic equivalent of
/// `gzip -n`, so identical input bytes give identical output bytes.
///
/// The compressor is `flate2`'s default pure-Rust `miniz_oxide` backend. A
/// zlib/zlib-ng feature would add a C build dependency and must never be enabled; the
/// kernel decompresses the standard deflate stream regardless.
///
/// # Errors
///
/// Returns [`CpioError::Io`] if `out_path` cannot be created or written.
pub fn gzip_to(combined: &[u8], out_path: &Path) -> Result<(), CpioError> {
    use flate2::{Compression, GzBuilder};
    use std::io::Write;
    let out_file =
        fs::File::create(out_path).map_err(|err| CpioError::io(format!("creating {}", out_path.display()), err))?;
    let mut enc = GzBuilder::new().mtime(0).write(out_file, Compression::new(9));
    enc.write_all(combined)
        .map_err(|err| CpioError::io(format!("writing {}", out_path.display()), err))?;
    enc.finish()
        .map_err(|err| CpioError::io(format!("finishing {}", out_path.display()), err))?;
    Ok(())
}

#[cfg(test)]
mod cpio_tests {
    use super::*;
    use std::process::Command;

    fn build_golden_tree(root: &Path) {
        use std::os::unix::fs::{symlink, PermissionsExt};
        // Pin every mode explicitly so the golden hash does not depend on the umask.
        let chmod = |p: &Path, m: u32| {
            fs::set_permissions(p, fs::Permissions::from_mode(m)).unwrap();
        };

        fs::create_dir_all(root.join("bin")).unwrap();
        fs::create_dir_all(root.join("etc")).unwrap();
        chmod(&root.join("bin"), 0o755);
        chmod(&root.join("etc"), 0o755);

        fs::write(root.join("bin/real"), b"hello\n").unwrap();
        chmod(&root.join("bin/real"), 0o755);
        // A hardlink group of three, exercising the reversed data-less deferral.
        fs::hard_link(root.join("bin/real"), root.join("bin/link1")).unwrap();
        fs::hard_link(root.join("bin/real"), root.join("bin/link2")).unwrap();
        symlink("real", root.join("bin/sym")).unwrap();

        fs::write(root.join("etc/conf"), b"k=v\n").unwrap();
        chmod(&root.join("etc/conf"), 0o644);
    }

    #[test]
    fn nodes_segment_golden_sha256_is_stable() {
        let seg = nodes_segment();
        // Byte-identity tripwire: nodes_segment() takes no filesystem input, so a
        // change to this hash means the emitted device-node bytes changed — rebuild
        // the golden only when that change is intended.
        assert_eq!(
            crate::artifact::sha256_hex(&seg),
            "d651f3a29e0ef23ce5c60d0403500c14b6fa4d13607d77bfdec0b9f773dcc206",
        );
    }

    #[test]
    fn rootfs_segment_golden_sha256_is_stable() {
        let tmp = std::env::temp_dir().join(format!("tdvmm-cpio-golden-{}", std::process::id()));
        let root = tmp.join("root");
        let _ = fs::remove_dir_all(&tmp);
        build_golden_tree(&root);
        let seg = rootfs_segment(&root).unwrap();
        let _ = fs::remove_dir_all(&tmp);
        // Byte-identity tripwire for the rootfs codec (sort order, inode renumbering,
        // hardlink deferral, symlink payload). Rebuild only on an intended change.
        assert_eq!(
            crate::artifact::sha256_hex(&seg),
            "c58dd5637c3dea56eecb18809340d2914904444506599daaade387f33233913c",
        );
    }

    #[test]
    fn gzip_to_is_deterministic_with_zero_mtime() {
        let dir = std::env::temp_dir().join(format!("tdvmm-cpio-gzip-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p1 = dir.join("a.gz");
        let p2 = dir.join("b.gz");
        let input = b"the quick brown fox jumps over the lazy dog\n".repeat(64);

        gzip_to(&input, &p1).unwrap();
        gzip_to(&input, &p2).unwrap();
        let g1 = fs::read(&p1).unwrap();
        let g2 = fs::read(&p2).unwrap();
        let _ = fs::remove_dir_all(&dir);

        // Deterministic: identical input -> identical gzip bytes.
        assert_eq!(g1, g2);
        // The gzip member's MTIME (bytes 4..8) is zeroed, as `gzip -n` does.
        assert_eq!(&g1[4..8], &[0, 0, 0, 0]);
        // Round-trips through a standard inflate.
        let mut dec = flate2::read::GzDecoder::new(&g1[..]);
        let mut back = Vec::new();
        std::io::Read::read_to_end(&mut dec, &mut back).unwrap();
        assert_eq!(back, input);
        // Byte-identity tripwire for the deflate output.
        assert_eq!(
            crate::artifact::sha256_hex(&g1),
            "775fc26dce5db8c51aae4406818d9bb849ae0d402c2a1db38e7dbf6cc05ce8d2",
        );
    }

    fn oracle_rootfs_cpio(root: &Path) -> Vec<u8> {
        // find . -mindepth 1 (excl dev) -print0 | LC_ALL=C sort -z | cpio ...
        let script = format!(
            "cd {} && find . -mindepth 1 \\( -path './dev' -o -path './dev/*' \\) -prune -o -print0 \
             | LC_ALL=C sort -z \
             | cpio --null --create --format=newc --owner=0:0 --quiet --reproducible",
            root.display()
        );
        let out = Command::new("bash").arg("-c").arg(&script).output().unwrap();
        assert!(out.status.success(), "oracle cpio failed: {}", String::from_utf8_lossy(&out.stderr));
        out.stdout
    }

    #[test]
    fn rootfs_segment_matches_gnu_cpio_reproducible() {
        let tmp = std::env::temp_dir().join(format!("tdvmm-cpio-test-{}", std::process::id()));
        let root = tmp.join("root");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::create_dir_all(root.join("etc")).unwrap();
        fs::create_dir_all(root.join("dev")).unwrap();
        fs::write(root.join("dev/should_be_excluded"), b"x").unwrap();
        fs::write(root.join("bin/real"), b"hello-file-content\n").unwrap();
        // adjacent hardlinks
        fs::hard_link(root.join("bin/real"), root.join("bin/hard1")).unwrap();
        fs::hard_link(root.join("bin/real"), root.join("bin/hard2")).unwrap();
        // symlink
        std::os::unix::fs::symlink("busybox", root.join("bin/sym")).unwrap();
        // non-adjacent hardlink group
        fs::write(root.join("bin/aaa"), b"DATA").unwrap();
        fs::write(root.join("bin/mmm"), b"sep").unwrap();
        fs::hard_link(root.join("bin/aaa"), root.join("bin/zzz")).unwrap();
        fs::hard_link(root.join("bin/aaa"), root.join("bin/bbb")).unwrap();
        fs::write(root.join("etc/a.txt"), b"aaa\n").unwrap();
        fs::write(root.join("etc/z.txt"), b"zzz\n").unwrap();
        // an empty dir + nested
        fs::create_dir_all(root.join("var/lib")).unwrap();

        // The oracle records epoch mtimes, so touch every path to the build epoch
        // before running it (the emitter writes the epoch constant unconditionally).
        Command::new("find")
            .arg(&root)
            .args(["-exec", "touch", "-h", "-d", "@1785542400", "{}", "+"])
            .status()
            .unwrap();

        let mut mine = Cpio::new();
        write_rootfs_segment(&mut mine, &root).unwrap();
        let oracle = oracle_rootfs_cpio(&root);
        let _ = fs::remove_dir_all(&tmp);

        if mine.out != oracle {
            fs::create_dir_all("scratch").ok();
            fs::write("scratch/mine.cpio", &mine.out).ok();
            fs::write("scratch/oracle.cpio", &oracle).ok();
            // Localize the first difference for debugging.
            let n = mine.out.len().min(oracle.len());
            let first = (0..n).find(|&i| mine.out[i] != oracle[i]);
            panic!(
                "rootfs cpio mismatch: mine={} oracle={} first_diff={:?}",
                mine.out.len(),
                oracle.len(),
                first
            );
        }
    }
}
