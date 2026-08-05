//! The artifact store: where `tdvmm build` writes name-keyed `.tdvmm` files and
//! where `run`/`test`/`inspect`/`verify`/`ls` look them up by name.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::error::ArtifactError;

/// The store directory: `<cache>/artifacts`, where `<cache>` is `$TDVMM_CACHE_DIR`
/// (if set and non-empty) else `$HOME/.tdvmm`. This mirrors `tdvmm build`'s cache
/// resolution minus the `--cache-dir` flag (which run/test/inspect/verify don't
/// expose), so `build` writes name-keyed artifacts exactly where `run <name>` reads.
pub fn store_dir() -> PathBuf {
    let cache = match std::env::var("TDVMM_CACHE_DIR") {
        Ok(d) if !d.is_empty() => PathBuf::from(d),
        _ => {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home).join(".tdvmm")
        }
    };
    cache.join("artifacts")
}

/// One `.tdvmm` in the store: short name (filename minus `.tdvmm`), path, size, mtime.
pub struct StoreEntry {
    pub name: String,
    pub path: PathBuf,
    pub size: u64,
    pub modified: SystemTime,
}

/// Enumerate the `*.tdvmm` artifacts in the store, sorted by name. A missing store
/// directory is not an error — nothing has been built yet.
pub fn list_store() -> Result<Vec<StoreEntry>, ArtifactError> {
    list_in(&store_dir())
}

fn list_in(store: &Path) -> Result<Vec<StoreEntry>, ArtifactError> {
    let rd = match std::fs::read_dir(store) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(ArtifactError::io(format!("reading store {}", store.display()), e)),
    };
    let mut out = Vec::new();
    for de in rd {
        let de = de.map_err(|e| ArtifactError::io("reading store entry", e))?;
        let path = de.path();
        let name = match path
            .file_name()
            .and_then(|s| s.to_str())
            .and_then(|f| f.strip_suffix(".tdvmm").map(str::to_string))
        {
            Some(n) => n,
            None => continue,
        };
        let md = match de.metadata() {
            Ok(m) if m.is_file() => m,
            _ => continue,
        };
        out.push(StoreEntry {
            name,
            path,
            size: md.len(),
            modified: md.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Resolve an artifact argument to a path. A bare argument is a store NAME; an
/// argument containing `/` is a filesystem path. See [`resolve_in`] for the rules.
pub fn resolve(arg: &str) -> Result<PathBuf, ArtifactError> {
    resolve_in(&store_dir(), arg)
}

/// Core of [`resolve`], parameterized on the store dir so it is testable without
/// touching `$HOME` / `$TDVMM_CACHE_DIR`. Name-first (Docker-like):
///   1. `arg` contains `/` → it is a path: use it if the file exists, else error.
///      A bare name is never shadowed by (nor shadows) a CWD file — to point at a
///      file on disk, write a path (`./x.tdvmm` or absolute).
///   2. otherwise `arg` is a store name → `<store>/<name>.tdvmm` (a trailing
///      `.tdvmm` on the name is accepted), erroring with the available names on a miss.
pub fn resolve_in(store: &Path, arg: &str) -> Result<PathBuf, ArtifactError> {
    if arg.contains('/') {
        if Path::new(arg).is_file() {
            return Ok(PathBuf::from(arg));
        }
        return Err(ArtifactError::no_such(format!("no such artifact file: {arg}")));
    }
    let name = arg.strip_suffix(".tdvmm").unwrap_or(arg);
    let candidate = store.join(format!("{name}.tdvmm"));
    if candidate.is_file() {
        return Ok(candidate);
    }
    let avail = list_in(store)?;
    let names = if avail.is_empty() {
        "  (store is empty)".to_string()
    } else {
        avail
            .iter()
            .map(|e| format!("  {}", e.name))
            .collect::<Vec<_>>()
            .join("\n")
    };
    Err(ArtifactError::no_such(format!(
        "no artifact named {name:?} in {}\navailable:\n{names}",
        store.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_in_rules() {
        let store = PathBuf::from("target/test-artifacts/resolve-test");
        let _ = std::fs::remove_dir_all(&store);
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(store.join("alpha.tdvmm"), b"x").unwrap();
        std::fs::write(store.join("beta.tdvmm"), b"x").unwrap();

        // name hit — bare and with the `.tdvmm` suffix both resolve to the store.
        assert_eq!(resolve_in(&store, "alpha").unwrap(), store.join("alpha.tdvmm"));
        assert_eq!(resolve_in(&store, "alpha.tdvmm").unwrap(), store.join("alpha.tdvmm"));

        // name miss lists the available names.
        let e = resolve_in(&store, "nope").unwrap_err().to_string();
        assert!(e.contains("alpha") && e.contains("beta"), "miss must list names: {e}");

        // a path-like arg (contains `/`) that does not exist is a file error.
        let e = resolve_in(&store, "some/dir/x.tdvmm").unwrap_err().to_string();
        assert!(e.contains("no such artifact file"), "got: {e}");

        // a path (contains `/`) that exists resolves as a file — the only way to
        // point at a file on disk.
        let loose = store.join("loose-file.tdvmm");
        std::fs::write(&loose, b"y").unwrap();
        assert_eq!(resolve_in(&store, loose.to_str().unwrap()).unwrap(), loose);

        // A bare name maps strictly to `<store>/<name>.tdvmm`, never to a same-named
        // plain file: `gamma` (no `.tdvmm`) in the store does not satisfy `gamma`.
        std::fs::write(store.join("gamma"), b"z").unwrap();
        let e = resolve_in(&store, "gamma").unwrap_err().to_string();
        assert!(e.contains("no artifact named"), "bare name must not pick up a same-named file: {e}");

        // Regression guard: a `<name>.tdvmm` in the CWD must never be picked up — a
        // bare arg resolves strictly against the store. This flips RED if a file-wins
        // branch is ever reintroduced ahead of the store lookup. Cargo runs tests
        // from the crate root; the guard removes the file even if an assertion panics.
        struct CwdFileGuard(PathBuf);
        impl Drop for CwdFileGuard {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let _guard = CwdFileGuard(PathBuf::from("resolve-shadow-guard.tdvmm"));
        std::fs::write("resolve-shadow-guard.tdvmm", b"cwd").unwrap();

        // store HAS the name -> resolves to the STORE copy, not the CWD file.
        std::fs::write(store.join("resolve-shadow-guard.tdvmm"), b"store").unwrap();
        assert_eq!(
            resolve_in(&store, "resolve-shadow-guard.tdvmm").unwrap(),
            store.join("resolve-shadow-guard.tdvmm"),
            "bare name must resolve to the store, never the CWD file"
        );
        assert_eq!(
            resolve_in(&store, "resolve-shadow-guard").unwrap(),
            store.join("resolve-shadow-guard.tdvmm"),
        );

        // store LACKS the name -> miss ERROR, not the CWD file.
        let empty = PathBuf::from("target/test-artifacts/resolve-shadow-empty");
        let _ = std::fs::remove_dir_all(&empty);
        std::fs::create_dir_all(&empty).unwrap();
        assert!(
            resolve_in(&empty, "resolve-shadow-guard.tdvmm").is_err(),
            "a CWD file must not satisfy a bare-name store lookup"
        );
        assert!(resolve_in(&empty, "resolve-shadow-guard").is_err());
    }

    #[test]
    fn list_in_filters_and_sorts() {
        let store = PathBuf::from("target/test-artifacts/list-test");
        let _ = std::fs::remove_dir_all(&store);
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(store.join("zeta.tdvmm"), b"12345").unwrap();
        std::fs::write(store.join("alpha.tdvmm"), b"1").unwrap();
        std::fs::write(store.join("notes.txt"), b"ignore me").unwrap();
        let got = list_in(&store).unwrap();
        let names: Vec<_> = got.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "zeta"]);
        assert_eq!(got[1].size, 5);
    }

    #[test]
    fn list_in_missing_dir_is_empty() {
        let got = list_in(Path::new("target/test-artifacts/does-not-exist-xyz")).unwrap();
        assert!(got.is_empty());
    }
}
