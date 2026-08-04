//! The `.tdvmm` single-file artifact (format v1) — the OP-1a deliverable.
//!
//! A baked stack is ONE self-contained file: a plain, **uncompressed** outer TAR
//! whose members are, in this canonical order,
//!
//!   1. `manifest.json`     — the anchor set + per-member hashes + run-defaults
//!   2. `compose.lock.yml`  — the only compose file the guest sees (for `inspect`)
//!   3. `kernel`            — the ELF `vmlinux`
//!   4. `initramfs`         — the per-stack initramfs (already gzip-compressed)
//!
//! ## Why plain tar, hand-rolled
//!
//! A custom sectioned container was rejected (Fable-locked): a standard tar is
//! debuggable with `tar tvf` / `tar xf`. We hand-roll a **deterministic** USTAR
//! writer rather than pull a tar crate, for two reasons: (1) total control over
//! the byte layout, so identical inputs produce a **byte-identical** `.tdvmm`
//! (fixed `mtime=0`, `uid=0`, `gid=0`, fixed mode, fixed member order, no PAX/GNU
//! extensions, no volatile fields); and (2) cheap single-member access — `inspect`
//! reads only the first member (`manifest.json`) and stops, never touching the big
//! `kernel`/`initramfs` payloads.
//!
//! ## Identity
//!
//! The artifact's identity is the **sha256 of the whole file** (there is no
//! embedded self-hash — that would be chicken-and-egg). `manifest.json` records a
//! sha256 for every *other* member; `verify` recomputes those and closes the loop,
//! and also prints the whole-file sha256 as the identity.
//!
//! ## Reserved member prefixes (unused in v1)
//!
//! `scenario/`, `record.log`, and `snapshot/` are reserved for later phases
//! (fault-injection scenarios, the record log, VM snapshots). v1 emits none of
//! them; the reader ignores any it does not recognize.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

// ---- canonical member names ------------------------------------------------
pub const MEMBER_MANIFEST: &str = "manifest.json";
pub const MEMBER_COMPOSE_LOCK: &str = "compose.lock.yml";
pub const MEMBER_KERNEL: &str = "kernel";
pub const MEMBER_INITRAMFS: &str = "initramfs";

/// The current on-disk format version. Bumped only on an incompatible change to
/// the tar member set / manifest schema.
pub const FORMAT_VERSION: u32 = 1;

fn default_format_version() -> u32 {
    FORMAT_VERSION
}

// ============================================================================
// manifest.json
// ============================================================================

/// One non-manifest member's integrity record. `manifest.json` itself is NOT
/// listed (no self-hash — see the module note); `verify` recomputes each of these.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Member {
    pub name: String,
    pub size: u64,
    pub sha256: String,
}

/// The pinned Docker Compose v2 engine baked into the guest.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ComposeEngine {
    pub version: String,
    pub sha256: String,
}

/// One image the bake resolved: its upstream digest ref, the pinned ref the guest
/// actually runs, the squash/build/plain policy, and (for squashed/built images)
/// the reproducible content identity.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImagePin {
    pub upstream: String,
    pub pinned: String,
    pub policy: String,
    #[serde(default)]
    pub content_id: String,
    #[serde(default)]
    pub size_mib: u64,
}

/// Bake-toolchain pins that produced the artifact. Every field is a DECLARED
/// input (identical on every host), never host-probed — Fable guardrail §3: no
/// host-probed value (tool versions, paths, uname, timestamps) may enter the
/// hashed artifact bytes. What pins the bake toolchain is the set of
/// `@sha256`-pinned BUILDER IMAGES below.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Toolchain {
    /// The pinned builder-image refs (`image@sha256`) that produced the guest
    /// binaries: the musl agent builder + the kernel builder. Sorted, so the
    /// bytes are order-stable. A toolchain bump = a changed digest here.
    #[serde(default)]
    pub builders: Vec<String>,
    #[serde(default)]
    pub alpine: String,
    #[serde(default)]
    pub compose: String,
}

/// The full anchor set: everything that pins WHAT this stack is, beyond the raw
/// member hashes. A host/CPU change surfaces as a changed `cpuid_*`; an image or
/// engine change surfaces here too — so a changed anchor changes the manifest,
/// hence the whole-file sha256.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Anchors {
    /// sha256 of the `cpuid_profile` text below (matches `guest/manifest.txt`).
    pub cpuid_sha256: String,
    /// The effective guest clock/timer CPUID profile (`tdvmm dump-cpuid` output).
    pub cpuid_profile: String,
    pub compose_engine: ComposeEngine,
    #[serde(default)]
    pub images: Vec<ImagePin>,
    #[serde(default)]
    pub toolchain: Toolchain,
    /// The bake's guest-RAM estimate (MiB).
    pub ram_estimate_mib: u64,
    /// sha256 of the baked static `tdvmm-agent` binary (Fable §2). `default` so
    /// pre-agent-anchor artifacts still deserialize.
    #[serde(default)]
    pub agent_sha256: String,
    /// The build hash the baked agent reports over `ping`/hello — the run-time
    /// compatibility oracle (Fable §4). Matches `agent_build_hash` in the ping log.
    #[serde(default)]
    pub agent_build_hash: String,
}

/// The baked run-defaults: the config `tdvmm run` applies unless a CLI flag
/// overrides it (baked < flag — see `main.rs`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunDefaults {
    pub mem_mib: u64,
    pub cmdline: String,
    pub fast_forward: bool,
    /// Virtual-time horizon (a duration string, e.g. `"36h"`); `null` = unbounded.
    #[serde(default)]
    pub max_virtual_time: Option<String>,
}

/// The whole `manifest.json`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    #[serde(default = "default_format_version")]
    pub format_version: u32,
    pub stack: String,
    pub project: String,
    /// Per-member integrity records (every member EXCEPT `manifest.json`).
    #[serde(default)]
    pub members: Vec<Member>,
    pub anchors: Anchors,
    pub run_defaults: RunDefaults,
}

impl Manifest {
    /// Parse a `manifest.json` byte buffer.
    pub fn from_bytes(bytes: &[u8]) -> Result<Manifest, ArtifactError> {
        serde_json::from_slice(bytes).map_err(|e| ArtifactError(format!("parsing manifest.json: {e}")))
    }

    /// Serialize to **canonical** manifest bytes: pretty JSON, struct field order
    /// fixed by the type, trailing newline. Deterministic for identical data —
    /// this is what makes the `.tdvmm` bit-reproducible.
    pub fn to_canonical_json(&self) -> Result<Vec<u8>, ArtifactError> {
        let mut v = serde_json::to_vec_pretty(self)
            .map_err(|e| ArtifactError(format!("serializing manifest.json: {e}")))?;
        v.push(b'\n');
        Ok(v)
    }

    /// Look up a member's recorded hash by name.
    pub fn member(&self, name: &str) -> Option<&Member> {
        self.members.iter().find(|m| m.name == name)
    }
}

// ============================================================================
// errors
// ============================================================================

#[derive(Debug)]
pub struct ArtifactError(pub String);
impl std::fmt::Display for ArtifactError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for ArtifactError {}

fn ioerr(ctx: &str, e: std::io::Error) -> ArtifactError {
    ArtifactError(format!("{ctx}: {e}"))
}

// ============================================================================
// sha256
// ============================================================================

/// Hex-encoded sha256 of a byte slice.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex(&h.finalize())
}

/// Hex-encoded sha256 of a whole file, streamed (the `.tdvmm` identity).
pub fn file_sha256_hex(path: &str) -> Result<String, ArtifactError> {
    let mut f = std::fs::File::open(path).map_err(|e| ioerr(&format!("opening {path}"), e))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).map_err(|e| ioerr(&format!("reading {path}"), e))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
    }
    s
}

// ============================================================================
// artifact store + name resolution
// ============================================================================

/// The artifact store directory: `<cache>/artifacts`, where `<cache>` is
/// `$TDVMM_CACHE_DIR` (if set and non-empty) else `$HOME/.tdvmm`. This mirrors
/// `tdvmm build`'s cache resolution minus the `--cache-dir` flag (which the
/// run/test/inspect/verify verbs do not expose), so `build` writes name-keyed
/// artifacts exactly where `run <name>` looks for them.
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
/// directory is not an error — it just means nothing has been built yet.
pub fn list_store() -> Result<Vec<StoreEntry>, ArtifactError> {
    list_in(&store_dir())
}

fn list_in(store: &Path) -> Result<Vec<StoreEntry>, ArtifactError> {
    let rd = match std::fs::read_dir(store) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(ioerr(&format!("reading store {}", store.display()), e)),
    };
    let mut out = Vec::new();
    for de in rd {
        let de = de.map_err(|e| ioerr("reading store entry", e))?;
        let path = de.path();
        let name = match path.file_name().and_then(|s| s.to_str()).and_then(|f| {
            f.strip_suffix(".tdvmm").map(str::to_string)
        }) {
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

/// Resolve an artifact argument to a path. A bare argument is a store NAME; a
/// filesystem path (anything containing `/`) is used as a path. See [`resolve_in`]
/// for the exact rules.
pub fn resolve(arg: &str) -> Result<PathBuf, ArtifactError> {
    resolve_in(&store_dir(), arg)
}

/// Core of [`resolve`], parameterized on the store dir so it is testable without
/// touching `$HOME` / `$TDVMM_CACHE_DIR`. Name-first (Docker-like), in order:
///   1. `arg` looks like a path (has a `/`)  → it is a path, never a name: use it
///      if the file exists, else error `no such artifact file`. A bare name never
///      shadows or is shadowed by a CWD file — to point at a file on disk, write a
///      path (`./x.tdvmm` or an absolute path).
///   2. otherwise `arg` is a store name      → `<store>/<name>.tdvmm` (a trailing
///      `.tdvmm` on the name is accepted), erroring with the list of available
///      names on a miss.
pub fn resolve_in(store: &Path, arg: &str) -> Result<PathBuf, ArtifactError> {
    if arg.contains('/') {
        if Path::new(arg).is_file() {
            return Ok(PathBuf::from(arg));
        }
        return Err(ArtifactError(format!("no such artifact file: {arg}")));
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
    Err(ArtifactError(format!(
        "no artifact named {name:?} in {}\navailable:\n{names}",
        store.display()
    )))
}

// ============================================================================
// deterministic USTAR tar
// ============================================================================

const BLOCK: usize = 512;

/// Build one deterministic USTAR header for `(name, size)`. All ownership/time
/// fields are pinned to zero and the mode to 0644, so the header — and hence the
/// whole archive — is a pure function of the member names + sizes + contents.
fn ustar_header(name: &str, size: u64) -> [u8; BLOCK] {
    let mut h = [0u8; BLOCK];
    // name[0..100]
    let nb = name.as_bytes();
    h[..nb.len()].copy_from_slice(nb);
    // mode[100..108] = 0000644
    write_octal(&mut h[100..108], 0o644, 7);
    // uid[108..116], gid[116..124] = 0
    write_octal(&mut h[108..116], 0, 7);
    write_octal(&mut h[116..124], 0, 7);
    // size[124..136]
    write_octal(&mut h[124..136], size, 11);
    // mtime[136..148] = 0 (fixed — no volatile timestamp)
    write_octal(&mut h[136..148], 0, 11);
    // chksum[148..156]: filled with spaces for the checksum computation.
    for b in &mut h[148..156] {
        *b = b' ';
    }
    // typeflag[156] = '0' (regular file)
    h[156] = b'0';
    // linkname[157..257] = 0
    // magic[257..263] = "ustar\0", version[263..265] = "00"
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    // uname/gname/dev*/prefix all left zero.
    // checksum = unsigned sum of all header bytes (with chksum as spaces).
    let sum: u32 = h.iter().map(|&b| b as u32).sum();
    // 6 octal digits, then NUL (written by write_octal at index 154), then a
    // space at 155 — the canonical GNU/BSD checksum field form.
    write_octal(&mut h[148..155], sum as u64, 6);
    h[155] = b' ';
    h
}

/// Write `val` as `digits` octal ASCII chars followed by a NUL, right into `field`
/// (which must be at least `digits + 1` wide). Zero-padded on the left.
fn write_octal(field: &mut [u8], val: u64, digits: usize) {
    let s = format!("{val:0width$o}", width = digits);
    let sb = s.as_bytes();
    field[..digits].copy_from_slice(&sb[..digits]);
    field[digits] = 0;
}

/// One member to write into the archive.
pub struct MemberInput<'a> {
    pub name: &'a str,
    pub data: &'a [u8],
}

/// Write a deterministic, uncompressed USTAR archive of `members` (in the given
/// order) to `out`. Ends with the two zero blocks the format requires.
pub fn write_tar<W: Write>(out: &mut W, members: &[MemberInput]) -> Result<(), ArtifactError> {
    for m in members {
        if m.name.len() > 100 {
            return Err(ArtifactError(format!(
                "member name {:?} exceeds 100 bytes (USTAR, no long-name extension)",
                m.name
            )));
        }
        let hdr = ustar_header(m.name, m.data.len() as u64);
        out.write_all(&hdr).map_err(|e| ioerr("writing tar header", e))?;
        out.write_all(m.data).map_err(|e| ioerr("writing tar data", e))?;
        let rem = m.data.len() % BLOCK;
        if rem != 0 {
            let pad = [0u8; BLOCK];
            out.write_all(&pad[..BLOCK - rem])
                .map_err(|e| ioerr("writing tar padding", e))?;
        }
    }
    let end = [0u8; BLOCK * 2];
    out.write_all(&end).map_err(|e| ioerr("writing tar trailer", e))?;
    Ok(())
}

/// A single parsed USTAR header: the member name, its content size, and the byte
/// offset of its content within the file.
struct Entry {
    name: String,
    size: u64,
    data_offset: u64,
}

/// Read the next USTAR header at the file's current position. Returns `Ok(None)`
/// at the end-of-archive zero block.
fn read_header<R: Read + Seek>(f: &mut R) -> Result<Option<Entry>, ArtifactError> {
    let mut hdr = [0u8; BLOCK];
    let n = read_full(f, &mut hdr)?;
    if n == 0 {
        return Ok(None); // clean EOF
    }
    if n < BLOCK {
        return Err(ArtifactError("truncated tar header".into()));
    }
    if hdr.iter().all(|&b| b == 0) {
        return Ok(None); // end-of-archive marker
    }
    let name = parse_str(&hdr[..100]);
    let size = parse_octal(&hdr[124..136])?;
    let data_offset = f
        .stream_position()
        .map_err(|e| ioerr("tar tell", e))?;
    Ok(Some(Entry {
        name,
        size,
        data_offset,
    }))
}

/// Advance past a member's (block-padded) content to the next header.
fn skip_content<R: Seek>(f: &mut R, size: u64) -> Result<(), ArtifactError> {
    let padded = size.div_ceil(BLOCK as u64) * BLOCK as u64;
    f.seek(SeekFrom::Current(padded as i64))
        .map_err(|e| ioerr("tar seek", e))?;
    Ok(())
}

fn read_content<R: Read + Seek>(f: &mut R, e: &Entry) -> Result<Vec<u8>, ArtifactError> {
    f.seek(SeekFrom::Start(e.data_offset))
        .map_err(|err| ioerr("tar seek", err))?;
    let mut buf = vec![0u8; e.size as usize];
    let n = read_full(f, &mut buf)?;
    if (n as u64) < e.size {
        return Err(ArtifactError(format!(
            "truncated content for member {:?}",
            e.name
        )));
    }
    // Position at the next header (past the padding).
    let padded = e.size.div_ceil(BLOCK as u64) * BLOCK as u64;
    f.seek(SeekFrom::Start(e.data_offset + padded))
        .map_err(|err| ioerr("tar seek", err))?;
    Ok(buf)
}

fn read_full<R: Read>(f: &mut R, buf: &mut [u8]) -> Result<usize, ArtifactError> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = f
            .read(&mut buf[filled..])
            .map_err(|e| ioerr("tar read", e))?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
}

fn parse_str(field: &[u8]) -> String {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).into_owned()
}

fn parse_octal(field: &[u8]) -> Result<u64, ArtifactError> {
    let s = parse_str(field);
    let s = s.trim_matches(|c| c == ' ' || c == '\0');
    if s.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(s, 8).map_err(|e| ArtifactError(format!("bad octal tar field {s:?}: {e}")))
}

// ============================================================================
// high-level readers
// ============================================================================

/// Read ONLY the `manifest.json` member, without touching `kernel`/`initramfs`.
/// This is what `inspect` uses — it stops at the first member (canonical order
/// puts `manifest.json` first), so it never pays for the big payloads.
pub fn read_manifest(path: &str) -> Result<Manifest, ArtifactError> {
    let mut f = std::fs::File::open(path).map_err(|e| ioerr(&format!("opening {path}"), e))?;
    while let Some(e) = read_header(&mut f)? {
        if e.name == MEMBER_MANIFEST {
            let bytes = read_content(&mut f, &e)?;
            return Manifest::from_bytes(&bytes);
        }
        skip_content(&mut f, e.size)?;
    }
    Err(ArtifactError(format!(
        "{path}: no {MEMBER_MANIFEST} member (not a .tdvmm artifact?)"
    )))
}

/// What `run` needs: the manifest plus the kernel + initramfs bytes read straight
/// into memory (NO extraction to a temp dir — the caller feeds these buffers to
/// the loader). `compose.lock.yml` is returned too so `run` can member-hash-verify
/// it on load.
pub struct RunPayload {
    pub manifest: Manifest,
    pub kernel: Vec<u8>,
    pub initramfs: Vec<u8>,
    pub compose_lock: Vec<u8>,
}

/// Read the members `run` needs. Streams once; captures manifest + the three
/// payload members; skips anything else (reserved prefixes).
pub fn read_for_run(path: &str) -> Result<RunPayload, ArtifactError> {
    let mut f = std::fs::File::open(path).map_err(|e| ioerr(&format!("opening {path}"), e))?;
    let mut manifest: Option<Manifest> = None;
    let mut kernel: Option<Vec<u8>> = None;
    let mut initramfs: Option<Vec<u8>> = None;
    let mut compose_lock: Option<Vec<u8>> = None;
    while let Some(e) = read_header(&mut f)? {
        match e.name.as_str() {
            MEMBER_MANIFEST => manifest = Some(Manifest::from_bytes(&read_content(&mut f, &e)?)?),
            MEMBER_KERNEL => kernel = Some(read_content(&mut f, &e)?),
            MEMBER_INITRAMFS => initramfs = Some(read_content(&mut f, &e)?),
            MEMBER_COMPOSE_LOCK => compose_lock = Some(read_content(&mut f, &e)?),
            _ => skip_content(&mut f, e.size)?, // reserved / unknown member
        }
    }
    Ok(RunPayload {
        manifest: manifest.ok_or_else(|| ArtifactError(format!("{path}: missing {MEMBER_MANIFEST}")))?,
        kernel: kernel.ok_or_else(|| ArtifactError(format!("{path}: missing {MEMBER_KERNEL}")))?,
        initramfs: initramfs
            .ok_or_else(|| ArtifactError(format!("{path}: missing {MEMBER_INITRAMFS}")))?,
        compose_lock: compose_lock
            .ok_or_else(|| ArtifactError(format!("{path}: missing {MEMBER_COMPOSE_LOCK}")))?,
    })
}

/// One member's verification result.
pub struct MemberCheck {
    pub name: String,
    pub expected: String,
    pub actual: String,
    pub ok: bool,
}

/// The result of verifying an artifact: the whole-file identity plus a per-member
/// hash check against the manifest.
pub struct VerifyReport {
    pub file_sha256: String,
    pub checks: Vec<MemberCheck>,
    /// Members named in `manifest.members` that were absent from the archive.
    pub missing: Vec<String>,
}

impl VerifyReport {
    pub fn all_ok(&self) -> bool {
        self.missing.is_empty() && self.checks.iter().all(|c| c.ok)
    }
}

/// Recompute every non-manifest member's sha256 and compare it to the value
/// recorded in `manifest.json`; also compute the whole-file sha256 (the identity).
pub fn verify(path: &str) -> Result<VerifyReport, ArtifactError> {
    let file_sha256 = file_sha256_hex(path)?;
    let mut f = std::fs::File::open(path).map_err(|e| ioerr(&format!("opening {path}"), e))?;
    let mut manifest: Option<Manifest> = None;
    // Collect actual hashes for every member present (except manifest.json).
    let mut actual: Vec<(String, String)> = Vec::new();
    while let Some(e) = read_header(&mut f)? {
        let bytes = read_content(&mut f, &e)?;
        if e.name == MEMBER_MANIFEST {
            manifest = Some(Manifest::from_bytes(&bytes)?);
        } else {
            actual.push((e.name.clone(), sha256_hex(&bytes)));
        }
    }
    let manifest =
        manifest.ok_or_else(|| ArtifactError(format!("{path}: missing {MEMBER_MANIFEST}")))?;

    let mut checks = Vec::new();
    let mut missing = Vec::new();
    for m in &manifest.members {
        match actual.iter().find(|(n, _)| n == &m.name) {
            Some((_, act)) => checks.push(MemberCheck {
                name: m.name.clone(),
                expected: m.sha256.clone(),
                actual: act.clone(),
                ok: *act == m.sha256,
            }),
            None => missing.push(m.name.clone()),
        }
    }
    Ok(VerifyReport {
        file_sha256,
        checks,
        missing,
    })
}

// ============================================================================
// packing (used by `tdvmm build` — see build.rs::pack_tdvmm)
// ============================================================================

/// Assemble a `.tdvmm` from its parts. `manifest_in` is a (possibly partial)
/// manifest JSON produced by the bake script; this function fills in
/// `format_version` and the per-member hash records from the ACTUAL bytes, then
/// re-serializes the manifest **canonically** — so the producer's key order /
/// whitespace never affects the output, and identical inputs give a byte-identical
/// `.tdvmm`.
pub fn pack(
    manifest_in: &[u8],
    kernel: &[u8],
    initramfs: &[u8],
    compose_lock: &[u8],
) -> Result<Vec<u8>, ArtifactError> {
    let mut manifest = Manifest::from_bytes(manifest_in)?;
    manifest.format_version = FORMAT_VERSION;
    // Per-member integrity records, in canonical (non-manifest) member order.
    manifest.members = vec![
        Member {
            name: MEMBER_COMPOSE_LOCK.into(),
            size: compose_lock.len() as u64,
            sha256: sha256_hex(compose_lock),
        },
        Member {
            name: MEMBER_KERNEL.into(),
            size: kernel.len() as u64,
            sha256: sha256_hex(kernel),
        },
        Member {
            name: MEMBER_INITRAMFS.into(),
            size: initramfs.len() as u64,
            sha256: sha256_hex(initramfs),
        },
    ];
    let manifest_json = manifest.to_canonical_json()?;

    let members = [
        MemberInput {
            name: MEMBER_MANIFEST,
            data: &manifest_json,
        },
        MemberInput {
            name: MEMBER_COMPOSE_LOCK,
            data: compose_lock,
        },
        MemberInput {
            name: MEMBER_KERNEL,
            data: kernel,
        },
        MemberInput {
            name: MEMBER_INITRAMFS,
            data: initramfs,
        },
    ];
    let mut out = Vec::with_capacity(kernel.len() + initramfs.len() + compose_lock.len() + 4096);
    write_tar(&mut out, &members)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn sample_manifest() -> Manifest {
        Manifest {
            format_version: FORMAT_VERSION,
            stack: "dogfood".into(),
            project: "tdvmm_dogfood".into(),
            members: vec![],
            anchors: Anchors {
                cpuid_sha256: "abc".into(),
                cpuid_profile: "# profile\n0x1 ...".into(),
                compose_engine: ComposeEngine {
                    version: "v5.3.1".into(),
                    sha256: "deadbeef".into(),
                },
                images: vec![ImagePin {
                    upstream: "docker.io/library/postgres@sha256:57c7".into(),
                    pinned: "localhost/tdvmm-postgres@sha256:cbf2".into(),
                    policy: "squash".into(),
                    content_id: "sha256:af5b".into(),
                    size_mib: 283,
                }],
                toolchain: Toolchain {
                    builders: vec![
                        "docker.io/library/debian@sha256:beef".into(),
                        "docker.io/library/rust@sha256:cafe".into(),
                    ],
                    alpine: "3.22.5".into(),
                    compose: "v5.3.1".into(),
                },
                ram_estimate_mib: 1742,
                agent_sha256: "sha256agent".into(),
                agent_build_hash: "deadbeefcafe0001".into(),
            },
            run_defaults: RunDefaults {
                mem_mib: 3072,
                cmdline: "console=ttyS0 tdvmm.stack=1".into(),
                fast_forward: true,
                max_virtual_time: None,
            },
        }
    }

    #[test]
    fn ustar_header_is_well_formed_and_checksum_valid() {
        let h = ustar_header("manifest.json", 1234);
        assert_eq!(&h[257..263], b"ustar\0");
        assert_eq!(&h[263..265], b"00");
        assert_eq!(h[156], b'0');
        // Recompute the checksum with the field blanked, compare to stored.
        let mut probe = h;
        for b in &mut probe[148..156] {
            *b = b' ';
        }
        let sum: u32 = probe.iter().map(|&b| b as u32).sum();
        let stored = parse_octal(&h[148..154]).unwrap();
        assert_eq!(sum as u64, stored);
        assert_eq!(parse_octal(&h[124..136]).unwrap(), 1234);
    }

    #[test]
    fn pack_roundtrips_and_is_byte_identical() {
        let mut m = sample_manifest();
        m.members.clear(); // pack fills these
        let min = m.to_canonical_json().unwrap();
        let kernel = b"\x7fELF fake kernel bytes".to_vec();
        let initramfs = b"\x1f\x8b fake gzip initramfs".to_vec();
        let lock = b"name: tdvmm_dogfood\n".to_vec();

        let a = pack(&min, &kernel, &initramfs, &lock).unwrap();
        let b = pack(&min, &kernel, &initramfs, &lock).unwrap();
        // Determinism: identical inputs -> byte-identical artifact.
        assert_eq!(a, b, "pack must be byte-deterministic");
        assert_eq!(sha256_hex(&a), sha256_hex(&b));

        // The archive begins with the manifest header (canonical order).
        assert_eq!(parse_str(&a[..100]), MEMBER_MANIFEST);

        // Round-trip via the readers.
        std::fs::create_dir_all("target/test-artifacts").ok();
        let p = "target/test-artifacts/roundtrip.tdvmm";
        std::fs::write(p, &a).unwrap();

        let man = read_manifest(p).unwrap();
        assert_eq!(man.stack, "dogfood");
        assert_eq!(man.members.len(), 3);
        assert_eq!(man.member(MEMBER_KERNEL).unwrap().sha256, sha256_hex(&kernel));

        let payload = read_for_run(p).unwrap();
        assert_eq!(payload.kernel, kernel);
        assert_eq!(payload.initramfs, initramfs);
        assert_eq!(payload.compose_lock, lock);

        let report = verify(p).unwrap();
        assert!(report.all_ok(), "fresh artifact must verify");
        assert_eq!(report.file_sha256, sha256_hex(&a));
        assert_eq!(report.checks.len(), 3);
    }

    #[test]
    fn verify_catches_a_flipped_byte() {
        let m = sample_manifest();
        let min = m.to_canonical_json().unwrap();
        let kernel = b"kernel-payload-aaaaaaaaaaaaaaaa".to_vec();
        let initramfs = b"initramfs-payload-bbbbbbbbbbbb".to_vec();
        let lock = b"name: x\n".to_vec();
        let mut a = pack(&min, &kernel, &initramfs, &lock).unwrap();

        // Find the kernel content in the archive and flip a byte of it.
        let pos = a
            .windows(kernel.len())
            .position(|w| w == kernel.as_slice())
            .expect("kernel bytes present");
        a[pos + 3] ^= 0xff;

        std::fs::create_dir_all("target/test-artifacts").ok();
        let p = "target/test-artifacts/corrupt.tdvmm";
        std::fs::write(p, &a).unwrap();

        let report = verify(p).unwrap();
        assert!(!report.all_ok(), "a flipped byte must fail verify");
        let k = report
            .checks
            .iter()
            .find(|c| c.name == MEMBER_KERNEL)
            .unwrap();
        assert!(!k.ok);
        assert_ne!(k.actual, k.expected);
    }

    #[test]
    fn canonical_json_is_stable_regardless_of_input_order() {
        // Two JSON encodings of the same manifest with different key order must
        // normalize to identical canonical bytes.
        let ordered = r#"{"format_version":1,"stack":"s","project":"p",
            "anchors":{"cpuid_sha256":"h","cpuid_profile":"prof",
              "compose_engine":{"version":"v","sha256":"e"},
              "images":[],"toolchain":{},"ram_estimate_mib":10},
            "run_defaults":{"mem_mib":3072,"cmdline":"c","fast_forward":true,"max_virtual_time":null}}"#;
        let shuffled = r#"{"project":"p","stack":"s",
            "run_defaults":{"cmdline":"c","fast_forward":true,"mem_mib":3072},
            "anchors":{"ram_estimate_mib":10,"compose_engine":{"sha256":"e","version":"v"},
              "cpuid_profile":"prof","cpuid_sha256":"h"},
            "format_version":1}"#;
        let a = Manifest::from_bytes(ordered.as_bytes())
            .unwrap()
            .to_canonical_json()
            .unwrap();
        let b = Manifest::from_bytes(shuffled.as_bytes())
            .unwrap()
            .to_canonical_json()
            .unwrap();
        assert_eq!(a, b, "canonical JSON must not depend on input key order");
    }

    #[test]
    fn read_header_stops_at_trailer() {
        let members = [MemberInput {
            name: "only",
            data: b"hello",
        }];
        let mut buf = Vec::new();
        write_tar(&mut buf, &members).unwrap();
        let mut c = Cursor::new(buf);
        let e = read_header(&mut c).unwrap().unwrap();
        assert_eq!(e.name, "only");
        assert_eq!(e.size, 5);
        let data = read_content(&mut c, &e).unwrap();
        assert_eq!(&data, b"hello");
        assert!(read_header(&mut c).unwrap().is_none(), "trailer -> None");
    }

    #[test]
    fn resolve_in_rules() {
        let store = std::path::PathBuf::from("target/test-artifacts/resolve-test");
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

        // a path-like arg (contains `/`) that does not exist is a file error, not
        // a name lookup.
        let e = resolve_in(&store, "some/dir/x.tdvmm").unwrap_err().to_string();
        assert!(e.contains("no such artifact file"), "got: {e}");

        // a path (contains `/`) that exists resolves as a file — this is the only
        // way to point at a file on disk.
        let loose = store.join("loose-file.tdvmm");
        std::fs::write(&loose, b"y").unwrap();
        assert_eq!(resolve_in(&store, loose.to_str().unwrap()).unwrap(), loose);

        // NAME-FIRST, no shadowing: a bare name maps strictly to `<store>/<name>.tdvmm`
        // and never to a same-named file. `gamma` (a plain file, no `.tdvmm`) sitting in
        // the store does NOT satisfy the bare name `gamma`; only `gamma.tdvmm` would.
        std::fs::write(store.join("gamma"), b"z").unwrap();
        let e = resolve_in(&store, "gamma").unwrap_err().to_string();
        assert!(e.contains("no artifact named"), "bare name must not pick up a same-named file: {e}");

        // NAME-FIRST, no shadowing from the CURRENT DIRECTORY (regression guard): a
        // `<name>.tdvmm` in the CWD must NEVER be picked up — a bare arg resolves
        // strictly against the store. This is the case that flips RED if a file-wins
        // `if Path::new(arg).is_file()` branch is ever reintroduced ahead of the store
        // lookup. Cargo runs tests from the crate root, so the file lands there; the
        // guard removes it even if an assertion panics.
        struct CwdFileGuard(std::path::PathBuf);
        impl Drop for CwdFileGuard {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let _guard = CwdFileGuard(std::path::PathBuf::from("resolve-shadow-guard.tdvmm"));
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

        // store LACKS the name -> miss ERROR, not the CWD file. THIS is the assertion
        // a reintroduced file-wins branch would break.
        let empty = std::path::PathBuf::from("target/test-artifacts/resolve-shadow-empty");
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
        let store = std::path::PathBuf::from("target/test-artifacts/list-test");
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
