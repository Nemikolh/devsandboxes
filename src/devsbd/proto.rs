//! Frame protocol between the host (`devsandbox`) and the in-container helper
//! (`devsbd bridge`), over `exec -i` stdio (docs/sandbox-helper.md). One file
//! shared by both crates (devsbd includes it via `#[path]`) so the two sides
//! can't drift; std-only because devsbd has no dependencies.
//!
//! Wire format, little-endian, one byte stream per direction:
//! `u32 stream | u8 kind | u32 len | payload[len]`. Control frames (`Hello`,
//! `Ping`, `Pong`, `Caps`) use stream 0; the daemon allocates stream ids from 1
//! for the connections it accepts.
//!
//! Kinds: 0 `Hello` · 1 `Open` · 2 `Data` · 3 `Close` · 4 `Ping` · 5 `Pong` ·
//! 6 `Quit` · 7 `Connect` · 8 `Window` · 9 `Eof` · 10 `Caps`. 7–10 are the
//! additive port-forwarding frames (see docs/port-forwarding.md); older peers
//! skip them as unknown kinds.
//!
//! **Frozen across versions:** the header layout, `Hello`'s leading `u32`
//! version, and `Quit`. An outdated daemon must still understand the `Quit` a
//! newer one sends to replace it, and a `Hello` from either side must still
//! decode its `version` far enough to report a mismatch — even if the rest of
//! the payload has a shape this build doesn't recognize. Unknown frame kinds
//! are skipped, so a later build can add frames without a `VERSION` bump.

use std::error::Error;
use std::fmt;
use std::io::{self, Read, Write};

/// Bumped on any incompatible wire change. Host and helper ship together, so
/// a mismatch means a stale helper left in a still-running container.
pub const VERSION: u32 = 1;

/// Process exit code `devsbd bridge` uses when its handshake with the daemon
/// fails on a protocol version mismatch (as opposed to any other error, which
/// exits 1). The host reads this after the bridge's stdout hits EOF to classify
/// the failure as a mismatch without string-matching stderr, so it can stop
/// retrying that container until it leaves the running set (a restart rewrites
/// the helper). Distinct from the argv-usage code (2).
pub const MISMATCH_EXIT: i32 = 3;

/// Upper bound on one frame's payload. Agent messages are small; the cap
/// turns stray bytes on the stream (e.g. a shell banner on stdout) into an
/// error rather than a multi-GiB allocation.
pub const MAX_PAYLOAD: u32 = 1 << 20;

/// `Open` channel ids. Reserved up front so API proxying later adds a
/// channel, not a protocol change.
pub mod channel {
    pub const SSH_AGENT: u8 = 1;
    #[allow(dead_code)] // reserved for API proxying (docs/sandbox-helper.md)
    pub const HTTP_PROXY: u8 = 2;
    /// A notification record from the outbox (`notify.rs`): the daemon sends
    /// the record then `Eof`; the host replies `notify::REPLY_OK`. Only opened
    /// toward a host that advertised `caps::NOTIFY`.
    pub const NOTIFY: u8 = 3;
}

const KIND_HELLO: u8 = 0;
const KIND_OPEN: u8 = 1;
const KIND_DATA: u8 = 2;
const KIND_CLOSE: u8 = 3;
const KIND_PING: u8 = 4;
const KIND_PONG: u8 = 5;
const KIND_QUIT: u8 = 6;
const KIND_CONNECT: u8 = 7;
const KIND_WINDOW: u8 = 8;
const KIND_EOF: u8 = 9;
const KIND_CAPS: u8 = 10;

const HEADER_LEN: usize = 9;

/// Optional capability bits carried in `Caps` (and, later, `Hello`). A missing
/// bit means the peer won't handle the matching frames; unknown bits are
/// ignored, so the set can grow without a `VERSION` bump.
pub mod caps {
    /// Daemon serves `Connect` streams with flow control + half-close.
    pub const TCP_FORWARD: u32 = 1 << 0;
    /// Host serves ssh-agent streams for this bridge.
    pub const SSH_AGENT: u32 = 1 << 1;
    /// Host serves notify streams (`channel::NOTIFY`) for this bridge.
    pub const NOTIFY: u32 = 1 << 2;
}

/// Starting per-direction credit for a flow-controlled stream: a sender may
/// have this many bytes outstanding before a `Window` grants more. Sized so a
/// single stalled reader can't starve the mux yet bulk transfers stay full.
pub const INITIAL_WINDOW: u32 = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// First frame each way. `hash` is the sender's build hash (informational;
    /// only `version` must match). `caps` is a bitset of optional capabilities
    /// (0 today; unknown bits ignored). Wire layout:
    /// `u32 version | u8 hash_len | hash | u32 caps`, trailing bytes ignored,
    /// so later builds can append fields. Only the leading `version` is frozen.
    Hello { version: u32, hash: String, caps: u32 },
    /// A new stream on `channel`, e.g. an ssh client connected to the agent socket.
    Open { stream: u32, channel: u8 },
    Data { stream: u32, bytes: Vec<u8> },
    /// Either side is done with `stream`; the peer drops its end. `reason` is an
    /// optional UTF-8 diagnostic (empty = none) the host surfaces, e.g.
    /// `connection refused`; an empty reason encodes to no payload, so it stays
    /// byte-identical to the original `Close` and old peers are unaffected.
    Close { stream: u32, reason: String },
    /// Host asks the daemon to open a TCP stream to `host:port` from inside the
    /// container. `stream` carries the host id (high bit set). Flow-controlled
    /// end to end; see docs/port-forwarding.md.
    Connect { stream: u32, host: String, port: u16 },
    /// Flow control: the receiver grants `credit` more bytes it's ready to
    /// accept on `stream`. Batched (one per half-window), not one per chunk.
    Window { stream: u32, credit: u32 },
    /// Half-close: the sender's local side hit EOF on `stream`; the peer
    /// `shutdown(Write)`s its local socket but keeps the other direction open.
    Eof { stream: u32 },
    /// Host capabilities, sent once on stream 0 right after the handshake. Lets
    /// the daemon route by what this host supports; passes through the bridge
    /// verbatim. An old daemon skips this unknown kind.
    Caps(u32),
    /// Liveness check; the peer answers `Pong` with the same payload.
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    /// Sent as the first frame on the daemon's control socket by a daemon of a
    /// different build taking over: the running daemon exits. Frozen (see the
    /// module doc).
    Quit,
}

impl Frame {
    /// Serialized frame (header + payload), built in one buffer so a writer
    /// shared behind a mutex emits each frame with a single `write_all`.
    pub fn encode(&self) -> Vec<u8> {
        let (stream, kind, payload): (u32, u8, Vec<u8>) = match self {
            Frame::Hello { version, hash, caps } => {
                let mut p = version.to_le_bytes().to_vec();
                p.push(hash.len() as u8);
                p.extend_from_slice(hash.as_bytes());
                p.extend_from_slice(&caps.to_le_bytes());
                (0, KIND_HELLO, p)
            }
            Frame::Open { stream, channel } => (*stream, KIND_OPEN, vec![*channel]),
            Frame::Data { stream, bytes } => (*stream, KIND_DATA, bytes.clone()),
            Frame::Close { stream, reason } => (*stream, KIND_CLOSE, reason.clone().into_bytes()),
            Frame::Connect { stream, host, port } => {
                let mut p = port.to_le_bytes().to_vec();
                p.push(host.len() as u8);
                p.extend_from_slice(host.as_bytes());
                (*stream, KIND_CONNECT, p)
            }
            Frame::Window { stream, credit } => (*stream, KIND_WINDOW, credit.to_le_bytes().to_vec()),
            Frame::Eof { stream } => (*stream, KIND_EOF, Vec::new()),
            Frame::Caps(c) => (0, KIND_CAPS, c.to_le_bytes().to_vec()),
            Frame::Ping(p) => (0, KIND_PING, p.clone()),
            Frame::Pong(p) => (0, KIND_PONG, p.clone()),
            Frame::Quit => (0, KIND_QUIT, Vec::new()),
        };
        let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
        out.extend_from_slice(&stream.to_le_bytes());
        out.push(kind);
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&payload);
        out
    }

    /// `Ok(None)` for an unknown kind: its payload is already consumed, so the
    /// caller skips it and reads on. Additive frames therefore need no
    /// `VERSION` bump.
    fn decode(stream: u32, kind: u8, payload: Vec<u8>) -> io::Result<Option<Frame>> {
        Ok(Some(match kind {
            KIND_HELLO => {
                if payload.len() < 4 {
                    return Err(invalid("short Hello"));
                }
                let version = u32::from_le_bytes(payload[..4].try_into().unwrap());
                // A mismatched version can carry any layout (a future build's
                // Hello): keep `version` so the mismatch is reportable and
                // don't fail on the rest, which this build can't interpret.
                if version != VERSION {
                    return Ok(Some(Frame::Hello { version, hash: String::new(), caps: 0 }));
                }
                let Some((&hash_len, rest)) = payload[4..].split_first() else {
                    return Err(invalid("truncated Hello"));
                };
                let hash_len = hash_len as usize;
                if rest.len() < hash_len + 4 {
                    return Err(invalid("truncated Hello"));
                }
                let hash = String::from_utf8(rest[..hash_len].to_vec())
                    .map_err(|_| invalid("Hello hash is not utf-8"))?;
                let caps = u32::from_le_bytes(rest[hash_len..hash_len + 4].try_into().unwrap());
                // Bytes after `caps` are ignored (room for later fields).
                Frame::Hello { version, hash, caps }
            }
            KIND_OPEN => match payload[..] {
                [channel] => Frame::Open { stream, channel },
                _ => return Err(invalid("Open payload must be one channel byte")),
            },
            KIND_DATA => Frame::Data { stream, bytes: payload },
            // An empty payload is a plain close; a reason is decoded lossily so a
            // garbled diagnostic never turns a close into a hard error.
            KIND_CLOSE => Frame::Close {
                stream,
                reason: String::from_utf8_lossy(&payload).into_owned(),
            },
            KIND_CONNECT => {
                // u16 port LE | u8 host_len | host utf-8.
                if payload.len() < 3 {
                    return Err(invalid("short Connect"));
                }
                let port = u16::from_le_bytes(payload[..2].try_into().unwrap());
                let host_len = payload[2] as usize;
                if payload.len() != 3 + host_len {
                    return Err(invalid("Connect host length mismatch"));
                }
                let host = String::from_utf8(payload[3..].to_vec())
                    .map_err(|_| invalid("Connect host is not utf-8"))?;
                if host.is_empty() {
                    return Err(invalid("Connect host is empty"));
                }
                if port == 0 {
                    return Err(invalid("Connect port is 0"));
                }
                Frame::Connect { stream, host, port }
            }
            KIND_WINDOW => match payload[..].try_into() {
                Ok(b) => Frame::Window { stream, credit: u32::from_le_bytes(b) },
                Err(_) => return Err(invalid("Window payload must be four bytes")),
            },
            KIND_EOF => Frame::Eof { stream },
            KIND_CAPS => match payload[..].try_into() {
                Ok(b) => Frame::Caps(u32::from_le_bytes(b)),
                Err(_) => return Err(invalid("Caps payload must be four bytes")),
            },
            KIND_PING => Frame::Ping(payload),
            KIND_PONG => Frame::Pong(payload),
            KIND_QUIT => Frame::Quit,
            _ => return Ok(None),
        }))
    }
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

/// A peer spoke a different protocol `VERSION` than ours. Carried inside an
/// `InvalidData` `io::Error` so callers can recover the two versions
/// (`version_mismatch`) and phrase a message with the right direction, rather
/// than string-matching. `Display` is generic; the host and bridge format
/// their own container/direction-aware text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionMismatch {
    pub peer: u32,
    pub ours: u32,
}

impl fmt::Display for VersionMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "protocol version {} (peer) != {} (ours)", self.peer, self.ours)
    }
}

impl Error for VersionMismatch {}

/// Recover a [`VersionMismatch`] from a handshake error, if that's what it is.
pub fn version_mismatch(e: &io::Error) -> Option<&VersionMismatch> {
    e.get_ref()?.downcast_ref::<VersionMismatch>()
}

/// Write one frame and flush (stdio pipes are buffered on the helper side).
pub fn write_frame(w: &mut impl Write, frame: &Frame) -> io::Result<()> {
    w.write_all(&frame.encode())?;
    w.flush()
}

/// Read one frame. `Ok(None)` on EOF at a frame boundary (peer went away
/// cleanly); EOF inside a frame is `UnexpectedEof`.
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<Frame>> {
    loop {
        let mut header = [0u8; HEADER_LEN];
        let mut filled = 0;
        while filled < HEADER_LEN {
            match r.read(&mut header[filled..]) {
                Ok(0) if filled == 0 => return Ok(None),
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => filled += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        let stream = u32::from_le_bytes(header[0..4].try_into().unwrap());
        let kind = header[4];
        let len = u32::from_le_bytes(header[5..9].try_into().unwrap());
        if len > MAX_PAYLOAD {
            return Err(invalid(&format!("frame payload {len} exceeds {MAX_PAYLOAD} bytes")));
        }
        let mut payload = vec![0u8; len as usize];
        r.read_exact(&mut payload)?;
        // Unknown kinds are consumed and skipped; keep reading for a known one.
        if let Some(frame) = Frame::decode(stream, kind, payload)? {
            return Ok(Some(frame));
        }
    }
}

/// What the peer told us in its `Hello`: its build hash (informational) and its
/// capability bitset (0 from a peer that doesn't advertise any).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub hash: String,
    pub caps: u32,
}

/// Exchange `Hello`s: send ours (advertising `caps`), then require the peer's
/// first frame to be a `Hello` with our `VERSION`. Returns the peer's hash and
/// advertised caps.
pub fn handshake(r: &mut impl Read, w: &mut impl Write, hash: &str, caps: u32) -> io::Result<Peer> {
    write_frame(w, &Frame::Hello { version: VERSION, hash: hash.to_string(), caps })?;
    match read_frame(r)? {
        Some(Frame::Hello { version: VERSION, hash, caps }) => Ok(Peer { hash, caps }),
        Some(Frame::Hello { version, .. }) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            VersionMismatch { peer: version, ours: VERSION },
        )),
        Some(other) => Err(invalid(&format!("expected Hello, got {other:?}"))),
        None => Err(io::Error::new(io::ErrorKind::UnexpectedEof, "peer closed before Hello")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn all_kinds() -> Vec<Frame> {
        vec![
            Frame::Hello { version: VERSION, hash: "ab".repeat(32), caps: 0 },
            Frame::Open { stream: 7, channel: channel::SSH_AGENT },
            Frame::Open { stream: 9, channel: channel::NOTIFY },
            Frame::Data { stream: 7, bytes: b"\x00\x00\x00\x01\x0b".to_vec() },
            Frame::Data { stream: u32::MAX, bytes: Vec::new() },
            Frame::Close { stream: 7, reason: String::new() },
            Frame::Close { stream: 8, reason: "connection refused".into() },
            Frame::Connect { stream: 0x8000_0001, host: "postgres".into(), port: 5432 },
            Frame::Window { stream: 7, credit: INITIAL_WINDOW },
            Frame::Eof { stream: 7 },
            Frame::Caps(caps::TCP_FORWARD | caps::SSH_AGENT | caps::NOTIFY),
            Frame::Ping(b"t".to_vec()),
            Frame::Pong(Vec::new()),
            Frame::Quit,
        ]
    }

    #[test]
    fn frames_roundtrip_back_to_back() {
        let mut wire = Vec::new();
        for f in all_kinds() {
            write_frame(&mut wire, &f).unwrap();
        }
        let mut r = Cursor::new(wire);
        for f in all_kinds() {
            assert_eq!(read_frame(&mut r).unwrap(), Some(f));
        }
        assert_eq!(read_frame(&mut r).unwrap(), None);
    }

    #[test]
    fn header_layout_is_little_endian() {
        let wire = Frame::Data { stream: 0x0102_0304, bytes: b"hi".to_vec() }.encode();
        assert_eq!(wire, [4, 3, 2, 1, KIND_DATA, 2, 0, 0, 0, b'h', b'i']);
    }

    /// `Quit` and `Hello`'s frozen prefix (the header + leading `u32` version)
    /// must never change, or daemon takeover and mismatch reporting break for
    /// old helpers. The rest of `Hello`'s payload (hash_len, hash, caps, later
    /// fields) is *not* frozen; an old reader recovers `version` regardless.
    #[test]
    fn frozen_frames_keep_their_bytes() {
        assert_eq!(Frame::Quit.encode(), [0, 0, 0, 0, 6, 0, 0, 0, 0]);
        let hello = Frame::Hello { version: 1, hash: "h".into(), caps: 0 }.encode();
        // stream 0 | KIND_HELLO | len 10 | version 1 — the frozen prefix.
        assert_eq!(hello[..HEADER_LEN + 4], [0, 0, 0, 0, 0, 10, 0, 0, 0, 1, 0, 0, 0]);
        // Full current layout, for change awareness.
        assert_eq!(hello[HEADER_LEN + 4..], [1, b'h', 0, 0, 0, 0]);
    }

    #[test]
    fn connect_byte_layout() {
        // u16 port LE | u8 host_len | host utf-8, after the header.
        let wire = Frame::Connect { stream: 0x8000_0002, host: "db".into(), port: 5432 }.encode();
        assert_eq!(
            wire,
            [
                0x02, 0x00, 0x00, 0x80, // stream, high bit set
                KIND_CONNECT,
                5, 0, 0, 0, // payload len
                0x38, 0x15, // port 5432 LE
                2,          // host_len
                b'd', b'b',
            ]
        );
    }

    #[test]
    fn window_byte_layout() {
        let wire = Frame::Window { stream: 7, credit: INITIAL_WINDOW }.encode();
        #[rustfmt::skip]
        let want = [
            7, 0, 0, 0,        // stream
            KIND_WINDOW,
            4, 0, 0, 0,        // payload len
            0x00, 0x00, 0x04, 0x00, // 262144 LE
        ];
        assert_eq!(wire, want);
    }

    /// An empty-reason `Close` carries no payload, so it encodes byte-identically
    /// to the pre-`reason` `Close` (stream | KIND_CLOSE | len 0) and old peers see
    /// no change. A reason rides along and decodes back.
    #[test]
    fn close_reason_is_backward_compatible() {
        let bare = Frame::Close { stream: 4, reason: String::new() }.encode();
        assert_eq!(bare, [4, 0, 0, 0, KIND_CLOSE, 0, 0, 0, 0]);

        let with_reason = Frame::Close { stream: 4, reason: "refused".into() }.encode();
        assert_eq!(
            read_frame(&mut Cursor::new(with_reason)).unwrap(),
            Some(Frame::Close { stream: 4, reason: "refused".into() })
        );

        // An old-style empty-payload Close on the wire decodes to an empty reason.
        assert_eq!(
            read_frame(&mut Cursor::new(bare)).unwrap(),
            Some(Frame::Close { stream: 4, reason: String::new() })
        );
    }

    /// A `Close` reason that isn't valid UTF-8 decodes lossily rather than
    /// failing: a garbled diagnostic must never break tearing a stream down.
    #[test]
    fn close_reason_non_utf8_decodes_lossily() {
        let mut wire = vec![4, 0, 0, 0, KIND_CLOSE, 2, 0, 0, 0];
        wire.extend_from_slice(&[0xff, 0xfe]);
        assert_eq!(
            read_frame(&mut Cursor::new(wire)).unwrap(),
            Some(Frame::Close { stream: 4, reason: "\u{fffd}\u{fffd}".into() })
        );
    }

    #[test]
    fn malformed_connect_and_window_rejected() {
        let base = Frame::Connect { stream: 0x8000_0001, host: "db".into(), port: 80 }.encode();
        // Short (only 2 payload bytes, missing host_len).
        let mut short = base.clone();
        short[5..9].copy_from_slice(&2u32.to_le_bytes());
        short.truncate(HEADER_LEN + 2);
        // Empty host: port 80 | host_len 0 | (no host bytes).
        let empty_host = {
            let mut w = vec![0x01, 0x00, 0x00, 0x80, KIND_CONNECT, 3, 0, 0, 0];
            w.extend_from_slice(&80u16.to_le_bytes());
            w.push(0);
            w
        };
        // Non-utf8 host.
        let bad_utf8 = {
            let mut w = vec![0x01, 0x00, 0x00, 0x80, KIND_CONNECT, 4, 0, 0, 0];
            w.extend_from_slice(&80u16.to_le_bytes());
            w.push(1);
            w.push(0xff);
            w
        };
        // Port 0.
        let mut port_zero = base.clone();
        port_zero[HEADER_LEN..HEADER_LEN + 2].copy_from_slice(&0u16.to_le_bytes());
        // Window with a payload that isn't four bytes.
        let mut bad_window = Frame::Window { stream: 1, credit: 1 }.encode();
        bad_window[5..9].copy_from_slice(&3u32.to_le_bytes());
        bad_window.truncate(HEADER_LEN + 3);

        for wire in [short, empty_host, bad_utf8, port_zero, bad_window] {
            let err = read_frame(&mut Cursor::new(wire)).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        }
    }

    /// Reader yielding one byte per `read`, like a pipe under pressure.
    struct Trickle(Cursor<Vec<u8>>);
    impl Read for Trickle {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = buf.len().min(1);
            self.0.read(&mut buf[..n])
        }
    }

    #[test]
    fn short_reads_reassemble() {
        let f = Frame::Data { stream: 3, bytes: vec![9; 100] };
        let mut r = Trickle(Cursor::new(f.encode()));
        assert_eq!(read_frame(&mut r).unwrap(), Some(f));
        assert_eq!(read_frame(&mut r).unwrap(), None);
    }

    #[test]
    fn truncated_frames_are_unexpected_eof() {
        let wire = Frame::Data { stream: 1, bytes: b"abc".to_vec() }.encode();
        for cut in [1, HEADER_LEN - 1, HEADER_LEN, wire.len() - 1] {
            let err = read_frame(&mut Cursor::new(&wire[..cut])).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof, "cut at {cut}");
        }
    }

    #[test]
    fn rejects_oversized_and_malformed() {
        let mut big = Frame::Data { stream: 1, bytes: Vec::new() }.encode();
        big[5..9].copy_from_slice(&(MAX_PAYLOAD + 1).to_le_bytes());
        let mut open = Frame::Open { stream: 1, channel: 1 }.encode();
        open[5] = 2;
        open.push(0);
        // A shell banner on stdout instead of frames.
        let banner = b"Welcome to Ubuntu 24.04 LTS\n".to_vec();
        // Our version but nothing after it: must error, not index past the end.
        let mut bare_hello = vec![0, 0, 0, 0, KIND_HELLO, 4, 0, 0, 0];
        bare_hello.extend_from_slice(&VERSION.to_le_bytes());
        for wire in [big, open, banner, bare_hello] {
            let err = read_frame(&mut Cursor::new(wire)).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        }
    }

    /// An unknown kind is skipped (payload consumed), so a newer build's
    /// additive frame between two known ones doesn't break an old reader.
    #[test]
    fn unknown_kinds_are_skipped_mid_stream() {
        let mut unknown = Frame::Data { stream: 9, bytes: b"future".to_vec() }.encode();
        unknown[4] = 99; // a kind this build doesn't know
        let mut wire = Frame::Ping(b"a".to_vec()).encode();
        wire.extend_from_slice(&unknown);
        wire.extend_from_slice(&Frame::Pong(b"b".to_vec()).encode());
        let mut r = Cursor::new(wire);
        assert_eq!(read_frame(&mut r).unwrap(), Some(Frame::Ping(b"a".to_vec())));
        assert_eq!(read_frame(&mut r).unwrap(), Some(Frame::Pong(b"b".to_vec())));
        assert_eq!(read_frame(&mut r).unwrap(), None);

        // A trailing unknown kind alone reads as a clean end (EOF after skip).
        assert_eq!(read_frame(&mut Cursor::new(unknown)).unwrap(), None);
    }

    /// A `Hello` with trailing bytes past `caps` still decodes (room for later
    /// fields), keeping `version`, `hash` and `caps`.
    #[test]
    fn hello_ignores_trailing_bytes() {
        let mut wire = Frame::Hello { version: VERSION, hash: "h".into(), caps: 3 }.encode();
        let extra = b"future-field";
        let new_len = (wire.len() - HEADER_LEN + extra.len()) as u32;
        wire[5..9].copy_from_slice(&new_len.to_le_bytes());
        wire.extend_from_slice(extra);
        assert_eq!(
            read_frame(&mut Cursor::new(wire)).unwrap(),
            Some(Frame::Hello { version: VERSION, hash: "h".into(), caps: 3 })
        );
    }

    /// A mismatched-version `Hello` recovers `version` even when the bytes
    /// after it are garbage this build can't parse (a future layout).
    #[test]
    fn mismatch_hello_decodes_version_over_garbage() {
        let mut payload = (VERSION + 7).to_le_bytes().to_vec();
        payload.extend_from_slice(&[0xff, 0xfe, 0x00]); // not our hash_len/hash/caps shape
        let mut wire = Vec::new();
        wire.extend_from_slice(&0u32.to_le_bytes());
        wire.push(KIND_HELLO);
        wire.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        wire.extend_from_slice(&payload);
        assert_eq!(
            read_frame(&mut Cursor::new(wire)).unwrap(),
            Some(Frame::Hello { version: VERSION + 7, hash: String::new(), caps: 0 })
        );
    }

    #[test]
    fn version_mismatch_is_a_downcastable_typed_error() {
        let mut stale = Vec::new();
        write_frame(&mut stale, &Frame::Hello { version: VERSION + 1, hash: String::new(), caps: 0 }).unwrap();
        let err = handshake(&mut Cursor::new(stale), &mut Vec::new(), "", 0).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let vm = version_mismatch(&err).expect("typed VersionMismatch");
        assert_eq!(*vm, VersionMismatch { peer: VERSION + 1, ours: VERSION });
        // A plain InvalidData error is not a mismatch.
        assert!(version_mismatch(&invalid("nope")).is_none());
    }

    #[test]
    fn handshake_returns_peer_hash_and_caps() {
        let mut peer = Vec::new();
        write_frame(
            &mut peer,
            &Frame::Hello { version: VERSION, hash: "peer".into(), caps: caps::TCP_FORWARD },
        )
        .unwrap();
        let mut sent = Vec::new();
        let got = handshake(&mut Cursor::new(peer), &mut sent, "mine", caps::SSH_AGENT).unwrap();
        assert_eq!(got, Peer { hash: "peer".into(), caps: caps::TCP_FORWARD });
        // We advertised our own caps in the Hello we sent.
        assert_eq!(
            read_frame(&mut Cursor::new(sent)).unwrap(),
            Some(Frame::Hello { version: VERSION, hash: "mine".into(), caps: caps::SSH_AGENT })
        );
    }

    #[test]
    fn handshake_rejects_version_mismatch_and_non_hello() {
        let mut stale = Vec::new();
        write_frame(&mut stale, &Frame::Hello { version: VERSION + 1, hash: String::new(), caps: 0 }).unwrap();
        let err = handshake(&mut Cursor::new(stale), &mut Vec::new(), "", 0).unwrap_err();
        assert!(version_mismatch(&err).is_some(), "{err}");

        let mut ping = Vec::new();
        write_frame(&mut ping, &Frame::Ping(Vec::new())).unwrap();
        assert!(handshake(&mut Cursor::new(ping), &mut Vec::new(), "", 0).is_err());

        let err = handshake(&mut Cursor::new(Vec::new()), &mut Vec::new(), "", 0).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    /// Both ends over real OS pipes, each side on its own thread, as the host
    /// and `devsbd bridge` will run.
    #[cfg(unix)]
    #[test]
    fn handshake_and_stream_over_pipes() {
        let (mut a_r, mut b_w) = std::io::pipe().unwrap();
        let (mut b_r, mut a_w) = std::io::pipe().unwrap();
        let helper = std::thread::spawn(move || {
            handshake(&mut b_r, &mut b_w, "helper", 0).unwrap();
            let Some(Frame::Ping(p)) = read_frame(&mut b_r).unwrap() else { panic!("want Ping") };
            write_frame(&mut b_w, &Frame::Pong(p)).unwrap();
            write_frame(&mut b_w, &Frame::Open { stream: 1, channel: channel::SSH_AGENT }).unwrap();
            write_frame(&mut b_w, &Frame::Data { stream: 1, bytes: b"req".to_vec() }).unwrap();
            write_frame(&mut b_w, &Frame::Close { stream: 1, reason: String::new() }).unwrap();
        });
        assert_eq!(handshake(&mut a_r, &mut a_w, "host", 0).unwrap().hash, "helper");
        write_frame(&mut a_w, &Frame::Ping(b"1".to_vec())).unwrap();
        assert_eq!(read_frame(&mut a_r).unwrap(), Some(Frame::Pong(b"1".to_vec())));
        assert_eq!(read_frame(&mut a_r).unwrap(), Some(Frame::Open { stream: 1, channel: 1 }));
        assert_eq!(read_frame(&mut a_r).unwrap(), Some(Frame::Data { stream: 1, bytes: b"req".to_vec() }));
        assert_eq!(read_frame(&mut a_r).unwrap(), Some(Frame::Close { stream: 1, reason: String::new() }));
        helper.join().unwrap();
        assert_eq!(read_frame(&mut a_r).unwrap(), None);
    }
}
