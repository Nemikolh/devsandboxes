//! `devsbd daemon`'s proxy ports (docs/architecture/helper/in-container.md,
//! *Proxy ports*): the `HTTPS_PROXY` the container's tools use. A
//! `CONNECT host:443` to a host the port binds becomes an `HTTP_PROXY`
//! stream to the broker (intercept); every other target is dialed from the
//! container, directly or through the proxy it had (the broker file's
//! `proxy` entries), so the route never widens what the container reaches.
//!
//! Only the request head is read, with [`HEAD_TIMEOUT`]; after it the
//! connection is a byte relay with no idle timeout (git fetches and SSE
//! streams sit quiet for minutes).

use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv6Addr, Shutdown, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::AtomicU32;
use std::sync::Arc;
use std::time::Duration;

use crate::broker::{respond, Route};
use crate::proto::{channel, ProxyHeader};

/// Bound on a client's request head, and on an upstream proxy's answer.
pub const MAX_HEAD: usize = 8 * 1024;

const HEAD_TIMEOUT: Duration = Duration::from_secs(10);

/// Per address a name resolves to.
const DIAL_TIMEOUT: Duration = Duration::from_secs(10);

const ESTABLISHED: &[u8] = b"HTTP/1.1 200 Connection Established\r\n\r\n";

/// One proxy port's routing.
pub struct ProxyPort {
    /// Whose `CONNECT host:443` goes to the broker.
    pub hosts: Vec<String>,
    pub upstreams: Arc<Upstreams>,
}

/// Accept on `listener` forever, a thread per client: routing may hold it,
/// and a relay lasts as long as its connection.
pub fn serve(listener: TcpListener, port: &Arc<ProxyPort>, route: &Route, ids: &Arc<AtomicU32>) {
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let (port, route, ids) = (Arc::clone(port), Arc::clone(route), Arc::clone(ids));
        std::thread::spawn(move || handle(conn, &port, &route, &ids));
    }
}

fn handle(mut conn: TcpStream, port: &ProxyPort, route: &Route, ids: &AtomicU32) {
    let _ = conn.set_nodelay(true);
    let _ = conn.set_read_timeout(Some(HEAD_TIMEOUT));
    let head = match read_head(&mut conn) {
        Ok(head) => head,
        Err(e) if e.kind() == io::ErrorKind::InvalidData => return respond(conn, "400 Bad Request", &format!("{e}\n")),
        Err(_) => return,
    };
    let request = match parse_request(&head) {
        Ok(request) => request,
        Err(why) => return respond(conn, "400 Bad Request", &format!("{why}\n")),
    };
    let _ = conn.set_read_timeout(None);
    match request {
        Request::Connect { host, port: 443 } if port.hosts.contains(&host) => {
            let Some(mux) = route() else { return crate::broker::refuse(conn) };
            // Before `Open`, so it precedes anything the broker sends.
            if conn.write_all(ESTABLISHED).is_err() {
                return;
            }
            let header = ProxyHeader::Intercept { host, port: 443 }.encode();
            let stream = crate::daemon::next_stream_id(ids);
            let _ = mux.attach_with_header(stream, conn, channel::HTTP_PROXY, &header);
        }
        Request::Connect { host, port: target } => match open_tunnel(&port.upstreams, &host, target) {
            Ok(upstream) => {
                if conn.write_all(ESTABLISHED).is_ok() {
                    relay(conn, upstream);
                }
            }
            Err(why) => respond(conn, "502 Bad Gateway", &format!("{why}\n")),
        },
        Request::Forward(fwd) => match open_forward(&port.upstreams, &fwd) {
            Ok((mut upstream, head)) => {
                if upstream.write_all(&head).is_ok() {
                    relay(conn, upstream);
                }
            }
            Err(why) => respond(conn, "502 Bad Gateway", &format!("{why}\n")),
        },
    }
}

/// Read up to and including the blank line ending a head, one byte at a
/// time so the bytes after it stay in the socket for the relay.
/// `InvalidData`: no head within [`MAX_HEAD`].
pub fn read_head(r: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() == MAX_HEAD {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("no request head in the first {MAX_HEAD} bytes")));
        }
        match r.read(&mut byte) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "connection ended inside the head")),
            Ok(_) => head.push(byte[0]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(head)
}

#[derive(Debug, PartialEq, Eq)]
pub enum Request {
    /// Authority-form; `host` normalized ([`normalize_host`]).
    Connect { host: String, port: u16 },
    /// Absolute-form `http://` request.
    Forward(Forward),
}

#[derive(Debug, PartialEq, Eq)]
pub struct Forward {
    pub method: String,
    pub version: String,
    /// The target as the client sent it, for an upstream proxy.
    pub absolute: String,
    /// Path and query, for the origin itself.
    pub origin: String,
    pub host: String,
    pub port: u16,
    pub headers: Vec<(String, String)>,
}

/// Parse a head [`read_head`] returned, strictly: ASCII, CRLF line ends,
/// single-space request line, `HTTP/1.0|1.1`, token header names, no
/// folded lines.
pub fn parse_request(head: &[u8]) -> Result<Request, String> {
    if !head.is_ascii() {
        return Err("request head isn't ASCII".into());
    }
    let text = std::str::from_utf8(head).map_err(|_| "request head isn't ASCII")?;
    let text = text.strip_suffix("\r\n\r\n").ok_or("request head without its blank line")?;
    let mut lines = text.split("\r\n");
    let line = lines.next().unwrap_or_default();
    let fields: Vec<&str> = line.split(' ').collect();
    let [method, target, version] = fields[..] else {
        return Err(format!("request line `{line}`"));
    };
    if method.is_empty() || !method.bytes().all(|b| b.is_ascii_uppercase()) {
        return Err(format!("method `{method}`"));
    }
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(format!("version `{version}`"));
    }
    if target.is_empty() || !target.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(format!("request target `{target}`"));
    }
    let mut headers = Vec::new();
    for line in lines {
        headers.push(parse_header(line)?);
    }
    if method == "CONNECT" {
        let (host, port) = split_authority(target, None)?;
        return Ok(Request::Connect { host, port });
    }
    let Some(rest) = target.get(..7).filter(|s| s.eq_ignore_ascii_case("http://")).map(|_| &target[7..]) else {
        return Err(format!("`{target}` isn't a proxy request (want CONNECT or an http:// target)"));
    };
    if target.contains('#') {
        return Err(format!("request target `{target}` has a fragment"));
    }
    let end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, path) = rest.split_at(end);
    if authority.contains('@') {
        return Err(format!("request target `{target}` has userinfo"));
    }
    let (host, port) = split_authority(authority, Some(80))?;
    let origin = match path {
        "" => "/".to_string(),
        p if p.starts_with('?') => format!("/{p}"),
        p => p.to_string(),
    };
    Ok(Request::Forward(Forward {
        method: method.to_string(),
        version: version.to_string(),
        absolute: target.to_string(),
        origin,
        host,
        port,
        headers,
    }))
}

fn parse_header(line: &str) -> Result<(String, String), String> {
    if line.contains(['\r', '\n']) {
        return Err("bare CR or LF in the head".into());
    }
    if line.starts_with([' ', '\t']) {
        return Err("folded header line".into());
    }
    let (name, value) = line.split_once(':').ok_or_else(|| format!("header line `{line}`"))?;
    let tchar = |b: u8| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b);
    if name.is_empty() || !name.bytes().all(tchar) {
        return Err(format!("header name `{name}`"));
    }
    let value = value.trim_matches([' ', '\t']);
    if value.bytes().any(|b| (b < 0x20 && b != b'\t') || b == 0x7f) {
        return Err(format!("header `{name}`'s value"));
    }
    Ok((name.to_string(), value.to_string()))
}

/// `host[:port]` or `[v6][:port]`; `default` when the port is absent
/// (`None`: required).
fn split_authority(s: &str, default: Option<u16>) -> Result<(String, u16), String> {
    let (host, port) = if s.starts_with('[') {
        let end = s.find(']').ok_or_else(|| format!("authority `{s}`"))?;
        let (host, rest) = s.split_at(end + 1);
        match rest {
            "" => (host, None),
            rest => (host, Some(rest.strip_prefix(':').ok_or_else(|| format!("authority `{s}`"))?)),
        }
    } else {
        match s.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (s, None),
        }
    };
    let port = match port {
        Some(p) => parse_port(p).ok_or_else(|| format!("port `{p}` in `{s}`"))?,
        None => default.ok_or_else(|| format!("`{s}` has no port"))?,
    };
    Ok((normalize_host(host)?, port))
}

fn parse_port(port: &str) -> Option<u16> {
    match port.parse::<u16>() {
        Ok(p) if p != 0 && port.bytes().all(|b| b.is_ascii_digit()) && !port.starts_with('0') => Some(p),
        _ => None,
    }
}

/// Case-folded, one trailing dot dropped, so `GitHub.com.` matches a bound
/// `github.com`; an IPv6 literal keeps its brackets.
fn normalize_host(raw: &str) -> Result<String, String> {
    if let Some(inner) = raw.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
        return match inner.parse::<Ipv6Addr>() {
            Ok(_) => Ok(format!("[{}]", inner.to_ascii_lowercase())),
            Err(_) => Err(format!("host `{raw}`")),
        };
    }
    let lower = raw.to_ascii_lowercase();
    let host = lower.strip_suffix('.').unwrap_or(&lower);
    let ok = host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty() && label.len() <= 63 && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        });
    if !ok {
        return Err(format!("host `{raw}`"));
    }
    Ok(host.to_string())
}

/// The proxy the container had, from the broker file's `proxy` entries.
#[derive(Debug, Default)]
pub struct Upstreams {
    /// For `CONNECT` targets.
    https: Option<Result<ProxyUrl, String>>,
    /// For absolute-form `http://` requests.
    http: Option<Result<ProxyUrl, String>>,
    no_proxy: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyUrl {
    host: String,
    port: u16,
    /// A `Proxy-Authorization` value, from the URL's userinfo.
    auth: Option<String>,
}

impl Upstreams {
    /// Lowercase names win over uppercase, as curl's; an empty value is
    /// unset; of a name set twice, the later.
    pub fn from_env(env: &[(String, String)]) -> Upstreams {
        let get = |names: &[&str]| {
            names.iter().find_map(|n| env.iter().rev().find(|(k, _)| k == n).map(|(_, v)| v.as_str()).filter(|v| !v.is_empty()))
        };
        Upstreams {
            https: get(&["https_proxy", "HTTPS_PROXY", "all_proxy", "ALL_PROXY"]).map(parse_proxy_url),
            http: get(&["http_proxy", "HTTP_PROXY", "all_proxy", "ALL_PROXY"]).map(parse_proxy_url),
            no_proxy: get(&["no_proxy", "NO_PROXY"]).unwrap_or_default().to_string(),
        }
    }

    /// Why a proxy variable can't be used; each such dial gets a 502
    /// rather than a direct dial past the container's proxy.
    pub fn problems(&self) -> impl Iterator<Item = &String> {
        [&self.https, &self.http].into_iter().filter_map(|p| p.as_ref().and_then(|p| p.as_ref().err()))
    }

    /// `Ok(None)`: dial direct.
    fn via(&self, tunnel: bool, host: &str, port: u16) -> Result<Option<&ProxyUrl>, String> {
        match if tunnel { &self.https } else { &self.http } {
            None => Ok(None),
            Some(_) if bypass(&self.no_proxy, host, port) => Ok(None),
            Some(Ok(proxy)) => Ok(Some(proxy)),
            Some(Err(why)) => Err(why.clone()),
        }
    }
}

/// `[http://][user[:pass]@]host[:port][/]`, port 80 by default (as Go,
/// Node and Python read it). Errors never echo the URL: it may hold a
/// password.
fn parse_proxy_url(url: &str) -> Result<ProxyUrl, String> {
    let rest = match url.split_once("://") {
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("http") => rest,
        Some((scheme, _)) => return Err(format!("the container's proxy is {scheme}://, only http:// proxies are supported")),
        None => url,
    };
    let bad = || "the container's proxy URL doesn't parse".to_string();
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let (userinfo, hostport) = match authority.rsplit_once('@') {
        Some((userinfo, hostport)) => (Some(userinfo), hostport),
        None => (None, authority),
    };
    let (host, port) = split_authority(hostport, Some(80)).map_err(|_| bad())?;
    let auth = match userinfo {
        None => None,
        Some(userinfo) => {
            let (user, pass) = userinfo.split_once(':').unwrap_or((userinfo, ""));
            let mut plain = pct_decode(user).ok_or_else(bad)?;
            plain.push(b':');
            plain.extend(pct_decode(pass).ok_or_else(bad)?);
            Some(format!("Basic {}", base64(&plain)))
        }
    };
    Ok(ProxyUrl { host, port, auth })
}

fn pct_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        if b != b'%' {
            out.push(b);
            continue;
        }
        let hex = [bytes.next()?, bytes.next()?];
        out.push(u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?);
    }
    Some(out)
}

fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, &b)| n | ((b as u32) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Whether `no_proxy` exempts `host:port`: `*`, a domain (leading `.` or
/// `*.` optional) matching itself and its subdomains, an IP, or a CIDR
/// matching IP literals; any of them with `:port` matching that port only.
fn bypass(no_proxy: &str, host: &str, port: u16) -> bool {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let ip = bare.parse::<IpAddr>().ok();
    no_proxy.split(|c: char| c == ',' || c.is_ascii_whitespace()).filter(|e| !e.is_empty()).any(|entry| {
        let entry = entry.to_ascii_lowercase();
        if entry == "*" {
            return true;
        }
        let (name, want) = split_entry(&entry);
        if want.is_some_and(|p| p != port) {
            return false;
        }
        if let Some((net, bits)) = name.split_once('/') {
            return ip.is_some_and(|ip| in_cidr(ip, net, bits));
        }
        let name = name.trim_start_matches("*.").trim_start_matches('.');
        let name = name.strip_suffix('.').unwrap_or(name);
        if let (Some(ip), Ok(want)) = (ip, name.parse::<IpAddr>()) {
            return ip == want;
        }
        !name.is_empty() && (bare == name || bare.ends_with(&format!(".{name}")))
    })
}

fn split_entry(entry: &str) -> (&str, Option<u16>) {
    if let Some((host, tail)) = entry.strip_prefix('[').and_then(|r| r.split_once(']')) {
        return (host, tail.strip_prefix(':').and_then(|p| p.parse().ok()));
    }
    match entry.split_once(':') {
        Some((host, port)) if !port.contains(':') => (host, port.parse().ok()),
        _ => (entry, None),
    }
}

fn in_cidr(ip: IpAddr, net: &str, bits: &str) -> bool {
    let (Ok(net), Ok(bits)) = (net.parse::<IpAddr>(), bits.parse::<u32>()) else { return false };
    let same = |a: u128, b: u128, width: u32| bits == 0 || (a ^ b) >> (width - bits) == 0;
    match (ip, net) {
        (IpAddr::V4(a), IpAddr::V4(n)) if bits <= 32 => same(u32::from(a).into(), u32::from(n).into(), 32),
        (IpAddr::V6(a), IpAddr::V6(n)) if bits <= 128 => same(a.into(), n.into(), 128),
        _ => false,
    }
}

/// Dial `host` with the container's resolver, each address in turn.
fn dial(host: &str, port: u16) -> io::Result<TcpStream> {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let mut last = io::Error::new(io::ErrorKind::NotFound, format!("`{host}` resolves to no address"));
    for addr in (bare, port).to_socket_addrs()? {
        match TcpStream::connect_timeout(&addr, DIAL_TIMEOUT) {
            Ok(s) => {
                let _ = s.set_nodelay(true);
                return Ok(s);
            }
            Err(e) => last = e,
        }
    }
    Err(last)
}

fn dial_proxy(proxy: &ProxyUrl) -> Result<TcpStream, String> {
    dial(&proxy.host, proxy.port).map_err(|e| format!("dialing the container's proxy {}:{}: {e}", proxy.host, proxy.port))
}

/// A connection to `host:port` ready to carry the client's bytes.
fn open_tunnel(upstreams: &Upstreams, host: &str, port: u16) -> Result<TcpStream, String> {
    let target = format!("{host}:{port}");
    let Some(proxy) = upstreams.via(true, host, port)? else {
        return dial(host, port).map_err(|e| format!("dialing {target}: {e}"));
    };
    let mut s = dial_proxy(proxy)?;
    let mut req = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n");
    if let Some(auth) = &proxy.auth {
        req.push_str(&format!("Proxy-Authorization: {auth}\r\n"));
    }
    req.push_str("\r\n");
    let _ = s.set_read_timeout(Some(HEAD_TIMEOUT));
    let answer = s.write_all(req.as_bytes()).and_then(|()| read_head(&mut s));
    let answer = answer.map_err(|e| format!("the container's proxy, CONNECT {target}: {e}"))?;
    match status_code(&answer) {
        Some(200..=299) => {}
        Some(code) => return Err(format!("the container's proxy answered {code} to CONNECT {target}")),
        None => return Err(format!("the container's proxy sent no HTTP status to CONNECT {target}")),
    }
    let _ = s.set_read_timeout(None);
    Ok(s)
}

fn status_code(head: &[u8]) -> Option<u16> {
    let line = head.split(|&b| b == b'\r').next()?;
    let line = std::str::from_utf8(line).ok()?;
    let mut fields = line.split(' ');
    let version = fields.next()?;
    let code = fields.next()?;
    (version.starts_with("HTTP/1.") && code.len() == 3).then(|| code.parse().ok()).flatten()
}

/// The upstream for `fwd` and the head to send it.
fn open_forward(upstreams: &Upstreams, fwd: &Forward) -> Result<(TcpStream, Vec<u8>), String> {
    match upstreams.via(false, &fwd.host, fwd.port)? {
        None => {
            let s = dial(&fwd.host, fwd.port).map_err(|e| format!("dialing {}:{}: {e}", fwd.host, fwd.port))?;
            Ok((s, forward_head(fwd, None)))
        }
        Some(proxy) => Ok((dial_proxy(proxy)?, forward_head(fwd, Some(proxy)))),
    }
}

/// `fwd`'s head for the origin (origin-form target) or for `via` (as sent,
/// with `via`'s credentials). `Connection: close` unless the client asks
/// to upgrade: after this head the connection is a relay to one
/// destination, so a kept-alive client's next request, maybe for another
/// host, must come on a new connection.
fn forward_head(fwd: &Forward, via: Option<&ProxyUrl>) -> Vec<u8> {
    let upgrade = fwd.headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("connection") && value.split(',').any(|t| t.trim().eq_ignore_ascii_case("upgrade"))
    });
    let target = if via.is_some() { &fwd.absolute } else { &fwd.origin };
    let mut out = format!("{} {target} {}\r\n", fwd.method, fwd.version);
    for (name, value) in &fwd.headers {
        let hop = ["proxy-authorization", "proxy-connection", "keep-alive"].iter().any(|h| name.eq_ignore_ascii_case(h))
            || (!upgrade && name.eq_ignore_ascii_case("connection"));
        if !hop {
            out.push_str(&format!("{name}: {value}\r\n"));
        }
    }
    if let Some(auth) = via.and_then(|p| p.auth.as_ref()) {
        out.push_str(&format!("Proxy-Authorization: {auth}\r\n"));
    }
    if !upgrade {
        out.push_str("Connection: close\r\n");
    }
    out.push_str("\r\n");
    out.into_bytes()
}

/// Copy both ways until both sides end, passing half-closes on.
fn relay(client: TcpStream, upstream: TcpStream) {
    let (Ok(c), Ok(u)) = (client.try_clone(), upstream.try_clone()) else { return };
    let up = std::thread::spawn(move || pipe(c, u));
    pipe(upstream, client);
    let _ = up.join();
}

fn pipe(mut from: TcpStream, mut to: TcpStream) {
    match io::copy(&mut from, &mut to) {
        Ok(_) => {
            let _ = to.shutdown(Shutdown::Write);
        }
        Err(_) => {
            let _ = from.shutdown(Shutdown::Both);
            let _ = to.shutdown(Shutdown::Both);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use std::sync::mpsc;

    fn connect(target: &str) -> Request {
        parse_request(format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").as_bytes()).unwrap()
    }

    #[test]
    fn connect_targets_normalize() {
        let want = |host: &str, port| Request::Connect { host: host.into(), port };
        assert_eq!(connect("github.com:443"), want("github.com", 443));
        assert_eq!(connect("GitHub.COM.:443"), want("github.com", 443));
        assert_eq!(connect("api.github.com:8443"), want("api.github.com", 8443));
        assert_eq!(connect("10.0.0.1:22"), want("10.0.0.1", 22));
        assert_eq!(connect("[::1]:443"), want("[::1]", 443));
        assert_eq!(connect("[FE80::1]:443"), want("[fe80::1]", 443));
        assert_eq!(connect("my_host:443"), want("my_host", 443));
        let bare = parse_request(b"CONNECT a.io:443 HTTP/1.0\r\n\r\n").unwrap();
        assert_eq!(bare, want("a.io", 443));
    }

    #[test]
    fn rejects_malformed_heads() {
        for (head, want) in [
            (&b"CONNECT github.com HTTP/1.1\r\n\r\n"[..], "has no port"),
            (b"CONNECT github.com: HTTP/1.1\r\n\r\n", "port ``"),
            (b"CONNECT github.com:0443 HTTP/1.1\r\n\r\n", "port `0443`"),
            (b"CONNECT github.com:70000 HTTP/1.1\r\n\r\n", "port `70000`"),
            (b"CONNECT github.com..:443 HTTP/1.1\r\n\r\n", "host `github.com..`"),
            (b"CONNECT .github.com:443 HTTP/1.1\r\n\r\n", "host"),
            (b"CONNECT ::1:443 HTTP/1.1\r\n\r\n", "host"),
            (b"CONNECT [::1:443 HTTP/1.1\r\n\r\n", "authority"),
            (b"CONNECT [::1]443 HTTP/1.1\r\n\r\n", "authority"),
            (b"CONNECT [nope]:443 HTTP/1.1\r\n\r\n", "host `[nope]`"),
            (b"CONNECT http://github.com:443/ HTTP/1.1\r\n\r\n", "port `443/`"),
            (b"CONNECT github.com:443 HTTP/2\r\n\r\n", "version `HTTP/2`"),
            (b"CONNECT  github.com:443 HTTP/1.1\r\n\r\n", "request line"),
            (b"connect github.com:443 HTTP/1.1\r\n\r\n", "method `connect`"),
            (b"CONNECT github.com:443 HTTP/1.1\r\n folded\r\n\r\n", "folded"),
            (b"CONNECT github.com:443 HTTP/1.1\r\nNoColon\r\n\r\n", "header line"),
            (b"CONNECT github.com:443 HTTP/1.1\r\nBad Name: x\r\n\r\n", "header name"),
            (b"CONNECT github.com:443 HTTP/1.1\nHost: x\r\n\r\n", "request line"),
            (b"CONNECT github.com:443 HTTP/1.1\r\nA: b\nC: d\r\n\r\n", "bare CR or LF"),
            (b"CONNECT github.com:443 HTTP/1.1\r\nA: \x01\r\n\r\n", "value"),
            (b"CONNECT g\xc3\xa9.com:443 HTTP/1.1\r\n\r\n", "ASCII"),
            (b"GET / HTTP/1.1\r\nHost: a\r\n\r\n", "isn't a proxy request"),
            (b"GET https://a.io/ HTTP/1.1\r\n\r\n", "isn't a proxy request"),
            (b"GET http://u@a.io/ HTTP/1.1\r\n\r\n", "userinfo"),
            (b"GET http://a.io/#x HTTP/1.1\r\n\r\n", "fragment"),
            (b"GET http:///x HTTP/1.1\r\n\r\n", "host ``"),
        ] {
            let err = parse_request(head).unwrap_err();
            assert!(err.contains(want), "{:?}: {err}", String::from_utf8_lossy(head));
        }
    }

    #[test]
    fn absolute_form_requests_parse() {
        let head = b"POST HTTP://Example.COM:8080?q=1 HTTP/1.1\r\nHost: example.com:8080\r\nContent-Length: 2\r\n\r\n";
        let Request::Forward(fwd) = parse_request(head).unwrap() else { panic!() };
        assert_eq!((fwd.host.as_str(), fwd.port, fwd.origin.as_str()), ("example.com", 8080, "/?q=1"));
        assert_eq!(fwd.absolute, "HTTP://Example.COM:8080?q=1");
        let Request::Forward(fwd) = parse_request(b"GET http://a.io HTTP/1.0\r\n\r\n").unwrap() else { panic!() };
        assert_eq!((fwd.port, fwd.origin.as_str()), (80, "/"));
    }

    #[test]
    fn head_reading_is_bounded_and_leaves_the_rest() {
        let mut wire = io::Cursor::new(b"CONNECT a.io:443 HTTP/1.1\r\n\r\n\x16\x03\x01".to_vec());
        assert_eq!(read_head(&mut wire).unwrap(), b"CONNECT a.io:443 HTTP/1.1\r\n\r\n");
        let mut rest = Vec::new();
        wire.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, b"\x16\x03\x01");
        let long = format!("CONNECT a.io:443 HTTP/1.1\r\nX: {}\r\n\r\n", "a".repeat(MAX_HEAD));
        let err = read_head(&mut io::Cursor::new(long.into_bytes())).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let cut = read_head(&mut io::Cursor::new(b"CONNECT a.io:443 HTTP/1.1\r\n".to_vec())).unwrap_err();
        assert_eq!(cut.kind(), io::ErrorKind::UnexpectedEof);
    }

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn proxy_env_precedence_and_urls() {
        let up = Upstreams::from_env(&env(&[
            ("HTTPS_PROXY", "http://upper:1"),
            ("https_proxy", "http://lower:2"),
            ("ALL_PROXY", "http://all:3"),
            ("http_proxy", ""),
        ]));
        assert_eq!(up.via(true, "a.io", 443).unwrap().map(|p| p.host.as_str()), Some("lower"));
        assert_eq!(up.via(false, "a.io", 80).unwrap().map(|p| p.host.as_str()), Some("all"), "empty is unset");
        assert!(Upstreams::from_env(&[]).via(true, "a.io", 443).unwrap().is_none());

        let url = parse_proxy_url("HTTP://us%40er:p%3Ass@Proxy.corp:3128/").unwrap();
        assert_eq!(url, ProxyUrl { host: "proxy.corp".into(), port: 3128, auth: Some(format!("Basic {}", base64(b"us@er:p:ss"))) });
        assert_eq!(parse_proxy_url("proxy:8080").unwrap(), ProxyUrl { host: "proxy".into(), port: 8080, auth: None });
        assert_eq!(parse_proxy_url("http://[::1]").unwrap().port, 80);
        let socks = Upstreams::from_env(&env(&[("all_proxy", "socks5://u:secret@s:1080"), ("no_proxy", "local")]));
        let err = socks.via(true, "a.io", 443).unwrap_err();
        assert!(err.contains("socks5://") && !err.contains("secret"), "{err}");
        assert_eq!(socks.problems().count(), 2);
        assert!(socks.via(true, "local", 443).unwrap().is_none(), "no_proxy before the scheme check");
        assert!(parse_proxy_url("http://u:%zz@p:1").unwrap_err().contains("doesn't parse"));
        assert!(!parse_proxy_url("http://u:secret@p:x").unwrap_err().contains("secret"));
    }

    #[test]
    fn base64_vectors() {
        for (plain, enc) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("foobar", "Zm9vYmFy")] {
            assert_eq!(base64(plain.as_bytes()), enc);
        }
    }

    #[test]
    fn no_proxy_matching() {
        let np = "localhost, .corp.example,*.internal  10.0.0.0/8,fd00::/8 192.168.1.5 svc:8080 [::1]";
        for (host, port, want) in [
            ("localhost", 443, true),
            ("corp.example", 443, true),
            ("git.corp.example", 443, true),
            ("xcorp.example", 443, false),
            ("a.internal", 443, true),
            ("10.1.2.3", 443, true),
            ("11.1.2.3", 443, false),
            ("[fd00::5]", 443, true),
            ("[fe80::5]", 443, false),
            ("192.168.1.5", 22, true),
            ("svc", 8080, true),
            ("svc", 443, false),
            ("[::1]", 443, true),
            ("github.com", 443, false),
        ] {
            assert_eq!(bypass(np, host, port), want, "{host}:{port}");
        }
        assert!(bypass("*", "anything", 1));
        assert!(bypass("example.com.", "example.com", 1));
        assert!(!bypass("", "example.com", 1));
        assert!(bypass("0.0.0.0/0", "1.2.3.4", 1));
    }

    #[test]
    fn forward_heads_for_origin_and_proxy() {
        let head = b"GET http://a.io/x HTTP/1.1\r\nHost: a.io\r\nProxy-Connection: keep-alive\r\nProxy-Authorization: Basic old\r\nConnection: keep-alive\r\nKeep-Alive: 5\r\nAccept: */*\r\n\r\n";
        let Request::Forward(fwd) = parse_request(head).unwrap() else { panic!() };
        assert_eq!(
            String::from_utf8(forward_head(&fwd, None)).unwrap(),
            "GET /x HTTP/1.1\r\nHost: a.io\r\nAccept: */*\r\nConnection: close\r\n\r\n"
        );
        let proxy = ProxyUrl { host: "p".into(), port: 1, auth: Some("Basic new".into()) };
        assert_eq!(
            String::from_utf8(forward_head(&fwd, Some(&proxy))).unwrap(),
            "GET http://a.io/x HTTP/1.1\r\nHost: a.io\r\nAccept: */*\r\nProxy-Authorization: Basic new\r\nConnection: close\r\n\r\n"
        );
        let ws = b"GET http://a.io/ws HTTP/1.1\r\nHost: a.io\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n";
        let Request::Forward(fwd) = parse_request(ws).unwrap() else { panic!() };
        assert_eq!(
            String::from_utf8(forward_head(&fwd, None)).unwrap(),
            "GET /ws HTTP/1.1\r\nHost: a.io\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n"
        );
    }

    /// A proxy port on an ephemeral port binding `hosts`.
    pub(crate) fn proxy_port(hosts: &[&str], env: &[(String, String)], route: Route) -> u16 {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let cfg = Arc::new(ProxyPort {
            hosts: hosts.iter().map(|h| h.to_string()).collect(),
            upstreams: Arc::new(Upstreams::from_env(env)),
        });
        let ids = Arc::new(AtomicU32::new(1));
        std::thread::spawn(move || serve(listener, &cfg, &route, &ids));
        port
    }

    fn no_route() -> Route {
        Arc::new(|| None)
    }

    /// A server answering each connection's bytes upper-cased once it
    /// half-closes.
    fn echo_server() -> u16 {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let mut conn = conn.unwrap();
                std::thread::spawn(move || {
                    let mut got = Vec::new();
                    conn.read_to_end(&mut got).unwrap();
                    conn.write_all(&got.to_ascii_uppercase()).unwrap();
                });
            }
        });
        port
    }

    /// A server that reports each connection's head, answers it with
    /// `answer`, then echoes the rest upper-cased once it half-closes.
    fn head_server(answer: &'static [u8]) -> (u16, mpsc::Receiver<String>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let mut conn = conn.unwrap();
                let tx = tx.clone();
                std::thread::spawn(move || {
                    let head = read_head(&mut conn).unwrap();
                    tx.send(String::from_utf8(head).unwrap()).unwrap();
                    conn.write_all(answer).unwrap();
                    let mut rest = Vec::new();
                    let _ = conn.read_to_end(&mut rest);
                    let _ = conn.write_all(&rest.to_ascii_uppercase());
                });
            }
        });
        (port, rx)
    }

    fn closed_port() -> u16 {
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap().local_addr().unwrap().port()
    }

    /// Send `head` then `body` (half-closing), and read everything back.
    fn exchange(port: u16, head: &str, body: &[u8]) -> String {
        let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        client.write_all(head.as_bytes()).unwrap();
        if head.starts_with("CONNECT") {
            let mut answer = [0u8; 12];
            client.read_exact(&mut answer).unwrap();
            if &answer != b"HTTP/1.1 200" {
                let mut rest = String::new();
                let _ = client.read_to_string(&mut rest);
                return format!("{}{rest}", String::from_utf8_lossy(&answer));
            }
            let mut rest = vec![0u8; ESTABLISHED.len() - 12];
            client.read_exact(&mut rest).unwrap();
        }
        // The proxy may have answered and closed already.
        let _ = client.write_all(body);
        let _ = client.shutdown(Shutdown::Write);
        let mut answer = String::new();
        client.read_to_string(&mut answer).unwrap();
        answer
    }

    #[test]
    fn unbound_targets_are_dialed_directly() {
        let echo = echo_server();
        // A bound host on another port is dialed too: only :443 goes to the broker.
        let port = proxy_port(&["localhost"], &[], no_route());
        assert_eq!(exchange(port, &format!("CONNECT 127.0.0.1:{echo} HTTP/1.1\r\n\r\n"), b"hi"), "HI");
        assert_eq!(exchange(port, &format!("CONNECT LocalHost.:{echo} HTTP/1.1\r\n\r\n"), b"hi"), "HI");
    }

    #[test]
    fn unbound_targets_go_through_the_containers_proxy() {
        let (upstream, heads) = head_server(b"HTTP/1.1 200 OK\r\n\r\n");
        let env = env(&[("HTTPS_PROXY", &format!("http://us%40er:pw@127.0.0.1:{upstream}"))]);
        let port = proxy_port(&["github.com"], &env, no_route());
        assert_eq!(exchange(port, "CONNECT Example.test:8443 HTTP/1.1\r\nHost: x\r\n\r\n", b"tls"), "TLS");
        let head = heads.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(
            head,
            format!("CONNECT example.test:8443 HTTP/1.1\r\nHost: example.test:8443\r\nProxy-Authorization: Basic {}\r\n\r\n", base64(b"us@er:pw"))
        );
    }

    #[test]
    fn no_proxy_targets_skip_the_containers_proxy() {
        let echo = echo_server();
        let env = env(&[("https_proxy", &format!("http://127.0.0.1:{}", closed_port())), ("no_proxy", "localhost,127.0.0.1")]);
        let port = proxy_port(&[], &env, no_route());
        assert_eq!(exchange(port, &format!("CONNECT 127.0.0.1:{echo} HTTP/1.1\r\n\r\n"), b"hi"), "HI");
        let refused = exchange(port, "CONNECT elsewhere.test:443 HTTP/1.1\r\n\r\n", b"");
        assert!(refused.starts_with("HTTP/1.1 502 Bad Gateway\r\n") && refused.contains("dialing the container's proxy"), "{refused}");
    }

    #[test]
    fn absolute_form_requests_are_forwarded() {
        let (origin, heads) = head_server(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
        let port = proxy_port(&["127.0.0.1"], &[], no_route());
        let head = format!("POST http://127.0.0.1:{origin}/x?y=1 HTTP/1.1\r\nHost: 127.0.0.1:{origin}\r\nProxy-Connection: keep-alive\r\nContent-Length: 4\r\n\r\n");
        let answer = exchange(port, &head, b"body");
        assert_eq!(answer, "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nokBODY");
        assert_eq!(
            heads.recv_timeout(Duration::from_secs(5)).unwrap(),
            format!("POST /x?y=1 HTTP/1.1\r\nHost: 127.0.0.1:{origin}\r\nContent-Length: 4\r\nConnection: close\r\n\r\n")
        );

        let (upstream, heads) = head_server(b"HTTP/1.1 204 No Content\r\n\r\n");
        let env = env(&[("http_proxy", &format!("http://u:p@127.0.0.1:{upstream}"))]);
        let port = proxy_port(&[], &env, no_route());
        let answer = exchange(port, "GET http://example.test/ HTTP/1.1\r\nHost: example.test\r\n\r\n", b"");
        assert_eq!(answer, "HTTP/1.1 204 No Content\r\n\r\n");
        assert_eq!(
            heads.recv_timeout(Duration::from_secs(5)).unwrap(),
            format!("GET http://example.test/ HTTP/1.1\r\nHost: example.test\r\nProxy-Authorization: Basic {}\r\nConnection: close\r\n\r\n", base64(b"u:p"))
        );
    }

    #[test]
    fn failed_dials_get_a_502() {
        let port = proxy_port(&[], &[], no_route());
        let dead = closed_port();
        let answer = exchange(port, &format!("CONNECT 127.0.0.1:{dead} HTTP/1.1\r\n\r\n"), b"");
        assert!(answer.starts_with("HTTP/1.1 502 Bad Gateway\r\n") && answer.contains(&format!("dialing 127.0.0.1:{dead}")), "{answer}");
        let answer = exchange(port, &format!("GET http://127.0.0.1:{dead}/ HTTP/1.1\r\n\r\n"), b"");
        assert!(answer.starts_with("HTTP/1.1 502 Bad Gateway\r\n"), "{answer}");

        let (upstream, _heads) = head_server(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n");
        let env = env(&[("HTTPS_PROXY", &format!("http://127.0.0.1:{upstream}"))]);
        let port = proxy_port(&[], &env, no_route());
        let answer = exchange(port, "CONNECT a.test:443 HTTP/1.1\r\n\r\n", b"");
        assert!(answer.contains("the container's proxy answered 403 to CONNECT a.test:443"), "{answer}");
    }

    #[test]
    fn a_bound_host_with_no_route_gets_a_503_and_bad_heads_a_400() {
        let port = proxy_port(&["github.com"], &[], no_route());
        let answer = exchange(port, "CONNECT GITHUB.com:443 HTTP/1.1\r\n\r\n", b"");
        assert!(answer.starts_with("HTTP/1.1 503 Service Unavailable\r\n"), "{answer}");
        let answer = exchange(port, "GET / HTTP/1.1\r\nHost: github.com\r\n\r\n", b"");
        assert!(answer.starts_with("HTTP/1.1 400 Bad Request\r\n") && answer.contains("isn't a proxy request"), "{answer}");
        let oversize = format!("CONNECT a.io:443 HTTP/1.1\r\nX: {}", "a".repeat(MAX_HEAD));
        let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        client.write_all(oversize.as_bytes()).unwrap();
        let mut answer = String::new();
        client.read_to_string(&mut answer).unwrap();
        assert!(answer.starts_with("HTTP/1.1 400 Bad Request\r\n"), "{answer}");
    }
}
