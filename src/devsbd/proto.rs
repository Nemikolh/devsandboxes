//! Frame protocol between the host (`devsandbox`) and the in-container helper
//! (`devsbd bridge`), over `exec -i` stdio (docs/sandbox-helper.md). One file
//! shared by both crates (devsbd includes it via `#[path]`) so the two sides
//! can't drift; std-only because devsbd has no dependencies.
//!
//! Wire format, little-endian, one byte stream per direction:
//! `u32 stream | u8 kind | u32 len | payload[len]`. Control frames (`Hello`,
//! `Ping`, `Pong`) use stream 0; the daemon allocates stream ids from 1 for
//! the connections it accepts.

use std::io::{self, Read, Write};

/// Bumped on any incompatible wire change. Host and helper ship together, so
/// a mismatch means a stale helper left in a still-running container.
pub const VERSION: u32 = 1;

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
}

const KIND_HELLO: u8 = 0;
const KIND_OPEN: u8 = 1;
const KIND_DATA: u8 = 2;
const KIND_CLOSE: u8 = 3;
const KIND_PING: u8 = 4;
const KIND_PONG: u8 = 5;

const HEADER_LEN: usize = 9;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// First frame each way. `hash` is the sender's build hash (informational;
    /// only `version` must match).
    Hello { version: u32, hash: String },
    /// A new stream on `channel`, e.g. an ssh client connected to the agent socket.
    Open { stream: u32, channel: u8 },
    Data { stream: u32, bytes: Vec<u8> },
    /// Either side is done with `stream`; the peer drops its end.
    Close { stream: u32 },
    /// Liveness check; the peer answers `Pong` with the same payload.
    Ping(Vec<u8>),
    Pong(Vec<u8>),
}

impl Frame {
    /// Serialized frame (header + payload), built in one buffer so a writer
    /// shared behind a mutex emits each frame with a single `write_all`.
    pub fn encode(&self) -> Vec<u8> {
        let (stream, kind, payload): (u32, u8, Vec<u8>) = match self {
            Frame::Hello { version, hash } => {
                let mut p = version.to_le_bytes().to_vec();
                p.extend_from_slice(hash.as_bytes());
                (0, KIND_HELLO, p)
            }
            Frame::Open { stream, channel } => (*stream, KIND_OPEN, vec![*channel]),
            Frame::Data { stream, bytes } => (*stream, KIND_DATA, bytes.clone()),
            Frame::Close { stream } => (*stream, KIND_CLOSE, Vec::new()),
            Frame::Ping(p) => (0, KIND_PING, p.clone()),
            Frame::Pong(p) => (0, KIND_PONG, p.clone()),
        };
        let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
        out.extend_from_slice(&stream.to_le_bytes());
        out.push(kind);
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&payload);
        out
    }

    fn decode(stream: u32, kind: u8, payload: Vec<u8>) -> io::Result<Frame> {
        Ok(match kind {
            KIND_HELLO => {
                if payload.len() < 4 {
                    return Err(invalid("short Hello"));
                }
                let version = u32::from_le_bytes(payload[..4].try_into().unwrap());
                let hash = String::from_utf8(payload[4..].to_vec())
                    .map_err(|_| invalid("Hello hash is not utf-8"))?;
                Frame::Hello { version, hash }
            }
            KIND_OPEN => match payload[..] {
                [channel] => Frame::Open { stream, channel },
                _ => return Err(invalid("Open payload must be one channel byte")),
            },
            KIND_DATA => Frame::Data { stream, bytes: payload },
            KIND_CLOSE => Frame::Close { stream },
            KIND_PING => Frame::Ping(payload),
            KIND_PONG => Frame::Pong(payload),
            other => return Err(invalid(&format!("unknown frame kind {other}"))),
        })
    }
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

/// Write one frame and flush (stdio pipes are buffered on the helper side).
pub fn write_frame(w: &mut impl Write, frame: &Frame) -> io::Result<()> {
    w.write_all(&frame.encode())?;
    w.flush()
}

/// Read one frame. `Ok(None)` on EOF at a frame boundary (peer went away
/// cleanly); EOF inside a frame is `UnexpectedEof`.
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<Frame>> {
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
    Frame::decode(stream, kind, payload).map(Some)
}

/// Exchange `Hello`s: send ours, then require the peer's first frame to be a
/// `Hello` with our `VERSION`. Returns the peer's build hash.
pub fn handshake(r: &mut impl Read, w: &mut impl Write, hash: &str) -> io::Result<String> {
    write_frame(w, &Frame::Hello { version: VERSION, hash: hash.to_string() })?;
    match read_frame(r)? {
        Some(Frame::Hello { version: VERSION, hash }) => Ok(hash),
        Some(Frame::Hello { version, .. }) => Err(invalid(&format!(
            "protocol version {version}, expected {VERSION}: helper out of date, restart the instance"
        ))),
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
            Frame::Hello { version: VERSION, hash: "ab".repeat(32) },
            Frame::Open { stream: 7, channel: channel::SSH_AGENT },
            Frame::Data { stream: 7, bytes: b"\x00\x00\x00\x01\x0b".to_vec() },
            Frame::Data { stream: u32::MAX, bytes: Vec::new() },
            Frame::Close { stream: 7 },
            Frame::Ping(b"t".to_vec()),
            Frame::Pong(Vec::new()),
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
    fn rejects_oversized_unknown_and_malformed() {
        let mut big = Frame::Data { stream: 1, bytes: Vec::new() }.encode();
        big[5..9].copy_from_slice(&(MAX_PAYLOAD + 1).to_le_bytes());
        let mut unknown = Frame::Close { stream: 1 }.encode();
        unknown[4] = 99;
        let mut open = Frame::Open { stream: 1, channel: 1 }.encode();
        open[5] = 2;
        open.push(0);
        // A shell banner on stdout instead of frames.
        let banner = b"Welcome to Ubuntu 24.04 LTS\n".to_vec();
        for wire in [big, unknown, open, banner] {
            let err = read_frame(&mut Cursor::new(wire)).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        }
    }

    #[test]
    fn handshake_returns_peer_hash() {
        let mut peer = Vec::new();
        write_frame(&mut peer, &Frame::Hello { version: VERSION, hash: "peer".into() }).unwrap();
        let mut sent = Vec::new();
        let got = handshake(&mut Cursor::new(peer), &mut sent, "mine").unwrap();
        assert_eq!(got, "peer");
        assert_eq!(
            read_frame(&mut Cursor::new(sent)).unwrap(),
            Some(Frame::Hello { version: VERSION, hash: "mine".into() })
        );
    }

    #[test]
    fn handshake_rejects_version_mismatch_and_non_hello() {
        let mut stale = Vec::new();
        write_frame(&mut stale, &Frame::Hello { version: VERSION + 1, hash: String::new() }).unwrap();
        let err = handshake(&mut Cursor::new(stale), &mut Vec::new(), "").unwrap_err();
        assert!(err.to_string().contains("out of date"), "{err}");

        let mut ping = Vec::new();
        write_frame(&mut ping, &Frame::Ping(Vec::new())).unwrap();
        assert!(handshake(&mut Cursor::new(ping), &mut Vec::new(), "").is_err());

        let err = handshake(&mut Cursor::new(Vec::new()), &mut Vec::new(), "").unwrap_err();
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
            handshake(&mut b_r, &mut b_w, "helper").unwrap();
            let Some(Frame::Ping(p)) = read_frame(&mut b_r).unwrap() else { panic!("want Ping") };
            write_frame(&mut b_w, &Frame::Pong(p)).unwrap();
            write_frame(&mut b_w, &Frame::Open { stream: 1, channel: channel::SSH_AGENT }).unwrap();
            write_frame(&mut b_w, &Frame::Data { stream: 1, bytes: b"req".to_vec() }).unwrap();
            write_frame(&mut b_w, &Frame::Close { stream: 1 }).unwrap();
        });
        assert_eq!(handshake(&mut a_r, &mut a_w, "host").unwrap(), "helper");
        write_frame(&mut a_w, &Frame::Ping(b"1".to_vec())).unwrap();
        assert_eq!(read_frame(&mut a_r).unwrap(), Some(Frame::Pong(b"1".to_vec())));
        assert_eq!(read_frame(&mut a_r).unwrap(), Some(Frame::Open { stream: 1, channel: 1 }));
        assert_eq!(read_frame(&mut a_r).unwrap(), Some(Frame::Data { stream: 1, bytes: b"req".to_vec() }));
        assert_eq!(read_frame(&mut a_r).unwrap(), Some(Frame::Close { stream: 1 }));
        helper.join().unwrap();
        assert_eq!(read_frame(&mut a_r).unwrap(), None);
    }
}
