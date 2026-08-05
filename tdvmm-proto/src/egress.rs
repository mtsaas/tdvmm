//! `egress` — the binary mux wire protocol for the `--allow-egress` channel
//! (COM4 / ttyS3), the ONE source of truth for the frames the guest-side
//! forwarder and the host-side `EgressBackend` exchange.
//!
//! This is a SEPARATE contract from the line-JSON control channel ([`crate`]
//! root): the egress data plane carries opaque TCP payload bytes, so it is a
//! compact binary framing with NO JSON on the hot path.
//!
//! ## Frame layout
//!
//! Every frame is a fixed 6-byte header followed by a type-specific payload:
//!
//! ```text
//! ┌────────┬────────┬─────────────┬──────────┬───────────────┐
//! │ ver:u8 │ type:u8│ stream:u16  │ len:u16  │ payload[len]   │
//! └────────┴────────┴─────────────┴──────────┴───────────────┘
//! ```
//!
//! All integers are **little-endian**. `ver` is [`EGRESS_VERSION`]; a decoder
//! rejects any other value. `len` is the payload length (≤ [`EGRESS_MAX_PAYLOAD`]),
//! so a frame is self-delimiting and the stream is parsed one frame at a time.
//!
//! The seven frame types and their payloads:
//!
//! | type | name       | direction    | payload                              |
//! |------|------------|--------------|--------------------------------------|
//! | 1    | `OPEN`     | guest → host | `port:u16` then the hostname bytes   |
//! | 2    | `OPEN_OK`  | host → guest | empty                                |
//! | 3    | `OPEN_ERR` | host → guest | `reason:u8` (see [`EgressReason`])    |
//! | 4    | `DATA`     | both         | opaque TCP bytes                     |
//! | 5    | `CLOSE`    | both         | empty (half-close, one direction)    |
//! | 6    | `RST`      | both         | empty (abortive close)               |
//! | 7    | `WINDOW`   | guest → host | `credit:u32` (host→guest flow ctrl)  |
//!
//! `OPEN` carries a *hostname* (SOCKS5h semantics): the host resolves it, so the
//! guest needs no resolver. `WINDOW` grants the host more credit to send toward
//! the guest; the guest→host direction is bounded on the host with no credit
//! scheme (it RSTs on overrun instead).
//!
//! ## Versioning
//!
//! [`EGRESS_SCHEMA`] is the wire-contract version, sibling to the JSON channel's
//! [`crate::SCHEMA`]. ANY change to the codec — a new type, a payload layout, the
//! header shape — MUST bump it, and the golden byte-vector test in this module is
//! the tripwire that forces the bump (its hardcoded bytes change when the codec
//! changes, and the same test asserts the schema constant).

use std::fmt;

/// The wire-contract version of the egress mux. Bump on ANY codec change; the
/// golden test [`tests::golden_corpus_is_byte_stable`] fails until the bump and
/// the golden bytes are updated together.
pub const EGRESS_SCHEMA: u32 = 1;

/// The `ver` byte every frame carries. Moves in lockstep with [`EGRESS_SCHEMA`];
/// a decoder rejects any other value.
pub const EGRESS_VERSION: u8 = 1;

// The docs promise the frame version and the schema move together; enforce it at
// compile time rather than trusting the prose.
const _: () = assert!(EGRESS_VERSION as u32 == EGRESS_SCHEMA);

/// The fixed frame-header length: `ver + type + stream + len`.
pub const EGRESS_HEADER_LEN: usize = 6;

/// The largest payload a single frame can carry — the `len` field is a `u16`.
pub const EGRESS_MAX_PAYLOAD: usize = u16::MAX as usize;

/// The largest hostname an `OPEN` frame may carry (the DNS name limit). Bounds the
/// allocation a decoded `OPEN` implies.
pub const EGRESS_MAX_HOST_LEN: usize = 255;

// Frame type discriminants (the `type` byte).
const T_OPEN: u8 = 1;
const T_OPEN_OK: u8 = 2;
const T_OPEN_ERR: u8 = 3;
const T_DATA: u8 = 4;
const T_CLOSE: u8 = 5;
const T_RST: u8 = 6;
const T_WINDOW: u8 = 7;

/// Why an `OPEN` was refused, carried as a single byte in an `OPEN_ERR` payload.
/// Plain data shared by host and guest, exactly like [`crate::ErrorKind`]: an
/// unrecognised code decodes to [`EgressReason::Other`] so a forward-compatible
/// peer never fails to parse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EgressReason {
    /// The hostname did not resolve.
    ResolveFailed,
    /// The TCP connect was refused (`ECONNREFUSED`).
    ConnectRefused,
    /// The destination was unreachable / the connect otherwise failed.
    Unreachable,
    /// The concurrent-stream cap was reached; no socket was opened.
    StreamLimit,
    /// The peer violated the mux contract (bad framing / illegal transition).
    ProtocolError,
    /// A bounded buffer overflowed and the stream was reset.
    Overrun,
    /// A reason code this build does not recognise.
    Other(u8),
}

impl EgressReason {
    /// The stable wire byte for this reason.
    pub const fn code(self) -> u8 {
        match self {
            EgressReason::ResolveFailed => 0,
            EgressReason::ConnectRefused => 1,
            EgressReason::Unreachable => 2,
            EgressReason::StreamLimit => 3,
            EgressReason::ProtocolError => 4,
            EgressReason::Overrun => 5,
            EgressReason::Other(x) => x,
        }
    }

    /// Interpret a wire byte; an unknown code becomes [`EgressReason::Other`].
    pub const fn from_code(code: u8) -> Self {
        match code {
            0 => EgressReason::ResolveFailed,
            1 => EgressReason::ConnectRefused,
            2 => EgressReason::Unreachable,
            3 => EgressReason::StreamLimit,
            4 => EgressReason::ProtocolError,
            5 => EgressReason::Overrun,
            x => EgressReason::Other(x),
        }
    }
}

impl fmt::Display for EgressReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EgressReason::ResolveFailed => f.write_str("resolve failed"),
            EgressReason::ConnectRefused => f.write_str("connection refused"),
            EgressReason::Unreachable => f.write_str("destination unreachable"),
            EgressReason::StreamLimit => f.write_str("too many streams"),
            EgressReason::ProtocolError => f.write_str("protocol error"),
            EgressReason::Overrun => f.write_str("buffer overrun"),
            EgressReason::Other(x) => write!(f, "reason {x}"),
        }
    }
}

/// One decoded mux frame. Payload-bearing variants borrow the input buffer, so
/// decoding never copies; the host copies out only the bytes it keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EgressFrame<'a> {
    /// Open a new stream to `host:port` (guest-chosen `stream` id).
    Open { stream: u16, port: u16, host: &'a [u8] },
    /// The stream connected.
    OpenOk { stream: u16 },
    /// The stream could not be opened.
    OpenErr { stream: u16, reason: EgressReason },
    /// Opaque TCP payload for an established stream.
    Data { stream: u16, payload: &'a [u8] },
    /// Half-close: the sender will send no more data on `stream`.
    Close { stream: u16 },
    /// Abortive close: drop `stream` immediately.
    Rst { stream: u16 },
    /// Grant the host `credit` more bytes to send toward the guest on `stream`.
    Window { stream: u16, credit: u32 },
}

impl EgressFrame<'_> {
    /// The stream id this frame targets.
    pub fn stream(&self) -> u16 {
        match *self {
            EgressFrame::Open { stream, .. }
            | EgressFrame::OpenOk { stream }
            | EgressFrame::OpenErr { stream, .. }
            | EgressFrame::Data { stream, .. }
            | EgressFrame::Close { stream }
            | EgressFrame::Rst { stream }
            | EgressFrame::Window { stream, .. } => stream,
        }
    }
}

/// A frame decoded from the front of a buffer, plus how many bytes it consumed —
/// the caller advances its parse cursor by [`Decoded::consumed`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decoded<'a> {
    pub frame: EgressFrame<'a>,
    pub consumed: usize,
}

/// A malformed frame or an unencodable value. Every variant is a leaf (no
/// `source`); [`decode`] returns these instead of ever panicking on hostile bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EgressCodecError {
    /// The `ver` byte was not [`EGRESS_VERSION`].
    UnsupportedVersion(u8),
    /// The `type` byte was not one of the seven known frame types.
    UnknownType(u8),
    /// A frame's `len` was illegal for its type (e.g. an `OPEN_OK` with a payload,
    /// or a `WINDOW` whose payload is not 4 bytes).
    BadLength { frame_type: u8, len: u16 },
    /// Encode-side: a hostname longer than [`EGRESS_MAX_HOST_LEN`].
    HostTooLong(usize),
    /// Encode-side: a payload longer than [`EGRESS_MAX_PAYLOAD`].
    PayloadTooLong(usize),
}

impl fmt::Display for EgressCodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EgressCodecError::UnsupportedVersion(v) => {
                write!(f, "unsupported egress frame version {v}")
            }
            EgressCodecError::UnknownType(t) => write!(f, "unknown egress frame type {t}"),
            EgressCodecError::BadLength { frame_type, len } => {
                write!(f, "illegal length {len} for egress frame type {frame_type}")
            }
            EgressCodecError::HostTooLong(n) => {
                write!(f, "hostname is {n} bytes, over the {EGRESS_MAX_HOST_LEN}-byte limit")
            }
            EgressCodecError::PayloadTooLong(n) => {
                write!(f, "payload is {n} bytes, over the {EGRESS_MAX_PAYLOAD}-byte frame limit")
            }
        }
    }
}

impl std::error::Error for EgressCodecError {}

/// Append `frame` to `out` as a framed wire message.
///
/// # Errors
///
/// [`EgressCodecError::HostTooLong`] or [`EgressCodecError::PayloadTooLong`] if a
/// field exceeds what the `len` field (or the hostname limit) can encode.
pub fn encode(frame: &EgressFrame<'_>, out: &mut Vec<u8>) -> Result<(), EgressCodecError> {
    match *frame {
        EgressFrame::Open { stream, port, host } => {
            if host.len() > EGRESS_MAX_HOST_LEN {
                return Err(EgressCodecError::HostTooLong(host.len()));
            }
            write_header(out, T_OPEN, stream, 2 + host.len())?;
            out.extend_from_slice(&port.to_le_bytes());
            out.extend_from_slice(host);
        }
        EgressFrame::OpenOk { stream } => write_header(out, T_OPEN_OK, stream, 0)?,
        EgressFrame::OpenErr { stream, reason } => {
            write_header(out, T_OPEN_ERR, stream, 1)?;
            out.push(reason.code());
        }
        EgressFrame::Data { stream, payload } => {
            if payload.len() > EGRESS_MAX_PAYLOAD {
                return Err(EgressCodecError::PayloadTooLong(payload.len()));
            }
            write_header(out, T_DATA, stream, payload.len())?;
            out.extend_from_slice(payload);
        }
        EgressFrame::Close { stream } => write_header(out, T_CLOSE, stream, 0)?,
        EgressFrame::Rst { stream } => write_header(out, T_RST, stream, 0)?,
        EgressFrame::Window { stream, credit } => {
            write_header(out, T_WINDOW, stream, 4)?;
            out.extend_from_slice(&credit.to_le_bytes());
        }
    }
    Ok(())
}

/// Write the 6-byte header, capping `len` at the `u16` field.
fn write_header(out: &mut Vec<u8>, ty: u8, stream: u16, len: usize) -> Result<(), EgressCodecError> {
    let len = u16::try_from(len).map_err(|_| EgressCodecError::PayloadTooLong(len))?;
    out.push(EGRESS_VERSION);
    out.push(ty);
    out.extend_from_slice(&stream.to_le_bytes());
    out.extend_from_slice(&len.to_le_bytes());
    Ok(())
}

/// Decode the frame at the front of `input`.
///
/// Returns `Ok(Some(decoded))` for a complete frame, `Ok(None)` when more bytes
/// are needed (a partial frame — the caller keeps the bytes and retries), and
/// `Err` for structurally invalid bytes.
///
/// This is an untrusted-input path: it validates the version and length before
/// trusting any field, allocates nothing (payloads borrow `input`), and never
/// panics.
///
/// # Errors
///
/// [`EgressCodecError::UnsupportedVersion`], [`EgressCodecError::UnknownType`], or
/// [`EgressCodecError::BadLength`] for a malformed frame.
pub fn decode(input: &[u8]) -> Result<Option<Decoded<'_>>, EgressCodecError> {
    // Reject a bad version as soon as the first byte is present, so a garbage
    // stream fails fast instead of stalling forever as an "incomplete" header.
    if let Some(&ver) = input.first() {
        if ver != EGRESS_VERSION {
            return Err(EgressCodecError::UnsupportedVersion(ver));
        }
    }
    if input.len() < EGRESS_HEADER_LEN {
        return Ok(None);
    }
    let ty = input[1];
    let stream = u16::from_le_bytes([input[2], input[3]]);
    let len = u16::from_le_bytes([input[4], input[5]]);
    let total = EGRESS_HEADER_LEN + len as usize;
    if input.len() < total {
        return Ok(None);
    }
    let payload = &input[EGRESS_HEADER_LEN..total];
    let bad_len = || EgressCodecError::BadLength { frame_type: ty, len };
    let frame = match ty {
        T_OPEN => {
            if payload.len() < 2 || payload.len() - 2 > EGRESS_MAX_HOST_LEN {
                return Err(bad_len());
            }
            EgressFrame::Open {
                stream,
                port: u16::from_le_bytes([payload[0], payload[1]]),
                host: &payload[2..],
            }
        }
        T_OPEN_OK => {
            if len != 0 {
                return Err(bad_len());
            }
            EgressFrame::OpenOk { stream }
        }
        T_OPEN_ERR => {
            if len != 1 {
                return Err(bad_len());
            }
            EgressFrame::OpenErr { stream, reason: EgressReason::from_code(payload[0]) }
        }
        T_DATA => EgressFrame::Data { stream, payload },
        T_CLOSE => {
            if len != 0 {
                return Err(bad_len());
            }
            EgressFrame::Close { stream }
        }
        T_RST => {
            if len != 0 {
                return Err(bad_len());
            }
            EgressFrame::Rst { stream }
        }
        T_WINDOW => {
            if len != 4 {
                return Err(bad_len());
            }
            EgressFrame::Window {
                stream,
                credit: u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]),
            }
        }
        other => return Err(EgressCodecError::UnknownType(other)),
    };
    Ok(Some(Decoded { frame, consumed: total }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed corpus that exercises every frame type and both a zero-length and a
    /// non-empty payload. Its encoding is the golden byte-identity tripwire.
    fn golden_corpus() -> Vec<EgressFrame<'static>> {
        vec![
            EgressFrame::Open { stream: 1, port: 443, host: b"example.com" },
            EgressFrame::OpenOk { stream: 1 },
            EgressFrame::Window { stream: 1, credit: 65536 },
            EgressFrame::Data { stream: 1, payload: b"GET / HTTP/1.0\r\n\r\n" },
            EgressFrame::Data { stream: 1, payload: b"" },
            EgressFrame::OpenErr { stream: 2, reason: EgressReason::ConnectRefused },
            EgressFrame::Close { stream: 1 },
            EgressFrame::Rst { stream: 2 },
            EgressFrame::OpenErr { stream: 3, reason: EgressReason::Other(200) },
        ]
    }

    fn encode_all(frames: &[EgressFrame<'_>]) -> Vec<u8> {
        let mut out = Vec::new();
        for f in frames {
            encode(f, &mut out).expect("golden corpus frames all encode");
        }
        out
    }

    #[test]
    fn every_frame_round_trips() {
        for f in golden_corpus() {
            let mut buf = Vec::new();
            encode(&f, &mut buf).unwrap();
            let decoded = decode(&buf).unwrap().expect("a whole frame decodes");
            assert_eq!(decoded.frame, f);
            assert_eq!(decoded.consumed, buf.len());
        }
    }

    #[test]
    fn decode_advances_across_concatenated_frames() {
        let frames = golden_corpus();
        let buf = encode_all(&frames);
        let mut off = 0;
        let mut seen = Vec::new();
        while let Some(d) = decode(&buf[off..]).unwrap() {
            seen.push(d.frame);
            off += d.consumed;
        }
        assert_eq!(off, buf.len());
        assert_eq!(seen, frames);
    }

    /// Byte-identity golden. The whole encoded corpus is compared to a hardcoded
    /// byte vector so any accidental codec drift (a field reorder, a type-byte
    /// change, an endianness slip) is caught. Rebuild these bytes ONLY on an
    /// intended codec change, and bump [`EGRESS_SCHEMA`] in the same commit.
    #[test]
    fn golden_corpus_is_byte_stable() {
        let got = encode_all(&golden_corpus());
        let expected: &[u8] = &[
            // Open { stream: 1, port: 443, host: "example.com" }
            0x01, 0x01, 0x01, 0x00, 0x0d, 0x00, 0xbb, 0x01, b'e', b'x', b'a', b'm', b'p', b'l',
            b'e', b'.', b'c', b'o', b'm',
            // OpenOk { stream: 1 }
            0x01, 0x02, 0x01, 0x00, 0x00, 0x00,
            // Window { stream: 1, credit: 65536 }
            0x01, 0x07, 0x01, 0x00, 0x04, 0x00, 0x00, 0x00, 0x01, 0x00,
            // Data { stream: 1, payload: "GET / HTTP/1.0\r\n\r\n" } (18 bytes)
            0x01, 0x04, 0x01, 0x00, 0x12, 0x00, b'G', b'E', b'T', b' ', b'/', b' ', b'H', b'T',
            b'T', b'P', b'/', b'1', b'.', b'0', b'\r', b'\n', b'\r', b'\n',
            // Data { stream: 1, payload: "" }
            0x01, 0x04, 0x01, 0x00, 0x00, 0x00,
            // OpenErr { stream: 2, reason: ConnectRefused }
            0x01, 0x03, 0x02, 0x00, 0x01, 0x00, 0x01,
            // Close { stream: 1 }
            0x01, 0x05, 0x01, 0x00, 0x00, 0x00,
            // Rst { stream: 2 }
            0x01, 0x06, 0x02, 0x00, 0x00, 0x00,
            // OpenErr { stream: 3, reason: Other(200) }
            0x01, 0x03, 0x03, 0x00, 0x01, 0x00, 0xc8,
        ];
        assert_eq!(got, expected, "egress codec output drifted — bump EGRESS_SCHEMA + regen goldens");
        assert_eq!(EGRESS_SCHEMA, 1, "EGRESS_SCHEMA changed without regenerating the golden bytes");
    }

    #[test]
    fn empty_input_is_incomplete_not_an_error() {
        assert_eq!(decode(&[]).unwrap(), None);
    }

    #[test]
    fn partial_header_is_incomplete() {
        // Version byte is valid, but fewer than 6 header bytes are present.
        assert_eq!(decode(&[EGRESS_VERSION, T_DATA, 0x01]).unwrap(), None);
    }

    #[test]
    fn truncated_payload_is_incomplete() {
        // Header claims a 10-byte payload; only 4 are present.
        let buf = [EGRESS_VERSION, T_DATA, 0x01, 0x00, 0x0a, 0x00, 1, 2, 3, 4];
        assert_eq!(decode(&buf).unwrap(), None);
    }

    #[test]
    fn bad_version_is_rejected_immediately() {
        assert_eq!(decode(&[0x00]).unwrap_err(), EgressCodecError::UnsupportedVersion(0));
        assert_eq!(decode(&[0xff, 0, 0, 0, 0, 0]).unwrap_err(), EgressCodecError::UnsupportedVersion(0xff));
    }

    #[test]
    fn unknown_type_is_rejected() {
        let buf = [EGRESS_VERSION, 99, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(decode(&buf).unwrap_err(), EgressCodecError::UnknownType(99));
    }

    #[test]
    fn fixed_size_types_reject_wrong_length() {
        // OPEN_OK must be empty.
        let open_ok = [EGRESS_VERSION, T_OPEN_OK, 0x01, 0x00, 0x03, 0x00, 1, 2, 3];
        assert!(matches!(decode(&open_ok), Err(EgressCodecError::BadLength { .. })));
        // WINDOW must be exactly 4 bytes.
        let window = [EGRESS_VERSION, T_WINDOW, 0x01, 0x00, 0x03, 0x00, 1, 2, 3];
        assert!(matches!(decode(&window), Err(EgressCodecError::BadLength { .. })));
        // OPEN must carry at least the 2-byte port.
        let open = [EGRESS_VERSION, T_OPEN, 0x01, 0x00, 0x01, 0x00, 1];
        assert!(matches!(decode(&open), Err(EgressCodecError::BadLength { .. })));
    }

    #[test]
    fn encode_rejects_oversize_host() {
        let host = vec![b'a'; EGRESS_MAX_HOST_LEN + 1];
        let mut out = Vec::new();
        assert_eq!(
            encode(&EgressFrame::Open { stream: 1, port: 80, host: &host }, &mut out),
            Err(EgressCodecError::HostTooLong(EGRESS_MAX_HOST_LEN + 1)),
        );
    }

    #[test]
    fn unknown_reason_code_round_trips_as_other() {
        assert_eq!(EgressReason::from_code(200), EgressReason::Other(200));
        assert_eq!(EgressReason::Other(200).code(), 200);
    }
}
