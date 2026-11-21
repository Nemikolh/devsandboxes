//! An upstream response: its head as strictly as a request's (one
//! unambiguous body length or a refusal), its body de-framed, and a
//! `Content-Encoding` sent despite `Accept-Encoding: identity` decoded.

use std::io::{self, BufRead, Read};

use flate2::read::{MultiGzDecoder, ZlibDecoder};

use super::{UpstreamError, bad_response};
use crate::broker::body::BodyReader;
use crate::broker::head::{self, Framing, Header, MAX_HEADERS};

/// Interim (1xx) responses skipped before a final one, at most.
const MAX_INTERIM: usize = 16;

/// How the body [`super::Response::body`] yields relates to the response
/// headers, i.e. how the caller frames it on its own connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Length {
    /// No body (`HEAD`, 204, 304).
    Empty,
    /// Exactly the headers' `Content-Length`: they can go on as they are.
    Exact(u64),
    /// Chunked upstream, delimited by the connection's end, or decoded: the
    /// caller drops `Content-Length` / `Transfer-Encoding` and frames it
    /// with `broker::body::ChunkedWriter`.
    Unframed,
}

/// A `Content-Encoding` decoded on the way. `br` and `zstd` are refused
/// instead (a dependency each, for upstreams that ignore `identity`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Gzip,
    /// zlib-wrapped, as RFC 9110 defines it (raw deflate fails to decode).
    Deflate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Head {
    pub status: u16,
    pub reason: String,
    pub headers: Vec<Header>,
    pub framing: Option<Framing>,
}

/// The final response head, after any interim ones. `framing` is `None`
/// for a body delimited by the connection's end.
pub(super) fn read_head<R: BufRead>(r: &mut R, head_request: bool) -> io::Result<Head> {
    for _ in 0..=MAX_INTERIM {
        let raw = head::read_raw_head(r)
            .map_err(|e| bad_response(e, "response head"))?
            .ok_or_else(|| io::Error::from(UpstreamError::BadResponse("upstream closed before answering".into())))?;
        let head = parse_head(&raw, head_request)?;
        match head.status {
            101 => return Err(UpstreamError::BadResponse("101 Switching Protocols to a request without Upgrade".into()).into()),
            100..=199 => continue,
            _ => return Ok(head),
        }
    }
    Err(UpstreamError::BadResponse("too many interim responses".into()).into())
}

pub(super) fn parse_head(raw: &[u8], head_request: bool) -> Result<Head, UpstreamError> {
    let bad = |why: &str| UpstreamError::BadResponse(why.to_string());
    let mut slots = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut resp = httparse::Response::new(&mut slots);
    match resp.parse(raw) {
        Ok(httparse::Status::Complete(n)) if n == raw.len() => {}
        _ => return Err(bad("malformed response head")),
    }
    let http11 = match resp.version {
        Some(1) => true,
        Some(0) => false,
        _ => return Err(bad("response version other than HTTP/1.x")),
    };
    let status = resp.code.ok_or_else(|| bad("malformed response head"))?;
    let reason = String::from_utf8_lossy(resp.reason.unwrap_or_default().as_bytes()).into_owned();
    let headers: Vec<Header> = resp
        .headers
        .iter()
        .map(|h| Header {
            name: h.name.to_string(),
            value: h.value.to_vec(),
        })
        .collect();
    let framing = if head_request || matches!(status, 100..=199 | 204 | 304) {
        Some(Framing::Empty)
    } else {
        framing(http11, &headers)?
    };
    Ok(Head {
        status,
        reason,
        headers,
        framing,
    })
}

fn framing(http11: bool, headers: &[Header]) -> Result<Option<Framing>, UpstreamError> {
    let bad = |why: &str| UpstreamError::BadResponse(why.to_string());
    let has_te = head::header_count(headers, "transfer-encoding") > 0;
    let has_cl = head::header_count(headers, "content-length") > 0;
    match (has_te, has_cl) {
        (true, true) => Err(bad("both Content-Length and Transfer-Encoding")),
        (true, false) => {
            let codings: Vec<&[u8]> = head::list_elements(headers, "transfer-encoding").collect();
            if http11 && codings.len() == 1 && codings[0].eq_ignore_ascii_case(b"chunked") {
                Ok(Some(Framing::Chunked))
            } else {
                Err(bad("Transfer-Encoding other than chunked"))
            }
        }
        (false, true) => {
            let mut length = None;
            for v in head::list_elements(headers, "content-length") {
                let n = head::parse_decimal(v).ok_or_else(|| bad("invalid Content-Length"))?;
                if length.is_some_and(|l| l != n) {
                    return Err(bad("conflicting Content-Length"));
                }
                length = Some(n);
            }
            Ok(length.map(Framing::Length))
        }
        (false, false) => Ok(None),
    }
}

/// `identity` (or none) is `None`; one `gzip` / `x-gzip` / `deflate` is
/// decoded; anything else, or a stack of codings, is refused.
pub(super) fn content_encoding(headers: &[Header]) -> Result<Option<Encoding>, UpstreamError> {
    let codings: Vec<&[u8]> = head::list_elements(headers, "content-encoding")
        .filter(|c| !c.is_empty() && !c.eq_ignore_ascii_case(b"identity"))
        .collect();
    match codings.as_slice() {
        [] => Ok(None),
        [c] if c.eq_ignore_ascii_case(b"gzip") || c.eq_ignore_ascii_case(b"x-gzip") => Ok(Some(Encoding::Gzip)),
        [c] if c.eq_ignore_ascii_case(b"deflate") => Ok(Some(Encoding::Deflate)),
        _ => {
            let all: Vec<String> = codings.iter().map(|c| String::from_utf8_lossy(c).into_owned()).collect();
            Err(UpstreamError::Encoding(all.join(", ")))
        }
    }
}

/// The body as the caller reads it: de-framed, then decoded.
pub(super) fn body<R: BufRead + Send + 'static>(
    conn: R,
    framing: Option<Framing>,
    encoding: Option<Encoding>,
) -> (Box<dyn Read + Send>, Length, Option<Encoding>) {
    let raw: Box<dyn Read + Send> = match framing {
        Some(Framing::Empty) => return (Box::new(io::empty()), Length::Empty, None),
        Some(f) => Box::new(BodyReader::new(conn, f)),
        None => Box::new(conn),
    };
    match encoding {
        Some(Encoding::Gzip) => (Box::new(MultiGzDecoder::new(raw)), Length::Unframed, encoding),
        Some(Encoding::Deflate) => (Box::new(ZlibDecoder::new(raw)), Length::Unframed, encoding),
        None => {
            let length = match framing {
                Some(Framing::Length(n)) => Length::Exact(n),
                _ => Length::Unframed,
            };
            (raw, length, None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn parse(raw: &str) -> Result<Head, UpstreamError> {
        parse_head(raw.as_bytes(), false)
    }

    fn framing_of(raw: &str) -> Result<Option<Framing>, UpstreamError> {
        parse(raw).map(|h| h.framing)
    }

    #[test]
    fn head_keeps_headers_raw_in_order() {
        let h = parse("HTTP/1.1 302 Found\r\nSet-Cookie: a\r\nlocation: /x\r\nSET-COOKIE: b\r\nContent-Length: 0\r\n\r\n")
            .unwrap();
        assert_eq!((h.status, h.reason.as_str()), (302, "Found"));
        let names: Vec<(&str, &[u8])> = h.headers.iter().map(|h| (h.name.as_str(), h.value.as_slice())).collect();
        assert_eq!(
            names,
            vec![
                ("Set-Cookie", &b"a"[..]),
                ("location", b"/x"),
                ("SET-COOKIE", b"b"),
                ("Content-Length", b"0"),
            ]
        );
    }

    #[test]
    fn framings() {
        assert_eq!(framing_of("HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n").unwrap(), Some(Framing::Length(5)));
        assert_eq!(
            framing_of("HTTP/1.1 200 OK\r\nContent-Length: 5, 5\r\nContent-Length: 5\r\n\r\n").unwrap(),
            Some(Framing::Length(5))
        );
        assert_eq!(framing_of("HTTP/1.1 200 OK\r\nTransfer-Encoding: Chunked\r\n\r\n").unwrap(), Some(Framing::Chunked));
        assert_eq!(framing_of("HTTP/1.1 200 OK\r\n\r\n").unwrap(), None);
        assert_eq!(framing_of("HTTP/1.0 200 OK\r\n\r\n").unwrap(), None);
        for raw in [
            "HTTP/1.1 204 No Content\r\nContent-Length: 9\r\n\r\n",
            "HTTP/1.1 304 Not Modified\r\nTransfer-Encoding: chunked\r\n\r\n",
        ] {
            assert_eq!(framing_of(raw).unwrap(), Some(Framing::Empty), "{raw}");
        }
        let h = parse_head(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\n", true).unwrap();
        assert_eq!(h.framing, Some(Framing::Empty));
    }

    #[test]
    fn ambiguous_framings_refused() {
        for raw in [
            "HTTP/1.1 200 OK\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip, chunked\r\n\r\n",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n\r\n",
            "HTTP/1.0 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: +1\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length:\r\n\r\n",
            "HTTP/1.1 200 OK\r\nX: a\r\n b\r\n\r\n",
            "HTTP/2 200 OK\r\n\r\n",
            "HTTP/1.1 2000 OK\r\n\r\n",
        ] {
            assert!(matches!(parse(raw), Err(UpstreamError::BadResponse(_))), "{raw:?}");
        }
    }

    #[test]
    fn interim_responses_skipped() {
        let raw = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 103 Early Hints\r\nLink: </a>\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi";
        let mut r = &raw[..];
        let h = read_head(&mut r, false).unwrap();
        assert_eq!((h.status, h.framing), (200, Some(Framing::Length(2))));
        assert_eq!(r, b"hi");

        let mut r = &b"HTTP/1.1 101 Switching Protocols\r\n\r\n"[..];
        let e = read_head(&mut r, false).unwrap_err();
        assert!(matches!(super::super::upstream_error(&e), Some(UpstreamError::BadResponse(_))));

        let endless = "HTTP/1.1 103 Early Hints\r\n\r\n".repeat(MAX_INTERIM + 1);
        let e = read_head(&mut endless.as_bytes(), false).unwrap_err();
        assert!(matches!(super::super::upstream_error(&e), Some(UpstreamError::BadResponse(_))));

        let e = read_head(&mut &b"HTTP/1.1 200 OK\r\n"[..], false).unwrap_err();
        assert!(matches!(super::super::upstream_error(&e), Some(UpstreamError::BadResponse(_))));
        let e = read_head(&mut &b"HTTP/1.1 200 OK\nX: y\n\n"[..], false).unwrap_err();
        assert!(matches!(super::super::upstream_error(&e), Some(UpstreamError::BadResponse(_))));
    }

    fn headers(ce: &[&str]) -> Vec<Header> {
        ce.iter()
            .map(|v| Header {
                name: "Content-Encoding".into(),
                value: v.as_bytes().to_vec(),
            })
            .collect()
    }

    #[test]
    fn encodings() {
        assert_eq!(content_encoding(&headers(&[])).unwrap(), None);
        assert_eq!(content_encoding(&headers(&["identity"])).unwrap(), None);
        assert_eq!(content_encoding(&headers(&["GZIP"])).unwrap(), Some(Encoding::Gzip));
        assert_eq!(content_encoding(&headers(&["x-gzip"])).unwrap(), Some(Encoding::Gzip));
        assert_eq!(content_encoding(&headers(&["identity, deflate"])).unwrap(), Some(Encoding::Deflate));
        for refused in [&["br"][..], &["zstd"], &["gzip, gzip"], &["gzip", "deflate"], &["compress"]] {
            assert!(matches!(content_encoding(&headers(refused)), Err(UpstreamError::Encoding(_))), "{refused:?}");
        }
    }

    fn read_all(r: &mut dyn Read) -> Vec<u8> {
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        out
    }

    #[test]
    fn bodies() {
        let (mut b, len, dec) = body(&b"helloNEXT"[..], Some(Framing::Length(5)), None);
        assert_eq!((read_all(&mut b), len, dec), (b"hello".to_vec(), Length::Exact(5), None));

        let (mut b, len, _) = body(&b"5\r\nhello\r\n0\r\n\r\n"[..], Some(Framing::Chunked), None);
        assert_eq!((read_all(&mut b), len), (b"hello".to_vec(), Length::Unframed));

        let (mut b, len, _) = body(&b"to the end"[..], None, None);
        assert_eq!((read_all(&mut b), len), (b"to the end".to_vec(), Length::Unframed));

        let (mut b, len, dec) = body(&b"ignored"[..], Some(Framing::Empty), Some(Encoding::Gzip));
        assert_eq!((read_all(&mut b), len, dec), (Vec::new(), Length::Empty, None));
    }

    #[test]
    fn decoded_bodies() {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(b"zipped ").unwrap();
        let mut gz = gz.finish().unwrap();
        let mut second = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        second.write_all(b"twice").unwrap();
        gz.extend(second.finish().unwrap());
        let framing = Some(Framing::Length(gz.len() as u64));
        let (mut b, len, dec) = body(io::Cursor::new(gz), framing, Some(Encoding::Gzip));
        assert_eq!((read_all(&mut b), len, dec), (b"zipped twice".to_vec(), Length::Unframed, Some(Encoding::Gzip)));

        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(b"deflated").unwrap();
        let (mut b, _, dec) = body(io::Cursor::new(z.finish().unwrap()), None, Some(Encoding::Deflate));
        assert_eq!((read_all(&mut b), dec), (b"deflated".to_vec(), Some(Encoding::Deflate)));

        let (mut b, _, _) = body(&b"not gzip"[..], None, Some(Encoding::Gzip));
        assert!(b.read_to_end(&mut Vec::new()).is_err());
    }
}
