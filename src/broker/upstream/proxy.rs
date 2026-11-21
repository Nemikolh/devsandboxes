//! The host's proxy env for upstream calls. The proxy is dialed as
//! configured (exempt from the address check: it is the host's own, often
//! on loopback or the LAN); the target is resolved and checked here and
//! the tunnel opened to the checked address, never to the name.

use std::io::{self, BufReader, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};

use super::{Timeouts, UpstreamError, dial};
use crate::broker::head::{self, MAX_HEADERS};
use crate::broker::redact;

/// curl's default when a proxy URL has no port.
const DEFAULT_PORT: u16 = 1080;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proxy {
    host: String,
    port: u16,
    /// A `Proxy-Authorization` value, from the URL's userinfo.
    auth: Option<Vec<u8>>,
    /// `NO_PROXY` hosts, as `no_proxy_entry` leaves them.
    no_proxy: Vec<String>,
}

impl Proxy {
    #[cfg_attr(test, allow(dead_code))] // Tests read `from_vars`.
    pub fn from_env() -> Result<Option<Proxy>, UpstreamError> {
        Proxy::from_vars(|k| std::env::var(k).ok())
    }

    /// `https_proxy`, else `HTTPS_PROXY`, `all_proxy`, `ALL_PROXY` (the
    /// upstream is always https, so `HTTP_PROXY` doesn't apply); `no_proxy`
    /// or `NO_PROXY`. Empty values count as unset.
    pub fn from_vars(var: impl Fn(&str) -> Option<String>) -> Result<Option<Proxy>, UpstreamError> {
        let first = |names: &[&str]| names.iter().find_map(|n| var(n).filter(|v| !v.trim().is_empty()));
        let Some(url) = first(&["https_proxy", "HTTPS_PROXY", "all_proxy", "ALL_PROXY"]) else {
            return Ok(None);
        };
        let no_proxy = first(&["no_proxy", "NO_PROXY"]).unwrap_or_default();
        Proxy::parse(url.trim(), &no_proxy).map(Some)
    }

    /// `[http://][user[:password]@]host[:port][/]`: only plain-http proxies
    /// (an `https://` or SOCKS proxy is refused rather than bypassed).
    pub fn parse(url: &str, no_proxy: &str) -> Result<Proxy, UpstreamError> {
        let bad = || UpstreamError::Invalid(format!("proxy URL {url:?} (only http:// proxies)"));
        let rest = match url.split_once("://") {
            Some((scheme, rest)) if scheme.eq_ignore_ascii_case("http") => rest,
            Some(_) => return Err(bad()),
            None => url,
        };
        let rest = rest.strip_suffix('/').unwrap_or(rest);
        if rest.bytes().any(|b| b <= b' ' || b >= 0x7f || matches!(b, b'/' | b'?' | b'#')) {
            return Err(bad());
        }
        let (userinfo, hostport) = match rest.rsplit_once('@') {
            Some((u, h)) => (Some(u), h),
            None => (None, rest),
        };
        let (host, port) = match hostport.strip_prefix('[') {
            Some(v6) => {
                let (h, after) = v6.split_once(']').ok_or_else(bad)?;
                (h, if after.is_empty() { None } else { Some(after.strip_prefix(':').ok_or_else(bad)?) })
            }
            None => match hostport.split_once(':') {
                Some((h, p)) => (h, Some(p)),
                None => (hostport, None),
            },
        };
        let port = match port {
            None => DEFAULT_PORT,
            Some(p) => p.parse::<u16>().ok().filter(|&p| p != 0).ok_or_else(bad)?,
        };
        if host.is_empty() {
            return Err(bad());
        }
        let auth = match userinfo {
            None => None,
            Some(u) => {
                let (user, password) = u.split_once(':').unwrap_or((u, ""));
                let (user, password) = (percent_decode(user).ok_or_else(bad)?, percent_decode(password).ok_or_else(bad)?);
                let mut value = b"Basic ".to_vec();
                value.extend(redact::basic_auth(&user, &password));
                Some(value)
            }
        };
        let no_proxy = no_proxy.split(',').filter_map(no_proxy_entry).collect();
        Ok(Proxy {
            host: host.to_string(),
            port,
            auth,
            no_proxy,
        })
    }

    /// `NO_PROXY` as curl reads it, minus CIDR ranges: `*`, a host, or a
    /// domain covering its subdomains, an optional `:port` ignored.
    pub fn bypass(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        let host = host.trim_end_matches('.');
        self.no_proxy.iter().any(|entry| {
            entry == "*"
                || host == entry
                || host.strip_suffix(entry.as_str()).is_some_and(|prefix| prefix.ends_with('.'))
        })
    }

    /// A tunnel through the proxy to `target`, ready for TLS.
    pub(super) fn tunnel(&self, target: SocketAddr, timeouts: &Timeouts) -> io::Result<TcpStream> {
        let addrs: Vec<SocketAddr> = (self.host.as_str(), self.port).to_socket_addrs()?.collect();
        let mut tcp = dial(&addrs, timeouts)?;
        let mut req = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n").into_bytes();
        if let Some(auth) = &self.auth {
            req.extend_from_slice(b"Proxy-Authorization: ");
            req.extend_from_slice(auth);
            req.extend_from_slice(b"\r\n");
        }
        req.extend_from_slice(b"\r\n");
        tcp.write_all(&req)?;
        tcp.flush()?;
        // One byte at a time: nothing past the head may be buffered away
        // from the TLS that follows.
        let raw = head::read_raw_head(&mut BufReader::with_capacity(1, &mut tcp))
            .map_err(|e| super::bad_response(e, "proxy"))?
            .ok_or_else(|| io::Error::from(UpstreamError::BadResponse("proxy closed before answering CONNECT".into())))?;
        let mut slots = [httparse::EMPTY_HEADER; MAX_HEADERS];
        let mut resp = httparse::Response::new(&mut slots);
        match resp.parse(&raw) {
            Ok(httparse::Status::Complete(_)) => {}
            _ => return Err(UpstreamError::BadResponse("malformed CONNECT answer from the proxy".into()).into()),
        }
        match resp.code {
            Some(code) if (200..300).contains(&code) => Ok(tcp),
            code => Err(UpstreamError::Proxy(code.unwrap_or(0)).into()),
        }
    }
}

/// Lowercased, leading `*.` / `.`, trailing `.` and any `:port` dropped,
/// an IPv6 literal unbracketed.
fn no_proxy_entry(entry: &str) -> Option<String> {
    let entry = entry.trim().to_ascii_lowercase();
    let host = match entry.strip_prefix('[') {
        Some(v6) => v6.split_once(']').map_or(v6, |(h, _)| h),
        None => match entry.split_once(':') {
            Some((h, p)) if !p.contains(':') => h,
            _ => &entry,
        },
    };
    let host = host.trim_start_matches("*.").trim_start_matches('.').trim_end_matches('.');
    (!host.is_empty()).then(|| host.to_string())
}

fn percent_decode(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars<'a>(set: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| set.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
    }

    #[test]
    fn from_vars_precedence() {
        assert_eq!(Proxy::from_vars(vars(&[])).unwrap(), None);
        assert_eq!(Proxy::from_vars(vars(&[("HTTP_PROXY", "http://p:1")])).unwrap(), None);
        let p = Proxy::from_vars(vars(&[("HTTPS_PROXY", "http://upper:1"), ("https_proxy", "http://lower:2")]))
            .unwrap()
            .unwrap();
        assert_eq!((p.host.as_str(), p.port), ("lower", 2));
        let p = Proxy::from_vars(vars(&[("https_proxy", " "), ("ALL_PROXY", "all:3"), ("NO_PROXY", "a.com")]))
            .unwrap()
            .unwrap();
        assert_eq!((p.host.as_str(), p.port), ("all", 3));
        assert_eq!(p.no_proxy, vec!["a.com".to_string()]);
    }

    #[test]
    fn parse() {
        let p = Proxy::parse("http://proxy.corp:3128/", "").unwrap();
        assert_eq!((p.host.as_str(), p.port, p.auth.clone()), ("proxy.corp", 3128, None));
        let p = Proxy::parse("proxy.corp", "").unwrap();
        assert_eq!(p.port, DEFAULT_PORT);
        let p = Proxy::parse("HTTP://[::1]:8080", "").unwrap();
        assert_eq!((p.host.as_str(), p.port), ("::1", 8080));
        let p = Proxy::parse("http://al%40ice:p%3Aw@p:1", "").unwrap();
        let mut want = b"Basic ".to_vec();
        want.extend(redact::basic_auth(b"al@ice", b"p:w"));
        assert_eq!(p.auth, Some(want));
        for bad in [
            "https://p:1",
            "socks5://p:1",
            "http://p:x",
            "http://p:0",
            "http://:1",
            "http://p:1/path",
            "http://u:%zz@p:1",
            "http://p 1",
            "http://[::1",
        ] {
            assert!(Proxy::parse(bad, "").is_err(), "{bad}");
        }
    }

    #[test]
    fn bypass() {
        let p = Proxy::parse("p:1", " Example.com, .corp.internal,*.svc,10.0.0.1, host:8080 ,[::1]:3,").unwrap();
        for host in ["example.com", "api.example.com", "EXAMPLE.com.", "x.corp.internal", "corp.internal", "a.svc", "10.0.0.1", "host", "::1"] {
            assert!(p.bypass(host), "{host}");
        }
        for host in ["notexample.com", "example.com.evil", "svc.io", "10.0.0.10", "hostx"] {
            assert!(!p.bypass(host), "{host}");
        }
        assert!(Proxy::parse("p:1", "*").unwrap().bypass("anything"));
        assert!(!Proxy::parse("p:1", "").unwrap().bypass("anything"));
    }
}
