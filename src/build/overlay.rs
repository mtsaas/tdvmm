//! The guest overlay (init, inittab, podman config, launcher scripts), embedded
//! into the binary so `tdvmm build` needs no source checkout.
//!
//! BYTE-IDENTITY CRITICAL: these files are copied into every baked rootfs and
//! their {relpath, content, permission bits} enter the cpio bytes. The modes are
//! PINNED here (not inherited from a checkout), matching exactly what a clean
//! checkout produces: 0755 for init + the launcher scripts, 0644 for the config
//! files, 0755 for freshly-created directories.

use std::path::Path;

use crate::artifact;
use super::fsops::set_mode;

/// One embedded overlay file: rootfs-relative path, pinned mode, content.
struct OverlayFile {
    path: &'static str,
    mode: u32,
    bytes: &'static [u8],
}

macro_rules! overlay_file {
    ($path:literal, $mode:literal) => {
        OverlayFile {
            path: $path,
            mode: $mode,
            bytes: include_bytes!(concat!("../../guest/initramfs-alpine/overlay/", $path)),
        }
    };
}

const OVERLAY_FILES: &[OverlayFile] = &[
    overlay_file!("init", 0o755),
    overlay_file!("etc/inittab", 0o644),
    overlay_file!("etc/containers/containers.conf", 0o644),
    overlay_file!("etc/containers/storage.conf", 0o644),
    overlay_file!("usr/local/bin/compose-up.sh", 0o755),
    overlay_file!("usr/local/bin/container-selftest.sh", 0o755),
    overlay_file!("usr/local/bin/healthcheck-ticker.sh", 0o755),
];

/// The overlay's directories, parents first. Materialized like `cp -a` merging
/// into the base rootfs: created (mode 0755) only where missing, existing base
/// directories (e.g. `/etc` from the Alpine tar) are left untouched.
const OVERLAY_DIRS: &[&str] = &["etc", "etc/containers", "usr", "usr/local", "usr/local/bin"];

/// Write the embedded overlay into `rootfs`, reproducing the retired
/// checkout-copy byte-for-byte: same relpaths, same contents, same effective
/// modes (pinned above, so no umask or checkout state can perturb them).
pub(super) fn materialize(rootfs: &Path) -> std::io::Result<()> {
    for dir in OVERLAY_DIRS {
        let d = rootfs.join(dir);
        if !d.exists() {
            std::fs::create_dir(&d)?;
            set_mode(&d, 0o755)?;
        }
    }
    for f in OVERLAY_FILES {
        let dst = rootfs.join(f.path);
        std::fs::write(&dst, f.bytes)?;
        set_mode(&dst, f.mode)?;
    }
    Ok(())
}

/// A stable identity of the embedded overlay for the cache keys: sha256 over
/// `<relpath>\0<mode>\0<sha256(content)>\n` per file, in table order (the table
/// is fixed at compile time, so the order is stable).
pub(super) fn overlay_id() -> String {
    let mut buf = String::new();
    for f in OVERLAY_FILES {
        buf.push_str(f.path);
        buf.push('\0');
        buf.push_str(&format!("{:o}", f.mode));
        buf.push('\0');
        buf.push_str(&artifact::sha256_hex(f.bytes));
        buf.push('\n');
    }
    artifact::sha256_hex(buf.as_bytes())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use super::super::util::now_nanos;

    /// The embedded table must reproduce the checkout overlay exactly — bytes,
    /// relpaths, and effective modes — or the baked cpio (and the `.tdvmm`)
    /// changes. Run from a checkout; the walk covers both directions (no file
    /// missing from the table, none extra).
    #[test]
    fn embedded_overlay_matches_checkout_bytes_and_modes() {
        let checkout = Path::new(env!("CARGO_MANIFEST_DIR")).join("guest/initramfs-alpine/overlay");
        let mut seen = 0;
        for f in OVERLAY_FILES {
            let on_disk = checkout.join(f.path);
            let bytes = std::fs::read(&on_disk).unwrap_or_else(|e| panic!("{}: {e}", on_disk.display()));
            assert_eq!(bytes, f.bytes, "content drift: {}", f.path);
            let mode = std::fs::metadata(&on_disk).unwrap().permissions().mode() & 0o7777;
            assert_eq!(mode, f.mode, "mode drift: {} (checkout {mode:o}, embedded {:o})", f.path, f.mode);
            seen += 1;
        }
        let on_disk_count = walkdir_count(&checkout);
        assert_eq!(seen, on_disk_count, "overlay file added/removed without updating OVERLAY_FILES");
    }

    #[test]
    fn materialize_pins_modes_regardless_of_umask() {
        let root = std::env::temp_dir().join(format!("tdvmm-overlay-{}-{}", std::process::id(), now_nanos()));
        std::fs::create_dir_all(&root).unwrap();
        materialize(&root).unwrap();
        for f in OVERLAY_FILES {
            let mode = std::fs::metadata(root.join(f.path)).unwrap().permissions().mode() & 0o7777;
            assert_eq!(mode, f.mode, "{}", f.path);
            assert_eq!(std::fs::read(root.join(f.path)).unwrap(), f.bytes, "{}", f.path);
        }
        for d in OVERLAY_DIRS {
            let mode = std::fs::metadata(root.join(d)).unwrap().permissions().mode() & 0o7777;
            assert_eq!(mode, 0o755, "{d}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    fn walkdir_count(dir: &Path) -> usize {
        let mut n = 0;
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    n += 1;
                }
            }
        }
        n
    }
}
