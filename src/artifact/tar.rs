//! Deterministic USTAR codec for the `.tdvmm` container.
//!
//! ## Byte layout
//!
//! A 512-byte header per member, then the member content, zero-padded up to the
//! next 512-byte boundary; repeated for each member; then two all-zero blocks as
//! the end-of-archive trailer. The header pins every ownership and time field
//! (mode 0644, uid/gid 0, mtime 0, `ustar\0` magic + `00` version, no PAX/GNU
//! extensions), so a member's bytes are a pure function of its name, size, and
//! content — identical members produce byte-identical archive bytes.
//!
//! Writing is via [`write_member`] + [`write_trailer`]; wrap the sink in a
//! [`HashingWriter`] to get the whole-file sha256 in the same pass. Reading is a
//! strict subset: [`Entries`] validates the `ustar` magic and header checksum,
//! bounds every allocation against the file length, and lets callers pull a
//! member's bytes ([`Entries::read`]) or hash them without buffering
//! ([`Entries::sha256`]).

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use sha2::{Digest, Sha256};

use super::error::ArtifactError;

pub(super) const BLOCK: usize = 512;

/// The largest member a USTAR octal `size` field (11 digits) can hold:
/// `0o77777777777` bytes, ~8 GiB − 1. Members are checked against this at seal time.
pub(super) const MAX_MEMBER_SIZE: u64 = 0o77_777_777_777;

// ---- hashing ---------------------------------------------------------------

/// Hex-encoded sha256 of a byte slice.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex(&h.finalize())
}

/// Hex-encoded sha256 of a whole file, streamed — the `.tdvmm` identity as read
/// back from disk.
pub fn file_sha256_hex(path: impl AsRef<Path>) -> Result<String, ArtifactError> {
    let path = path.as_ref();
    let mut f = std::fs::File::open(path)
        .map_err(|e| ArtifactError::io(format!("opening {}", path.display()), e))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f
            .read(&mut buf)
            .map_err(|e| ArtifactError::io(format!("reading {}", path.display()), e))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// A [`Write`] that hashes and counts every byte on its way to the inner writer,
/// so a single streaming pass yields the whole-file sha256 and length with no
/// re-read.
pub(super) struct HashingWriter<W: Write> {
    inner: W,
    hasher: Sha256,
    len: u64,
}

impl<W: Write> HashingWriter<W> {
    pub(super) fn new(inner: W) -> Self {
        HashingWriter { inner, hasher: Sha256::new(), len: 0 }
    }
    /// The hex sha256 of everything written, and the total byte count.
    pub(super) fn finish(self) -> (String, u64) {
        (hex(&self.hasher.finalize()), self.len)
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.len += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

// ---- writing ---------------------------------------------------------------

/// Write one member — its deterministic header, content, and block padding — to `w`.
pub(super) fn write_member<W: Write>(w: &mut W, name: &str, data: &[u8]) -> Result<(), ArtifactError> {
    let hdr = ustar_header(name, data.len() as u64);
    w.write_all(&hdr).map_err(|e| ArtifactError::io("writing tar header", e))?;
    w.write_all(data).map_err(|e| ArtifactError::io("writing tar data", e))?;
    let rem = data.len() % BLOCK;
    if rem != 0 {
        let pad = [0u8; BLOCK];
        w.write_all(&pad[..BLOCK - rem])
            .map_err(|e| ArtifactError::io("writing tar padding", e))?;
    }
    Ok(())
}

/// Write the two all-zero blocks that mark end-of-archive.
pub(super) fn write_trailer<W: Write>(w: &mut W) -> Result<(), ArtifactError> {
    let end = [0u8; BLOCK * 2];
    w.write_all(&end).map_err(|e| ArtifactError::io("writing tar trailer", e))
}

/// Build one deterministic USTAR header for `(name, size)`. Ownership and time
/// fields are pinned (mode 0644, uid/gid 0, mtime 0), so the header — and thus the
/// archive — is a pure function of the member names, sizes, and contents.
fn ustar_header(name: &str, size: u64) -> [u8; BLOCK] {
    debug_assert!(name.len() <= 100, "member name {name:?} exceeds the USTAR 100-byte field");
    let mut h = [0u8; BLOCK];
    let nb = name.as_bytes();
    h[..nb.len()].copy_from_slice(nb); // name[0..100]
    write_octal(&mut h[100..108], 0o644, 7); // mode
    write_octal(&mut h[108..116], 0, 7); // uid
    write_octal(&mut h[116..124], 0, 7); // gid
    write_octal(&mut h[124..136], size, 11); // size
    write_octal(&mut h[136..148], 0, 11); // mtime
    for b in &mut h[148..156] {
        *b = b' '; // checksum field = spaces while the checksum is summed
    }
    h[156] = b'0'; // typeflag: regular file
    h[257..263].copy_from_slice(b"ustar\0"); // magic
    h[263..265].copy_from_slice(b"00"); // version
    let sum: u32 = h.iter().map(|&b| b as u32).sum();
    // Canonical checksum form: 6 octal digits, a NUL, then a space.
    write_octal(&mut h[148..155], sum as u64, 6);
    h[155] = b' ';
    h
}

/// Write `val` as `digits` octal ASCII chars followed by a NUL into `field` (at
/// least `digits + 1` wide), left-zero-padded. The seal-time size cap guarantees
/// `val` fits; the assert catches a caller that skipped it.
fn write_octal(field: &mut [u8], val: u64, digits: usize) {
    debug_assert!(
        3 * digits >= 64 || val < (1u64 << (3 * digits)),
        "octal value {val} overflows {digits} digits"
    );
    let s = format!("{val:0width$o}", width = digits);
    let sb = s.as_bytes();
    field[..digits].copy_from_slice(&sb[..digits]);
    field[digits] = 0;
}

// ---- reading ---------------------------------------------------------------

/// A parsed USTAR header: the member name, its content size, and the byte offset
/// of its content within the file.
#[derive(Debug)]
pub(super) struct Entry {
    pub(super) name: String,
    pub(super) size: u64,
    data_offset: u64,
}

/// An iterator over an archive's members. Each `next()` first seeks past the
/// previous member's padded content, so callers never track positions themselves;
/// [`Entries::read`]/[`Entries::sha256`] pull a member's bytes on demand.
pub(super) struct Entries<R: Read + Seek> {
    reader: R,
    file_len: u64,
    /// Offset of the next header (end of the previous member's padded content),
    /// or `None` once the trailer/EOF is reached.
    next_header: Option<u64>,
}

impl<R: Read + Seek> Entries<R> {
    pub(super) fn new(mut reader: R) -> Result<Self, ArtifactError> {
        let file_len = reader.seek(SeekFrom::End(0)).map_err(|e| ArtifactError::io("tar size", e))?;
        Ok(Entries { reader, file_len, next_header: Some(0) })
    }

    /// The member's content bytes.
    pub(super) fn read(&mut self, e: &Entry) -> Result<Vec<u8>, ArtifactError> {
        self.seek_to_content(e)?;
        let mut buf = vec![0u8; e.size as usize];
        self.read_exact_or_truncated(&mut buf, &e.name)?;
        Ok(buf)
    }

    /// The member's sha256, hashed in fixed-size chunks (never buffers the whole
    /// member).
    pub(super) fn sha256(&mut self, e: &Entry) -> Result<String, ArtifactError> {
        self.seek_to_content(e)?;
        let mut hasher = Sha256::new();
        let mut remaining = e.size;
        let mut buf = [0u8; 1 << 16];
        while remaining > 0 {
            let want = remaining.min(buf.len() as u64) as usize;
            self.read_exact_or_truncated(&mut buf[..want], &e.name)?;
            hasher.update(&buf[..want]);
            remaining -= want as u64;
        }
        Ok(hex(&hasher.finalize()))
    }

    fn seek_to_content(&mut self, e: &Entry) -> Result<(), ArtifactError> {
        if e.data_offset.checked_add(e.size).is_none_or(|end| end > self.file_len) {
            return Err(ArtifactError::malformed(format!(
                "member {:?} claims {} bytes past end of archive",
                e.name, e.size
            )));
        }
        self.reader
            .seek(SeekFrom::Start(e.data_offset))
            .map_err(|err| ArtifactError::io("tar seek", err))?;
        Ok(())
    }

    fn read_exact_or_truncated(&mut self, buf: &mut [u8], name: &str) -> Result<(), ArtifactError> {
        if read_full(&mut self.reader, buf)? < buf.len() {
            return Err(ArtifactError::malformed(format!("truncated content for member {name:?}")));
        }
        Ok(())
    }
}

impl<R: Read + Seek> Iterator for Entries<R> {
    type Item = Result<Entry, ArtifactError>;
    fn next(&mut self) -> Option<Self::Item> {
        let off = self.next_header.take()?;
        if let Err(e) = self.reader.seek(SeekFrom::Start(off)) {
            return Some(Err(ArtifactError::io("tar seek", e)));
        }
        match read_header(&mut self.reader) {
            Ok(None) => None, // trailer or clean EOF; next_header stays None
            Ok(Some(e)) => {
                let padded = e.size.div_ceil(BLOCK as u64) * BLOCK as u64;
                self.next_header = Some(e.data_offset + padded);
                Some(Ok(e))
            }
            Err(err) => Some(Err(err)),
        }
    }
}

/// Read the header at the reader's current position. `Ok(None)` at the
/// end-of-archive zero block or a clean EOF. Validates the `ustar` magic and the
/// header checksum, so a non-`.tdvmm` file fails clearly rather than as a stray
/// octal-parse error.
fn read_header<R: Read + Seek>(f: &mut R) -> Result<Option<Entry>, ArtifactError> {
    let mut hdr = [0u8; BLOCK];
    let n = read_full(f, &mut hdr)?;
    if n == 0 {
        return Ok(None); // clean EOF
    }
    if n < BLOCK {
        return Err(ArtifactError::malformed("truncated tar header"));
    }
    if hdr.iter().all(|&b| b == 0) {
        return Ok(None); // end-of-archive marker
    }
    if &hdr[257..262] != b"ustar" {
        return Err(ArtifactError::malformed("not a .tdvmm artifact (bad tar magic)"));
    }
    let stored = parse_octal(&hdr[148..156])?;
    let mut probe = hdr;
    for b in &mut probe[148..156] {
        *b = b' ';
    }
    let sum: u64 = probe.iter().map(|&b| b as u64).sum();
    if sum != stored {
        return Err(ArtifactError::malformed("corrupt tar header (checksum mismatch)"));
    }
    let name = parse_str(&hdr[..100]);
    let size = parse_octal(&hdr[124..136])?;
    let data_offset = f.stream_position().map_err(|e| ArtifactError::io("tar tell", e))?;
    Ok(Some(Entry { name, size, data_offset }))
}

fn read_full<R: Read>(f: &mut R, buf: &mut [u8]) -> Result<usize, ArtifactError> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = f.read(&mut buf[filled..]).map_err(|e| ArtifactError::io("tar read", e))?;
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
    u64::from_str_radix(s, 8).map_err(|e| ArtifactError::malformed(format!("bad octal tar field {s:?}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn ustar_header_is_well_formed_and_checksum_valid() {
        let h = ustar_header("manifest.json", 1234);
        assert_eq!(&h[257..263], b"ustar\0");
        assert_eq!(&h[263..265], b"00");
        assert_eq!(h[156], b'0');
        let mut probe = h;
        for b in &mut probe[148..156] {
            *b = b' ';
        }
        let sum: u32 = probe.iter().map(|&b| b as u32).sum();
        let stored = parse_octal(&h[148..156]).unwrap();
        assert_eq!(sum as u64, stored);
        assert_eq!(parse_octal(&h[124..136]).unwrap(), 1234);
    }

    #[test]
    fn entries_iterates_and_reads() {
        let mut buf = Vec::new();
        write_member(&mut buf, "only", b"hello").unwrap();
        write_trailer(&mut buf).unwrap();
        let mut es = Entries::new(Cursor::new(buf)).unwrap();
        let e = es.next().unwrap().unwrap();
        assert_eq!(e.name, "only");
        assert_eq!(e.size, 5);
        assert_eq!(es.read(&e).unwrap(), b"hello");
        assert_eq!(es.sha256(&e).unwrap(), sha256_hex(b"hello"));
        assert!(es.next().is_none(), "trailer -> end of iteration");
    }

    #[test]
    fn rejects_non_tar_bytes() {
        let junk = vec![0x42u8; BLOCK]; // no ustar magic
        let mut es = Entries::new(Cursor::new(junk)).unwrap();
        let err = es.next().unwrap().unwrap_err().to_string();
        assert!(err.contains("bad tar magic"), "got: {err}");
    }

    #[test]
    fn rejects_oversized_member_claim() {
        // A valid header whose size field claims far more than the file holds.
        let mut buf = Vec::new();
        write_member(&mut buf, "only", b"hello").unwrap();
        write_trailer(&mut buf).unwrap();
        let mut es = Entries::new(Cursor::new(buf)).unwrap();
        let mut e = es.next().unwrap().unwrap();
        e.size = 1 << 40; // 1 TiB, well past the tiny file
        let err = es.read(&e).unwrap_err().to_string();
        assert!(err.contains("past end of archive"), "got: {err}");
    }
}
