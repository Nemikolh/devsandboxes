//! The broker's upstream client (docs/plans/active/credential-broker.md,
//! *Shape*, "The broker"): one HTTPS request per connection, over rustls
//! with the host's native roots, through the host's proxy env.
//!
//! It owns the transport and nothing else: no redirects followed (a 3xx
//! comes back as is), no headers of its own beyond `Host`, the body's
//! framing, `Accept-Encoding: identity` and `Connection: close`, and every
//! address dialed passes [`addr::allowed`] first.
//!
//! Hand-rolled over rustls and the broker's own head and body parsers
//! rather than ureq: the check has to run on the address dialed, the proxy
//! tunnel has to be opened to that checked address, and the caller needs
//! the response headers raw and in order; ureq takes a name to its proxy
//! and hands back an `http::HeaderMap`.

pub mod addr;
pub mod proxy;
pub mod response;
pub mod url;

#[cfg(test)]
mod tests;

use std::fmt;
use std::io::{self, BufReader, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};

pub use proxy::Proxy;
pub use response::{Encoding, Length};
pub use url::Target;

use super::head::{Framing, Header};

/// Fields the client writes itself; a caller's list carrying one (or any
/// other hop-by-hop field) is refused, so they are stripped before the
/// call, never sent twice.
pub const RESERVED: &[&str] = &[
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "accept-encoding",
    "keep-alive",
    "proxy-connection",
    "proxy-authorization",
    "te",
    "trailer",
    "upgrade",
    "expect",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    /// Per address dialed (the proxy's or the upstream's).
    pub connect: Duration,
    /// The longest one read or write may block: an idle bound, not a bound
    /// on the call (a streamed response may run for hours).
    pub idle: Duration,
}

impl Default for Timeouts {
    /// 10 s to connect; 5 min idle, as a non-streamed model call may think
    /// that long before its first byte.
    fn default() -> Self {
        Timeouts {
            connect: Duration::from_secs(10),
            idle: Duration::from_secs(300),
        }
    }
}

pub enum Body<'a> {
    /// No body and no framing field (a `GET`).
    Empty,
    /// Sent with `Content-Length`; the reader ending early is an error.
    Length(u64, &'a mut dyn Read),
    /// Sent chunked, one chunk per read.
    Chunked(&'a mut dyn Read),
}

pub struct Request<'a> {
    pub method: &'a str,
    /// `https://host[:port]/path?query` ([`Target::parse`]).
    pub url: &'a str,
    /// Sent verbatim and in order after `Host`; none of [`RESERVED`].
    pub headers: &'a [Header],
    pub body: Body<'a>,
    /// The upstream is one an approved claim marks private: the address
    /// check lets private ranges through ([`addr::allowed`]).
    pub private: bool,
}

pub struct Response {
    pub status: u16,
    pub reason: String,
    /// As received: names in their case, duplicates kept, in order.
    pub headers: Vec<Header>,
    /// How [`Response::body`] relates to `headers`' framing.
    pub length: Length,
    /// The `Content-Encoding` the body was decoded from: the caller drops
    /// that field before passing the headers on.
    pub decoded: Option<Encoding>,
    /// De-framed (and decoded) as it streams; reading past its end never
    /// touches the connection, which closes when this drops.
    pub body: Box<dyn Read + Send>,
}

/// What reaches a caller as an `io::Error` of kind `InvalidData` carrying
/// this ([`upstream_error`]); every other error is the transport's (a
/// timeout is `WouldBlock` or `TimedOut`, a TLS failure `InvalidData`
/// carrying a `rustls::Error`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpstreamError {
    /// A request the client won't send: bad URL, method or field, or a
    /// reserved field. The caller's bug, never the upstream's.
    Invalid(String),
    /// Every address the upstream resolved to failed the address check;
    /// the first one.
    Refused(IpAddr),
    Unresolved,
    /// The proxy answered `CONNECT` with this status.
    Proxy(u16),
    BadResponse(String),
    /// A `Content-Encoding` the client doesn't decode.
    Encoding(String),
}

impl UpstreamError {
    /// The status the broker answers its own client with.
    pub fn status(&self) -> u16 {
        match self {
            UpstreamError::Invalid(_) => 500,
            UpstreamError::Refused(_) => 403,
            _ => 502,
        }
    }
}

impl fmt::Display for UpstreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UpstreamError::Invalid(what) => write!(f, "invalid upstream request: {what}"),
            UpstreamError::Refused(ip) => {
                write!(f, "upstream address {ip} is loopback, link-local, private or reserved")
            }
            UpstreamError::Unresolved => f.write_str("upstream host resolved to no address"),
            UpstreamError::Proxy(code) => write!(f, "proxy refused CONNECT with {code}"),
            UpstreamError::BadResponse(why) => write!(f, "bad upstream response: {why}"),
            UpstreamError::Encoding(enc) => write!(f, "unsupported upstream Content-Encoding {enc:?}"),
        }
    }
}

impl std::error::Error for UpstreamError {}

impl From<UpstreamError> for io::Error {
    fn from(e: UpstreamError) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, e)
    }
}

pub fn upstream_error(e: &io::Error) -> Option<&UpstreamError> {
    e.get_ref()?.downcast_ref()
}

/// A head the shared reader refused (`HttpError`) as a bad response;
/// transport errors as they are.
fn bad_response(e: io::Error, what: &str) -> io::Error {
    match super::refusal(&e) {
        Some(why) => UpstreamError::BadResponse(format!("{what}: {why}")).into(),
        None => e,
    }
}

/// Built once and shared: holds no connection.
pub struct Upstream {
    tls: Arc<ClientConfig>,
    proxy: Option<Proxy>,
    timeouts: Timeouts,
}

impl Upstream {
    /// The host's native roots (so a corporate TLS-inspection CA the host
    /// trusts is trusted) and proxy env.
    #[cfg_attr(test, allow(dead_code))] // Tests inject their roots.
    pub fn from_host() -> Result<Upstream> {
        let found = rustls_native_certs::load_native_certs();
        let mut roots = RootCertStore::empty();
        let (added, _) = roots.add_parsable_certificates(found.certs);
        if added == 0 {
            match found.errors.first() {
                Some(e) => bail!("no usable TLS root certificates on this host: {e}"),
                None => bail!("no TLS root certificates on this host"),
            }
        }
        let proxy = Proxy::from_env().context("reading the host's proxy env")?;
        Upstream::new(roots, proxy, Timeouts::default())
    }

    pub fn new(roots: RootCertStore, proxy: Option<Proxy>, timeouts: Timeouts) -> Result<Upstream> {
        let mut tls = ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .context("TLS protocol versions")?
            .with_root_certificates(roots)
            .with_no_client_auth();
        tls.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Upstream {
            tls: Arc::new(tls),
            proxy,
            timeouts,
        })
    }

    /// Sends the whole request, then reads the response head. A body read
    /// error aborts the request (the connection drops before its end).
    pub fn send(&self, req: Request<'_>) -> io::Result<Response> {
        let target = Target::parse(req.url)?;
        let head = request_head(req.method, &target, req.headers, &req.body)?;
        let addrs = addr::checked((target.host.as_str(), target.port).to_socket_addrs()?.collect(), req.private)?;
        let tcp = match &self.proxy {
            Some(proxy) if !proxy.bypass(&target.host) => first_ok(&addrs, |a| proxy.tunnel(*a, &self.timeouts))?,
            _ => dial(&addrs, &self.timeouts)?,
        };
        let name = ServerName::try_from(target.host.clone())
            .map_err(|_| UpstreamError::Invalid(format!("TLS server name {:?}", target.host)))?;
        let conn = ClientConnection::new(self.tls.clone(), name).map_err(io::Error::other)?;
        let mut tls = StreamOwned::new(conn, tcp);

        tls.write_all(&head)?;
        match req.body {
            Body::Empty => {}
            Body::Length(n, body) => {
                let sent = io::copy(&mut body.take(n), &mut tls)?;
                if sent != n {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!("request body ended after {sent} of {n} bytes"),
                    ));
                }
            }
            Body::Chunked(body) => send_chunked(body, &mut tls)?,
        }
        tls.flush()?;

        let mut conn = BufReader::new(tls);
        let head = response::read_head(&mut conn, req.method.eq_ignore_ascii_case("HEAD"))?;
        let encoding = match head.framing {
            Some(Framing::Empty) => None,
            _ => response::content_encoding(&head.headers)?,
        };
        let (body, length, decoded) = response::body(conn, head.framing, encoding);
        Ok(Response {
            status: head.status,
            reason: head.reason,
            headers: head.headers,
            length,
            decoded,
            body,
        })
    }
}

/// Each chunk is one write (and so one TLS record), flushed, so a body
/// streamed in pieces reaches the upstream in those pieces.
fn send_chunked(body: &mut dyn Read, out: &mut impl Write) -> io::Result<()> {
    let mut buf = vec![0u8; 16 * 1024];
    let mut frame = Vec::with_capacity(buf.len() + 16);
    loop {
        let n = match body.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        frame.clear();
        write!(frame, "{n:x}\r\n")?;
        frame.extend_from_slice(&buf[..n]);
        frame.extend_from_slice(b"\r\n");
        out.write_all(&frame)?;
        out.flush()?;
    }
    out.write_all(b"0\r\n\r\n")
}

fn request_head(method: &str, target: &Target, headers: &[Header], body: &Body<'_>) -> Result<Vec<u8>, UpstreamError> {
    if method.is_empty() || !method.bytes().all(is_tchar) || method.eq_ignore_ascii_case("CONNECT") {
        return Err(UpstreamError::Invalid(format!("method {method:?}")));
    }
    for h in headers {
        if h.name.is_empty() || !h.name.bytes().all(is_tchar) {
            return Err(UpstreamError::Invalid(format!("field name {:?}", h.name)));
        }
        if RESERVED.iter().any(|r| h.name.eq_ignore_ascii_case(r)) {
            return Err(UpstreamError::Invalid(format!("reserved field {:?}", h.name)));
        }
        // VCHAR, SP, HTAB and obs-text: never CR, LF or NUL.
        if h.value.iter().any(|&b| (b < 0x20 && b != b'\t') || b == 0x7f) {
            return Err(UpstreamError::Invalid(format!("value of field {:?}", h.name)));
        }
    }
    let mut out = format!("{method} {} HTTP/1.1\r\nHost: {}\r\n", target.path, target.authority()).into_bytes();
    for h in headers {
        out.extend_from_slice(h.name.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(&h.value);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"Accept-Encoding: identity\r\n");
    match body {
        Body::Empty => {}
        Body::Length(n, _) => out.extend_from_slice(format!("Content-Length: {n}\r\n").as_bytes()),
        Body::Chunked(_) => out.extend_from_slice(b"Transfer-Encoding: chunked\r\n"),
    }
    out.extend_from_slice(b"Connection: close\r\n\r\n");
    Ok(out)
}

fn is_tchar(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// The first of `addrs` that connects, in order.
fn dial(addrs: &[SocketAddr], timeouts: &Timeouts) -> io::Result<TcpStream> {
    first_ok(addrs, |a| {
        let tcp = TcpStream::connect_timeout(a, timeouts.connect)?;
        tcp.set_read_timeout(Some(timeouts.idle))?;
        tcp.set_write_timeout(Some(timeouts.idle))?;
        Ok(tcp)
    })
}

/// The first success, else the last error.
fn first_ok<T>(addrs: &[SocketAddr], mut f: impl FnMut(&SocketAddr) -> io::Result<T>) -> io::Result<T> {
    let mut last = io::Error::from(UpstreamError::Unresolved);
    for a in addrs {
        match f(a) {
            Ok(t) => return Ok(t),
            Err(e) => last = e,
        }
    }
    Err(last)
}
