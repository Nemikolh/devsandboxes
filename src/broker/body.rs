//! Body framing both ways: [`BodyReader`] yields a request body as its
//! head's [`Framing`] delimits it, leaving the reader at the next request;
//! [`ChunkedWriter`] frames a response body chunked, for a body whose
//! length changes on the way (decoded, rewritten).
//!
//! Chunked input is as strict as the head: CRLF only, a chunk size of hex
//! digits with no whitespace before an extension (extensions are skipped
//! but may hold no control character), sizes past `u64` refused, and the
//! trailer section must be empty: nothing the broker forwards needs request
//! trailers, and dropping fields a client sent would be a silent rewrite.

use std::io::{self, BufRead, Read, Write};

use super::HttpError;
use super::head::Framing;

pub const MAX_CHUNK_LINE: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Length(u64),
    ChunkSize,
    ChunkData(u64),
    ChunkEnd,
    Trailers,
    Done,
}

pub struct BodyReader<R> {
    inner: R,
    state: State,
}

impl<R: BufRead> BodyReader<R> {
    pub fn new(inner: R, framing: Framing) -> Self {
        let state = match framing {
            Framing::Empty | Framing::Length(0) => State::Done,
            Framing::Length(n) => State::Length(n),
            Framing::Chunked => State::ChunkSize,
        };
        BodyReader { inner, state }
    }

    /// Whether the body has been read to its end (the reader then sits at
    /// the next request's first byte).
    pub fn is_done(&self) -> bool {
        self.state == State::Done
    }

    pub fn into_inner(self) -> R {
        self.inner
    }

    /// EOF before `left` is a truncated body.
    fn read_data(&mut self, buf: &mut [u8], left: u64) -> io::Result<usize> {
        let max = usize::try_from(left).unwrap_or(usize::MAX).min(buf.len());
        let n = self.inner.read(&mut buf[..max])?;
        if n == 0 && max > 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        Ok(n)
    }
}

impl<R: BufRead> Read for BodyReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            match self.state {
                State::Done => return Ok(0),
                State::Length(left) => {
                    let n = self.read_data(buf, left)?;
                    let left = left - n as u64;
                    self.state = if left == 0 { State::Done } else { State::Length(left) };
                    return Ok(n);
                }
                State::ChunkSize => {
                    let line = read_crlf_line(&mut self.inner, MAX_CHUNK_LINE)?;
                    self.state = match parse_chunk_size(&line)? {
                        0 => State::Trailers,
                        n => State::ChunkData(n),
                    };
                }
                State::ChunkData(left) => {
                    let n = self.read_data(buf, left)?;
                    let left = left - n as u64;
                    self.state = if left == 0 { State::ChunkEnd } else { State::ChunkData(left) };
                    return Ok(n);
                }
                State::ChunkEnd => {
                    if !read_crlf_line(&mut self.inner, 0)?.is_empty() {
                        return Err(HttpError::BadChunk.into());
                    }
                    self.state = State::ChunkSize;
                }
                State::Trailers => {
                    if !read_crlf_line(&mut self.inner, MAX_CHUNK_LINE)?.is_empty() {
                        return Err(HttpError::Trailers.into());
                    }
                    self.state = State::Done;
                }
            }
        }
    }
}

/// A bare LF, a lone CR, or more than `max` bytes before the CRLF is a
/// [`HttpError::BadChunk`].
fn read_crlf_line<R: BufRead>(r: &mut R, max: usize) -> io::Result<Vec<u8>> {
    let mut line = Vec::new();
    loop {
        let buf = r.fill_buf()?;
        if buf.is_empty() {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let mut used = 0;
        let mut done = false;
        for &b in buf {
            used += 1;
            match (line.last(), b) {
                (Some(b'\r'), b'\n') => {
                    line.pop();
                    done = true;
                    break;
                }
                (Some(b'\r'), _) | (_, b'\n') => return Err(HttpError::BadChunk.into()),
                _ => {}
            }
            // The pending CR is allowed past `max`: it may end the line.
            if line.len() >= max && b != b'\r' {
                return Err(HttpError::BadChunk.into());
            }
            line.push(b);
        }
        r.consume(used);
        if done {
            return Ok(line);
        }
    }
}

fn parse_chunk_size(line: &[u8]) -> Result<u64, HttpError> {
    let digits = line.iter().take_while(|b| b.is_ascii_hexdigit()).count();
    let (size, ext) = line.split_at(digits);
    if size.is_empty() {
        return Err(HttpError::BadChunk);
    }
    match ext.split_first() {
        None => {}
        Some((b';', rest)) if rest.iter().all(|&b| b == b'\t' || (b' '..=b'~').contains(&b) || b >= 0x80) => {}
        Some(_) => return Err(HttpError::BadChunk),
    }
    size.iter()
        .try_fold(0u64, |n, &d| {
            let d = (d as char).to_digit(16)?;
            n.checked_mul(16)?.checked_add(u64::from(d))
        })
        .ok_or(HttpError::BadChunk)
}

/// One chunk per non-empty `write`; [`ChunkedWriter::finish`] writes the
/// last chunk.
pub struct ChunkedWriter<W: Write> {
    inner: W,
}

impl<W: Write> ChunkedWriter<W> {
    pub fn new(inner: W) -> Self {
        ChunkedWriter { inner }
    }

    pub fn finish(mut self) -> io::Result<W> {
        self.inner.write_all(b"0\r\n\r\n")?;
        self.inner.flush()?;
        Ok(self.inner)
    }
}

impl<W: Write> Write for ChunkedWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // An empty chunk would end the body.
        if buf.is_empty() {
            return Ok(0);
        }
        write!(self.inner, "{:x}\r\n", buf.len())?;
        self.inner.write_all(buf)?;
        self.inner.write_all(b"\r\n")?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broker::refusal;

    fn body(raw: &[u8], framing: Framing) -> io::Result<(Vec<u8>, Vec<u8>)> {
        // A tiny buffer, so every state boundary falls mid-buffer somewhere.
        let mut r = BodyReader::new(io::BufReader::with_capacity(2, raw), framing);
        let mut out = Vec::new();
        r.read_to_end(&mut out)?;
        assert!(r.is_done());
        let mut rest = Vec::new();
        r.into_inner().read_to_end(&mut rest)?;
        Ok((out, rest))
    }

    fn refused(raw: &[u8]) -> HttpError {
        let e = body(raw, Framing::Chunked).unwrap_err();
        refusal(&e).cloned().unwrap_or_else(|| panic!("{raw:?}: {e}"))
    }

    #[test]
    fn length() {
        let (b, rest) = body(b"hello worldGET", Framing::Length(11)).unwrap();
        assert_eq!(b, b"hello world");
        assert_eq!(rest, b"GET");
        let (b, rest) = body(b"GET", Framing::Empty).unwrap();
        assert!(b.is_empty());
        assert_eq!(rest, b"GET");
        let (b, _) = body(b"GET", Framing::Length(0)).unwrap();
        assert!(b.is_empty());
    }

    #[test]
    fn length_truncated() {
        let e = body(b"hel", Framing::Length(11)).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(refusal(&e), None);
    }

    #[test]
    fn chunked() {
        let (b, rest) =
            body(b"5\r\nhello\r\n1;name=\"v\"\r\n \r\nA\r\n0123456789\r\n0\r\n\r\nGET", Framing::Chunked).unwrap();
        assert_eq!(b, b"hello 0123456789");
        assert_eq!(rest, b"GET");
        let (b, _) = body(b"0\r\n\r\n", Framing::Chunked).unwrap();
        assert!(b.is_empty());
        let (b, _) = body(b"00000000000000000000003\r\nabc\r\n0;x\r\n\r\n", Framing::Chunked).unwrap();
        assert_eq!(b, b"abc");
    }

    #[test]
    fn chunk_size_lines_refused() {
        for raw in [
            &b"\r\n"[..],
            b" 5\r\nhello\r\n0\r\n\r\n",
            b"5 \r\nhello\r\n0\r\n\r\n",
            b"5 ;x\r\nhello\r\n0\r\n\r\n",
            b"+5\r\nhello\r\n0\r\n\r\n",
            b"0x5\r\nhello\r\n0\r\n\r\n",
            b"5;x\x00\r\nhello\r\n0\r\n\r\n",
            b"g\r\n",
            // Past u64.
            b"10000000000000000\r\n",
            b"ffffffffffffffffff\r\n",
        ] {
            assert_eq!(refused(raw), HttpError::BadChunk, "{raw:?}");
        }
        let mut long = b"1;".to_vec();
        long.resize(MAX_CHUNK_LINE + 1, b'x');
        long.extend_from_slice(b"\r\n");
        assert_eq!(refused(&long), HttpError::BadChunk);
    }

    #[test]
    fn chunk_line_endings_refused() {
        for raw in [
            &b"5\nhello\r\n0\r\n\r\n"[..],
            b"5\r\nhello\n0\r\n\r\n",
            b"5\r\nhello\r\n0\n\r\n",
            b"5\r\nhello\r\n0\r\n\n",
            b"5\rhello\r\n0\r\n\r\n",
            // Data longer than its size.
            b"5\r\nhelloX\r\n0\r\n\r\n",
        ] {
            let e = body(raw, Framing::Chunked).unwrap_err();
            assert!(matches!(refusal(&e), Some(HttpError::BadChunk)), "{raw:?}: {e}");
        }
    }

    #[test]
    fn chunked_trailers_refused() {
        assert_eq!(refused(b"0\r\nX-Checksum: 1\r\n\r\n"), HttpError::Trailers);
    }

    #[test]
    fn chunked_truncated() {
        for raw in [&b"5\r\nhel"[..], b"5\r\nhello", b"5\r\nhello\r\n", b"0\r\n", b"5"] {
            let e = body(raw, Framing::Chunked).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof, "{raw:?}");
        }
    }

    #[test]
    fn chunk_max_line_exactly_fits() {
        let mut line = b"3;".to_vec();
        line.resize(MAX_CHUNK_LINE, b'x');
        line.extend_from_slice(b"\r\nabc\r\n0\r\n\r\n");
        assert_eq!(body(&line, Framing::Chunked).unwrap().0, b"abc");
    }

    #[test]
    fn writer_round_trips() {
        let mut w = ChunkedWriter::new(Vec::new());
        w.write_all(b"data: one\n\n").unwrap();
        w.write_all(b"").unwrap();
        w.write_all(&[b'x'; 300]).unwrap();
        let out = w.finish().unwrap();
        assert!(out.starts_with(b"b\r\ndata: one\n\n\r\n12c\r\n"));
        assert!(out.ends_with(b"\r\n0\r\n\r\n"));
        let (b, rest) = body(&out, Framing::Chunked).unwrap();
        assert_eq!(b.len(), 11 + 300);
        assert!(rest.is_empty());
    }
}
