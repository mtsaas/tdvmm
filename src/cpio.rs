//! Deterministic newc (SVR4) cpio emitter for `dvmm build` (OP-1b).
//!
//! Replaces the retired shell tail of `build_rootfs.sh`
//! (`gen_init_cpio` + `cpio --create --format=newc --reproducible` +
//! `zero_cpio_inodes.py`), emitting the **combined initramfs cpio directly from
//! Rust** with the EXACT same bytes so the initramfs — and thus the `.dvmm` — is
//! byte-identical to the old producer.
//!
//! The archive is two concatenated newc segments (the kernel initramfs unpacker
//! reads concatenated cpios), each padded to a 512-byte boundary:
//!
//!   1. **device nodes** — `/dev`, `/dev/console`, `/dev/null`, `/dev/ttyS0`,
//!      `/dev/ttyS1` + a trailer. Matches `gen_init_cpio -t <epoch>` with every
//!      `c_ino` zeroed by `zero_cpio_inodes.py` (fixed fake device `3:1`).
//!   2. **the rootfs tree** — every path except `dev`/`dev/*`, in `LC_ALL=C`
//!      byte-sorted order, with `--owner=0:0` (uid/gid 0), device numbers zeroed,
//!      all mtimes pinned to the guest epoch, and `c_ino` **renumbered by
//!      first-appearance** exactly as GNU cpio's `--reproducible` does — including
//!      its hardlink handling: the non-last links of an inode group are written
//!      (in reverse order) with `filesize=0`, and the last link carries the data.
//!
//! All 13 newc header fields are 8-char uppercase hex; header+name and data are
//! each padded to a 4-byte boundary. Fidelity is pinned by `cpio_tests` against
//! GNU `cpio --reproducible` on synthetic trees.

use std::collections::HashMap;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// The fixed guest wall-clock epoch (2026-08-01T00:00:00Z) — every archived
/// entry's mtime, matching `build_rootfs.sh`'s `touch -h -d @$BUILD_EPOCH`.
pub const BUILD_EPOCH: u32 = 1785542400;

// libc file-type bits (S_IFMT masking).
const S_IFMT: u32 = 0o170000;
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;
const S_IFLNK: u32 = 0o120000;
const S_IFCHR: u32 = 0o020000;
const S_IFBLK: u32 = 0o060000;

struct Cpio {
    out: Vec<u8>,
}

impl Cpio {
    fn new() -> Cpio {
        Cpio { out: Vec::new() }
    }

    /// Write one newc header (13 uppercase-hex fields) + name + NUL + 4-byte pad.
    #[allow(clippy::too_many_arguments)]
    fn header(
        &mut self,
        ino: u32,
        mode: u32,
        nlink: u32,
        mtime: u32,
        filesize: u32,
        devmajor: u32,
        devminor: u32,
        rdevmajor: u32,
        rdevminor: u32,
        name: &[u8],
    ) {
        let namesize = name.len() as u32 + 1; // includes the trailing NUL
        self.out.extend_from_slice(b"070701");
        let fields = [
            ino, mode, 0, /*uid*/ 0, /*gid*/ nlink, mtime, filesize, devmajor, devminor,
            rdevmajor, rdevminor, namesize, 0, /*check*/
        ];
        for v in fields {
            self.out.extend_from_slice(format!("{v:08X}").as_bytes());
        }
        self.out.extend_from_slice(name);
        self.out.push(0);
        let hdrlen = 110 + namesize as usize;
        let pad = (4 - (hdrlen % 4)) % 4;
        self.out.extend(std::iter::repeat(0u8).take(pad));
    }

    /// Write member data padded to a 4-byte boundary.
    fn data(&mut self, d: &[u8]) {
        self.out.extend_from_slice(d);
        let pad = (4 - (d.len() % 4)) % 4;
        self.out.extend(std::iter::repeat(0u8).take(pad));
    }

    fn trailer(&mut self) {
        // ino=0, mode=0, nlink=1, mtime=0, filesize=0, dev=0:0, rdev=0:0.
        self.header(0, 0, 1, 0, 0, 0, 0, 0, 0, b"TRAILER!!!");
    }

    /// Pad the whole archive-so-far up to a 512-byte boundary (cpio block size /
    /// gen_init_cpio trailer padding).
    fn pad_to_512(&mut self) {
        let pad = (512 - (self.out.len() % 512)) % 512;
        self.out.extend(std::iter::repeat(0u8).take(pad));
    }
}

/// Emit the device-node segment: matches `gen_init_cpio -t epoch` output with
/// every c_ino zeroed. Fixed fake device `3:1`; trailer + pad to 512.
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
        // c_ino=0, mtime=epoch, filesize=0, devmajor=3 devminor=1.
        c.header(0, mode, nlink, BUILD_EPOCH, 0, 3, 1, rmaj, rmin, name);
    }
    c.trailer();
    c.pad_to_512();
}

/// One collected rootfs entry (lstat, no symlink following).
struct Entry {
    name: Vec<u8>, // relative path, no leading "./"
    path: PathBuf,
    dev: u64,
    ino: u64,
    nlink: u32,
    mode: u32,
    is_dir: bool,
}

/// Recursively collect all entries under `root`, excluding `dev` and `dev/*`
/// (provided by the node segment). Relative names, no leading "./".
fn collect(root: &Path) -> std::io::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    // We must NOT follow symlinks; recurse only into real directories.
    while let Some(dir) = stack.pop() {
        for de in fs::read_dir(&dir)? {
            let de = de?;
            let path = de.path();
            let rel = path.strip_prefix(root).unwrap();
            let relbytes = rel.as_os_str().as_bytes().to_vec();
            // exclude dev and everything under it
            if relbytes == b"dev" || relbytes.starts_with(b"dev/") {
                continue;
            }
            let meta = fs::symlink_metadata(&path)?;
            let mode = meta.mode();
            let is_dir = (mode & S_IFMT) == S_IFDIR;
            entries.push(Entry {
                name: relbytes,
                path: path.clone(),
                dev: meta.dev(),
                ino: meta.ino(),
                nlink: meta.nlink() as u32,
                mode,
                is_dir,
            });
            if is_dir {
                stack.push(path);
            }
        }
    }
    // LC_ALL=C sort on the ("./"-prefixed) names == byte sort of relative names.
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
}

/// The rdev fields for a device node, else (0,0).
fn rdev_of(meta_rdev: u64, mode: u32) -> (u32, u32) {
    let t = mode & S_IFMT;
    if t == S_IFCHR || t == S_IFBLK {
        // Linux dev_t: major = (rdev >> 8) & 0xfff (glibc-compatible split cpio uses).
        // GNU cpio writes the classic major/minor. Use the same split GNU cpio does.
        let major = ((meta_rdev >> 8) & 0xfff) as u32;
        let minor = (meta_rdev & 0xff) as u32 | (((meta_rdev >> 12) & !0xffu64) as u32);
        (major, minor)
    } else {
        (0, 0)
    }
}

/// Emit the rootfs segment: byte-sorted, uid/gid 0, dev zeroed, mtime=epoch,
/// c_ino renumbered by first-appearance, GNU-cpio hardlink handling.
fn write_rootfs_segment(c: &mut Cpio, root: &Path) -> std::io::Result<()> {
    let entries = collect(root)?;

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

    // Pass 1b: per hardlink group (non-dir, nlink>1), count archived members.
    let mut group_total: HashMap<(u64, u64), usize> = HashMap::new();
    for e in &entries {
        if !e.is_dir && e.nlink > 1 {
            *group_total.entry((e.dev, e.ino)).or_insert(0) += 1;
        }
    }

    // Pass 2: emit. Defer hardlink-group members until the last archived one, at
    // which point write the earlier ones (reversed, data-less) then this one
    // (with data) — exactly GNU cpio --reproducible.
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
            // Trigger: write reversed(deferred) with no data, then this with data.
            if let Some(list) = deferred.remove(&key) {
                for &di in list.iter().rev() {
                    write_entry(c, &entries[di], ino_map[&key], false)?;
                }
            }
            write_entry(c, e, ino_map[&key], true)?;
        } else {
            write_entry(c, e, ino_map[&key], true)?;
        }
    }

    // Any group that never reached its total (links outside the archived tree)
    // should not happen for our closed rootfs; surface it rather than diverge.
    for (_, list) in deferred.iter() {
        debug_assert!(list.is_empty(), "cpio: undrained hardlink deferral");
    }

    c.trailer();
    c.pad_to_512();
    Ok(())
}

/// Write a single rootfs entry. `with_data` is false for a non-last hardlink.
fn write_entry(c: &mut Cpio, e: &Entry, new_ino: u32, with_data: bool) -> std::io::Result<()> {
    let t = e.mode & S_IFMT;
    let meta = fs::symlink_metadata(&e.path)?;
    let (rmaj, rmin) = rdev_of(meta.rdev(), e.mode);

    let payload: Vec<u8> = if !with_data {
        Vec::new()
    } else if t == S_IFREG {
        fs::read(&e.path)?
    } else if t == S_IFLNK {
        fs::read_link(&e.path)?.as_os_str().as_bytes().to_vec()
    } else {
        Vec::new()
    };

    // GNU cpio --reproducible writes nlink=2 for every directory (it does not
    // preserve the subdir-dependent on-disk link count); non-dirs keep their real
    // link count (so hardlink groups carry the true count).
    let nlink = if e.is_dir { 2 } else { e.nlink };

    c.header(
        new_ino,
        e.mode,
        nlink,
        BUILD_EPOCH,
        payload.len() as u32,
        0, // devmajor (ignore_devno)
        0, // devminor
        rmaj,
        rmin,
        &e.name,
    );
    if !payload.is_empty() {
        c.data(&payload);
    }
    Ok(())
}

/// The device-nodes cpio segment (with its trailer + 512-pad), on its own. The
/// kernel initramfs unpacker reads CONCATENATED newc archives, so a full
/// initramfs can be assembled as `nodes_segment() + <base seg> + <stack seg>`
/// (Fable Part D: a reusable base-runtime segment + a per-stack segment).
pub fn nodes_segment() -> Vec<u8> {
    let mut c = Cpio::new();
    write_nodes_segment(&mut c);
    c.out
}

/// Emit ONE rootfs tree as a standalone cpio segment (byte-sorted, uid/gid 0,
/// dev zeroed, mtime=epoch, GNU-cpio-compatible inode renumbering + hardlink
/// handling, trailer + 512-pad). Deterministic: identical trees -> identical
/// bytes. Concatenate segments to form the final initramfs.
pub fn rootfs_segment(root: &Path) -> std::io::Result<Vec<u8>> {
    let mut c = Cpio::new();
    write_rootfs_segment(&mut c, root)?;
    Ok(c.out)
}

/// gzip an already-assembled (concatenated) cpio byte buffer to `out_path` via the
/// host `gzip -9 -n` (identical bytes to the scripts' compressor). Staged to a
/// temp file to avoid a producer/consumer pipe deadlock on large inputs.
pub fn gzip_to(combined: &[u8], out_path: &Path) -> std::io::Result<()> {
    let tmp = out_path.with_extension("cpio.tmp");
    fs::write(&tmp, combined)?;
    let in_file = fs::File::open(&tmp)?;
    let out_file = fs::File::create(out_path)?;
    let status = std::process::Command::new("gzip")
        .args(["-9", "-n"])
        .stdin(std::process::Stdio::from(in_file))
        .stdout(std::process::Stdio::from(out_file))
        .status()?;
    let _ = fs::remove_file(&tmp);
    if !status.success() {
        return Err(std::io::Error::other("gzip failed"));
    }
    Ok(())
}

#[cfg(test)]
mod cpio_tests {
    use super::*;
    use std::process::Command;

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
        let tmp = std::env::temp_dir().join(format!("dvmm-cpio-test-{}", std::process::id()));
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

        // The real pipeline touches every file to the build epoch before cpio, so
        // the oracle records epoch mtimes — match that here (our emitter writes the
        // epoch constant unconditionally).
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
            let mut first = None;
            for i in 0..n {
                if mine.out[i] != oracle[i] {
                    first = Some(i);
                    break;
                }
            }
            panic!(
                "rootfs cpio mismatch: mine={} oracle={} first_diff={:?}",
                mine.out.len(),
                oracle.len(),
                first
            );
        }
    }
}
