//! A request head with strict framing: what the broker reads at the start
//! of every request on a connection, base URL (plain HTTP from the tool)
//! and intercept (inside the TLS the broker terminates after devsbd's
//! `CONNECT`) alike, so both see origin-form targets; absolute-form is
//! accepted too, since RFC 9112 requires it and it costs a split.
//!
//! Line endings are CRLF only: a bare LF (which httparse would accept) or a
//! lone CR anywhere in the head is refused, as are obs-fold and whitespace
//! before a colon (httparse refuses both in requests). Empty lines before
//! the request line are skipped (RFC 9112 2.2), counted against
//! [`MAX_HEAD`].
//!
//! The body length is decided here, once ([`Framing`]), so nothing
//! downstream can read a different one: `Content-Length` together with
//! `Transfer-Encoding`, `Content-Length`s that differ, a
//! `Transfer-Encoding` other than exactly one `chunked` (or any on
//! HTTP/1.0) are refused, as are `Upgrade` and `Connection: upgrade`.

use std::io::{self, BufRead};

use super::HttpError;

pub const MAX_HEAD: usize = 64 * 1024;
pub const MAX_HEADERS: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version {
    Http10,
    Http11,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Origin(String),
    Absolute {
        https: bool,
        authority: String,
        path: String,
    },
}

impl Target {
    fn parse(raw: &str) -> Result<Target, HttpError> {
        if raw.contains('#') {
            return Err(HttpError::BadTarget);
        }
        if raw.starts_with('/') {
            return Ok(Target::Origin(raw.to_string()));
        }
        let (https, rest) = match raw.split_once("://") {
            Some((scheme, rest)) if scheme.eq_ignore_ascii_case("http") => (false, rest),
            Some((scheme, rest)) if scheme.eq_ignore_ascii_case("https") => (true, rest),
            _ => return Err(HttpError::BadTarget),
        };
        let end = rest.find(['/', '?']).unwrap_or(rest.len());
        let (authority, path) = rest.split_at(end);
        if authority.is_empty() || authority.contains('@') {
            return Err(HttpError::BadTarget);
        }
        let path = match path {
            "" => "/".to_string(),
            p if p.starts_with('?') => format!("/{p}"),
            p => p.to_string(),
        };
        Ok(Target::Absolute {
            https,
            authority: authority.to_string(),
            path,
        })
    }

    pub fn path(&self) -> &str {
        match self {
            Target::Origin(p) | Target::Absolute { path: p, .. } => p,
        }
    }
}

/// The name keeps its case (lookups ignore it), the value is bytes
/// (obs-text is legal) with surrounding whitespace trimmed by httparse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub name: String,
    pub value: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    /// Neither `Content-Length` nor `Transfer-Encoding`: no body.
    Empty,
    Length(u64),
    Chunked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestHead {
    pub method: String,
    pub target: Target,
    pub version: Version,
    pub headers: Vec<Header>,
    pub framing: Framing,
    /// `Expect: 100-continue` on an HTTP/1.1 request: the caller sends
    /// `100 Continue` once policy allows the call, or its refusal instead
    /// (HTTP/1.0's `Expect` is ignored, RFC 9110 10.1.1).
    pub expect_continue: bool,
}

impl RequestHead {
    pub fn keep_alive(&self) -> bool {
        let mut tokens = list_elements(&self.headers, "connection");
        match self.version {
            Version::Http11 => !tokens.any(|t| t.eq_ignore_ascii_case(b"close")),
            Version::Http10 => {
                let tokens: Vec<_> = tokens.collect();
                !tokens.iter().any(|t| t.eq_ignore_ascii_case(b"close"))
                    && tokens.iter().any(|t| t.eq_ignore_ascii_case(b"keep-alive"))
            }
        }
    }
}

/// `None` when the connection ends cleanly before a request starts (the
/// end of a keep-alive connection).
pub fn read_head<R: BufRead>(r: &mut R) -> io::Result<Option<RequestHead>> {
    let mut head = Vec::new();
    let mut seen = 0usize;
    loop {
        let buf = r.fill_buf()?;
        if buf.is_empty() {
            return if head.is_empty() {
                Ok(None)
            } else {
                Err(HttpError::Truncated.into())
            };
        }
        let mut used = 0;
        let mut done = false;
        for &b in buf {
            used += 1;
            seen += 1;
            if seen > MAX_HEAD {
                return Err(HttpError::HeadTooLarge.into());
            }
            match (head.last(), b) {
                (Some(b'\r'), b'\n') => {}
                (Some(b'\r'), _) | (_, b'\n') => return Err(HttpError::BareLineEnding.into()),
                _ => {}
            }
            head.push(b);
            if head == b"\r\n" {
                head.clear();
            } else if head.ends_with(b"\r\n\r\n") {
                done = true;
                break;
            }
        }
        r.consume(used);
        if done {
            return Ok(Some(parse_head(&head)?));
        }
    }
}

pub fn parse_head(buf: &[u8]) -> Result<RequestHead, HttpError> {
    let mut slots = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut req = httparse::Request::new(&mut slots);
    match req.parse(buf) {
        Ok(httparse::Status::Complete(n)) if n == buf.len() => {}
        Ok(_) => return Err(HttpError::Malformed),
        Err(httparse::Error::TooManyHeaders) => return Err(HttpError::TooManyHeaders),
        Err(httparse::Error::Version) => return Err(HttpError::UnsupportedVersion),
        Err(_) => return Err(HttpError::Malformed),
    }
    let version = match req.version {
        Some(1) => Version::Http11,
        Some(0) => Version::Http10,
        _ => return Err(HttpError::UnsupportedVersion),
    };
    let method = req.method.ok_or(HttpError::Malformed)?.to_string();
    if method.eq_ignore_ascii_case("CONNECT") {
        return Err(HttpError::Connect);
    }
    let target = Target::parse(req.path.ok_or(HttpError::Malformed)?)?;
    let headers: Vec<Header> = req
        .headers
        .iter()
        .map(|h| Header {
            name: h.name.to_string(),
            value: h.value.to_vec(),
        })
        .collect();

    if header_count(&headers, "upgrade") > 0
        || list_elements(&headers, "connection").any(|t| t.eq_ignore_ascii_case(b"upgrade"))
    {
        return Err(HttpError::Upgrade);
    }
    let framing = framing(version, &headers)?;
    let expect_continue = match (version, header_count(&headers, "expect")) {
        (Version::Http10, _) | (_, 0) => false,
        (_, 1) if trim_ows(header(&headers, "expect").unwrap_or_default())
            .eq_ignore_ascii_case(b"100-continue") =>
        {
            true
        }
        _ => return Err(HttpError::Expectation),
    };
    Ok(RequestHead {
        method,
        target,
        version,
        headers,
        framing,
        expect_continue,
    })
}

fn framing(version: Version, headers: &[Header]) -> Result<Framing, HttpError> {
    let has_te = header_count(headers, "transfer-encoding") > 0;
    let has_cl = header_count(headers, "content-length") > 0;
    match (has_te, has_cl) {
        (true, true) => Err(HttpError::LengthAndChunked),
        (true, false) => {
            let codings: Vec<&[u8]> = list_elements(headers, "transfer-encoding").collect();
            if version == Version::Http11
                && codings.len() == 1
                && codings[0].eq_ignore_ascii_case(b"chunked")
            {
                Ok(Framing::Chunked)
            } else {
                Err(HttpError::BadTransferEncoding)
            }
        }
        (false, true) => {
            let mut length = None;
            for v in list_elements(headers, "content-length") {
                let n = parse_decimal(v).ok_or(HttpError::BadContentLength)?;
                if length.is_some_and(|l| l != n) {
                    return Err(HttpError::BadContentLength);
                }
                length = Some(n);
            }
            length.map(Framing::Length).ok_or(HttpError::BadContentLength)
        }
        (false, false) => Ok(Framing::Empty),
    }
}

fn parse_decimal(v: &[u8]) -> Option<u64> {
    if v.is_empty() || !v.iter().all(u8::is_ascii_digit) {
        return None;
    }
    v.iter()
        .try_fold(0u64, |n, d| n.checked_mul(10)?.checked_add(u64::from(d - b'0')))
}

/// Case-insensitive: policy refuses a request carrying its inject slot twice.
pub fn header_indices<'a>(headers: &'a [Header], name: &'a str) -> impl Iterator<Item = usize> + 'a {
    headers
        .iter()
        .enumerate()
        .filter(move |(_, h)| h.name.eq_ignore_ascii_case(name))
        .map(|(i, _)| i)
}

pub fn header_count(headers: &[Header], name: &str) -> usize {
    header_indices(headers, name).count()
}

pub fn header<'a>(headers: &'a [Header], name: &str) -> Option<&'a [u8]> {
    headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(name))
        .map(|h| h.value.as_slice())
}

/// Whitespace-trimmed (empty elements kept, so `chunked,` is two codings).
fn list_elements<'a>(headers: &'a [Header], name: &'a str) -> impl Iterator<Item = &'a [u8]> + 'a {
    header_indices(headers, name)
        .flat_map(move |i| headers[i].value.split(|&b| b == b','))
        .map(trim_ows)
}

fn trim_ows(v: &[u8]) -> &[u8] {
    let ows = |b: &u8| *b == b' ' || *b == b'\t';
    let start = v.iter().position(|b| !ows(b)).unwrap_or(v.len());
    let end = v.iter().rposition(|b| !ows(b)).map_or(start, |e| e + 1);
    &v[start..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broker::refusal;

    fn parse(s: &str) -> Result<RequestHead, HttpError> {
        parse_head(s.as_bytes())
    }

    fn read(s: &[u8]) -> Result<Option<RequestHead>, HttpError> {
        let mut r = s;
        read_head(&mut r).map_err(|e| refusal(&e).cloned().expect("a refusal"))
    }

    #[test]
    fn origin_form_get() {
        let h = parse("GET /api/0/projects/?a=b HTTP/1.1\r\nHost: 127.0.0.1:41000\r\n\r\n").unwrap();
        assert_eq!(h.method, "GET");
        assert_eq!(h.target, Target::Origin("/api/0/projects/?a=b".into()));
        assert_eq!(h.version, Version::Http11);
        assert_eq!(h.framing, Framing::Empty);
        assert!(!h.expect_continue);
        assert!(h.keep_alive());
        assert_eq!(header(&h.headers, "HOST"), Some(&b"127.0.0.1:41000"[..]));
    }

    #[test]
    fn absolute_form() {
        let h = parse("GET HTTPS://api.github.com?x=1 HTTP/1.1\r\nHost: api.github.com\r\n\r\n").unwrap();
        assert_eq!(
            h.target,
            Target::Absolute {
                https: true,
                authority: "api.github.com".into(),
                path: "/?x=1".into()
            }
        );
        assert_eq!(h.target.path(), "/?x=1");
        let h = parse("GET http://h:8080 HTTP/1.1\r\nHost: h:8080\r\n\r\n").unwrap();
        assert_eq!(h.target.path(), "/");
    }

    #[test]
    fn other_targets_refused() {
        for t in ["*", "api.github.com:443", "ftp://h/", "http:///x", "http://u@h/", "/a#frag"] {
            assert_eq!(
                parse(&format!("OPTIONS {t} HTTP/1.1\r\nHost: h\r\n\r\n")),
                Err(HttpError::BadTarget),
                "{t}"
            );
        }
    }

    #[test]
    fn connect_refused() {
        assert_eq!(
            parse("CONNECT api.github.com:443 HTTP/1.1\r\nHost: api.github.com:443\r\n\r\n"),
            Err(HttpError::Connect)
        );
    }

    #[test]
    fn versions() {
        let h = parse("GET / HTTP/1.0\r\n\r\n").unwrap();
        assert_eq!(h.version, Version::Http10);
        assert!(!h.keep_alive());
        let h = parse("GET / HTTP/1.0\r\nConnection: Keep-Alive\r\n\r\n").unwrap();
        assert!(h.keep_alive());
        for v in ["HTTP/2.0", "HTTP/1.2", "HTTP/0.9"] {
            assert_eq!(
                parse(&format!("GET / {v}\r\n\r\n")),
                Err(HttpError::UnsupportedVersion),
                "{v}"
            );
        }
        let h = parse("GET / HTTP/1.1\r\nConnection: keep-alive, Close\r\n\r\n").unwrap();
        assert!(!h.keep_alive());
    }

    #[test]
    fn content_length() {
        let h = parse("POST / HTTP/1.1\r\nContent-Length: 12\r\n\r\n").unwrap();
        assert_eq!(h.framing, Framing::Length(12));
        // Repeats of one value are one length (RFC 9112 6.3).
        let h = parse("POST / HTTP/1.1\r\nContent-Length: 5\r\ncontent-length: 5, 5\r\n\r\n").unwrap();
        assert_eq!(h.framing, Framing::Length(5));
        for cl in [
            "Content-Length: 5\r\nContent-Length: 6",
            "Content-Length: 5, 6",
            "Content-Length: +5",
            "Content-Length: -1",
            "Content-Length: 0x10",
            "Content-Length: 5 5",
            "Content-Length: 5,",
            "Content-Length:",
            "Content-Length: 99999999999999999999",
        ] {
            assert_eq!(
                parse(&format!("POST / HTTP/1.1\r\n{cl}\r\n\r\n")),
                Err(HttpError::BadContentLength),
                "{cl}"
            );
        }
    }

    #[test]
    fn transfer_encoding() {
        let h = parse("POST / HTTP/1.1\r\nTransfer-Encoding: Chunked\r\n\r\n").unwrap();
        assert_eq!(h.framing, Framing::Chunked);
        for te in [
            "Transfer-Encoding: gzip, chunked",
            "Transfer-Encoding: gzip",
            "Transfer-Encoding: chunked, chunked",
            "Transfer-Encoding: chunked\r\nTransfer-Encoding: chunked",
            "Transfer-Encoding: chunked,",
            "Transfer-Encoding: identity",
            "Transfer-Encoding: xchunked",
            "Transfer-Encoding: \"chunked\"",
        ] {
            assert_eq!(
                parse(&format!("POST / HTTP/1.1\r\n{te}\r\n\r\n")),
                Err(HttpError::BadTransferEncoding),
                "{te}"
            );
        }
        assert_eq!(
            parse("POST / HTTP/1.0\r\nTransfer-Encoding: chunked\r\n\r\n"),
            Err(HttpError::BadTransferEncoding)
        );
    }

    #[test]
    fn length_and_chunked_refused() {
        for head in [
            "POST / HTTP/1.1\r\nContent-Length: 4\r\nTransfer-Encoding: chunked\r\n\r\n",
            "POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\nContent-Length: 4\r\n\r\n",
            "POST / HTTP/1.1\r\nTransfer-Encoding: gzip\r\nContent-Length: 4\r\n\r\n",
        ] {
            assert_eq!(parse(head), Err(HttpError::LengthAndChunked), "{head}");
        }
    }

    #[test]
    fn smuggling_header_shapes_refused() {
        for head in [
            // Whitespace before the colon.
            "POST / HTTP/1.1\r\nTransfer-Encoding : chunked\r\n\r\n",
            "POST / HTTP/1.1\r\nContent-Length\t: 4\r\n\r\n",
            // obs-fold.
            "GET / HTTP/1.1\r\nX-A: a\r\n  folded\r\n\r\n",
            "GET / HTTP/1.1\r\nX-A: a\r\n\tfolded\r\n\r\n",
            // A header line with no colon, a leading space before the first one.
            "GET / HTTP/1.1\r\nTransfer-Encoding chunked\r\n\r\n",
            "GET / HTTP/1.1\r\n Host: h\r\n\r\n",
            // Two spaces in the request line.
            "GET  / HTTP/1.1\r\n\r\n",
        ] {
            assert_eq!(parse(head), Err(HttpError::Malformed), "{head:?}");
        }
    }

    #[test]
    fn upgrade_refused() {
        for h in [
            "Upgrade: websocket",
            "Connection: Upgrade",
            "Connection: keep-alive, upgrade",
            "upgrade: h2c",
        ] {
            assert_eq!(
                parse(&format!("GET / HTTP/1.1\r\n{h}\r\n\r\n")),
                Err(HttpError::Upgrade),
                "{h}"
            );
        }
    }

    #[test]
    fn expect() {
        let h = parse("POST / HTTP/1.1\r\nContent-Length: 1\r\nExpect: 100-Continue\r\n\r\n").unwrap();
        assert!(h.expect_continue);
        let h = parse("POST / HTTP/1.0\r\nContent-Length: 1\r\nExpect: whatever\r\n\r\n").unwrap();
        assert!(!h.expect_continue);
        for e in ["Expect: 102-processing", "Expect: 100-continue\r\nExpect: 100-continue"] {
            assert_eq!(
                parse(&format!("POST / HTTP/1.1\r\n{e}\r\n\r\n")),
                Err(HttpError::Expectation),
                "{e}"
            );
        }
    }

    #[test]
    fn header_lookup_counts_case_insensitively() {
        let h = parse("GET / HTTP/1.1\r\nAuthorization: a\r\nX: y\r\nauthorization: b\r\n\r\n").unwrap();
        assert_eq!(header_count(&h.headers, "AUTHORIZATION"), 2);
        assert_eq!(header_indices(&h.headers, "authorization").collect::<Vec<_>>(), vec![0, 2]);
        assert_eq!(header(&h.headers, "Authorization"), Some(&b"a"[..]));
        assert_eq!(header_count(&h.headers, "x-api-key"), 0);
    }

    #[test]
    fn too_many_headers() {
        let mut s = String::from("GET / HTTP/1.1\r\n");
        for i in 0..=MAX_HEADERS {
            s.push_str(&format!("X-{i}: v\r\n"));
        }
        s.push_str("\r\n");
        assert_eq!(parse(&s), Err(HttpError::TooManyHeaders));
    }

    #[test]
    fn read_leaves_the_body() {
        let mut r: &[u8] = b"\r\n\r\nPOST / HTTP/1.1\r\nContent-Length: 3\r\n\r\nabcGET";
        let h = read_head(&mut r).unwrap().unwrap();
        assert_eq!(h.framing, Framing::Length(3));
        assert_eq!(r, b"abcGET");
    }

    #[test]
    fn read_across_small_buffers() {
        let raw = b"GET /x HTTP/1.1\r\nHost: h\r\n\r\nrest";
        let mut r = io::BufReader::with_capacity(3, &raw[..]);
        let h = read_head(&mut r).unwrap().unwrap();
        assert_eq!(h.target.path(), "/x");
        let mut rest = String::new();
        io::Read::read_to_string(&mut r, &mut rest).unwrap();
        assert_eq!(rest, "rest");
    }

    #[test]
    fn read_end_of_connection() {
        assert_eq!(read(b""), Ok(None));
        assert_eq!(read(b"\r\n"), Ok(None));
        assert_eq!(read(b"GET / HTTP/1.1\r\nHost: h\r\n"), Err(HttpError::Truncated));
    }

    #[test]
    fn read_refuses_bare_line_endings() {
        for raw in [
            &b"GET / HTTP/1.1\nHost: h\r\n\r\n"[..],
            b"GET / HTTP/1.1\r\nHost: h\n\r\n",
            b"GET / HTTP/1.1\r\nHost: h\r\n\n",
            b"GET / HTTP/1.1\r\nX: a\rb\r\n\r\n",
            b"\nGET / HTTP/1.1\r\n\r\n",
        ] {
            assert_eq!(read(raw), Err(HttpError::BareLineEnding), "{raw:?}");
        }
    }

    #[test]
    fn read_bounds_the_head() {
        let mut raw = b"GET / HTTP/1.1\r\nX: ".to_vec();
        raw.resize(MAX_HEAD + 10, b'a');
        assert_eq!(read(&raw), Err(HttpError::HeadTooLarge));
        assert_eq!(read(&b"\r\n".repeat(MAX_HEAD)), Err(HttpError::HeadTooLarge));
    }
}
