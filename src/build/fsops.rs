//! In-process filesystem helpers (Move 3, Step D) — replace the host `cp -a` /
//! `chmod` / `install -D -m` shell-outs. The `.tdvmm` bytes come ONLY from the
//! normalizing cpio/artifact packers (Fable guardrail §2), so these helpers only
//! need to reproduce {file type, content, symlink target, permission bits}:
//! ownership, mtime, hardlink identity and sparseness are all normalized (or, for
//! hardlinks, reconstructed from dev/inode) by the packer and are NOT preserved
//! here. Seed layers + overlay + bind trees are plain files (Fable-locked).

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::artifact;
use super::util::sha256_file_hex;

/// `cp -a <src> <dst>` — recursively copy the single filesystem entity at `src`
/// to the path `dst` (directories recurse; symlinks are recreated as symlinks,
/// never followed; permission bits preserved). Directory modes are set only when
/// the directory is freshly created, so merging into an existing tree leaves that
/// tree's directory modes untouched (cp -a parity).
pub(super) fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(src)?;
    let ft = meta.file_type();
    if ft.is_symlink() {
        let target = std::fs::read_link(src)?;
        let _ = std::fs::remove_file(dst);
        std::os::unix::fs::symlink(target, dst)?;
    } else if ft.is_dir() {
        if !dst.exists() {
            std::fs::create_dir(dst)?;
            std::fs::set_permissions(dst, std::fs::Permissions::from_mode(meta.mode() & 0o7777))?;
        }
        for de in std::fs::read_dir(src)? {
            let de = de?;
            copy_tree(&de.path(), &dst.join(de.file_name()))?;
        }
    } else {
        // regular file (block/char/fifo nodes never occur in our copied trees).
        std::fs::copy(src, dst)?;
    }
    Ok(())
}

/// `cp -a <src>/. <dst>/` — merge the CONTENTS of directory `src` into the
/// existing directory `dst` (recursively). Used for the overlay + bind trees.
pub(super) fn copy_dir_contents(src: &Path, dst: &Path) -> std::io::Result<()> {
    for de in std::fs::read_dir(src)? {
        let de = de?;
        copy_tree(&de.path(), &dst.join(de.file_name()))?;
    }
    Ok(())
}

/// `chmod <mode> <path>` — set exactly the given permission bits.
pub(super) fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// `install -D -m <mode> <src> <dst>` — create `dst`'s parent dirs, copy `src`'s
/// contents to `dst`, then set the mode.
pub(super) fn install_file(src: &Path, dst: &Path, mode: u32) -> std::io::Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(src, dst)?;
    set_mode(dst, mode)?;
    Ok(())
}

/// Extract an UNCOMPRESSED tar archive into `dest`, in-process via the pure-Rust
/// `tar` crate (Move 3 — replaces the host `tar`). The archive is untrusted
/// TRANSPORT input (Fable guardrail §2): the `.tdvmm` bytes come solely from the
/// normalizing cpio packer, which re-derives uid/gid 0 + epoch + sorted order +
/// hardlink groups; only {file type, content, link target, permission bits} are
/// consumed here. Overwrites so a re-extract into a populated dir is idempotent.
pub(super) fn extract_tar(tarball: &Path, dest: &Path) -> std::io::Result<()> {
    let f = std::fs::File::open(tarball)?;
    let mut ar = tar::Archive::new(f);
    ar.set_preserve_permissions(true);
    ar.set_overwrite(true);
    ar.unpack(dest)?;
    Ok(())
}

/// Recursively list regular files under `dir` (no symlink following).
pub(super) fn walk_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        if !d.is_dir() {
            continue;
        }
        for e in std::fs::read_dir(&d)? {
            let p = e?.path();
            let meta = std::fs::symlink_metadata(&p)?;
            if meta.is_dir() {
                stack.push(p);
            } else if meta.is_file() {
                out.push(p);
            }
        }
    }
    Ok(out)
}

/// A stable content hash of a directory tree: for every regular file (recursively)
/// `<relpath>\0<sha256(content)>\n`, sorted by relpath, then sha256 of the whole.
/// `exclude` drops matching file BASENAMES (this bake's own committed outputs, so
/// the first bake does not bust its own key).
pub(super) fn tree_hash(root: &Path, exclude: &[&str]) -> std::io::Result<String> {
    if !root.exists() {
        return Ok(format!("MISSING:{}", root.display()));
    }
    let mut entries: Vec<(String, String)> = Vec::new();
    for path in walk_files(root)? {
        let base = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if exclude.contains(&base) {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        entries.push((rel, sha256_file_hex(&path)?));
    }
    entries.sort();
    let mut buf = String::new();
    for (rel, sha) in entries {
        buf.push_str(&rel);
        buf.push('\0');
        buf.push_str(&sha);
        buf.push('\n');
    }
    Ok(artifact::sha256_hex(buf.as_bytes()))
}

#[cfg(test)]
mod cache_tests {
    use super::*;
    use super::super::util::now_nanos;

    #[test]
    fn tree_hash_is_stable_and_content_sensitive() {
        let base = std::env::temp_dir().join(format!("tdvmm-th-{}-{}", std::process::id(), now_nanos()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("sub")).unwrap();
        std::fs::write(base.join("a.txt"), b"hello").unwrap();
        std::fs::write(base.join("sub/b.txt"), b"world").unwrap();

        let h1 = tree_hash(&base, &[]).unwrap();
        assert_eq!(h1, tree_hash(&base, &[]).unwrap(), "same tree -> same hash");

        // an excluded output file must not affect the key.
        std::fs::write(base.join("stack.lock"), b"ignored").unwrap();
        assert_eq!(h1, tree_hash(&base, &["stack.lock"]).unwrap(), "excluded file ignored");

        // a content change must flip the key.
        std::fs::write(base.join("a.txt"), b"HELLO").unwrap();
        assert_ne!(h1, tree_hash(&base, &["stack.lock"]).unwrap(), "content change -> new hash");

        let _ = std::fs::remove_dir_all(&base);
    }
}
