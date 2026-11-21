//! The upstream URL: `https://host[:port][/path][?query]` only. No userinfo
//! or fragment, and nothing a request line or `Host` field could be split
//! with.

use std::net::Ipv6Addr;

use super::UpstreamError;

pub const DEFAULT_PORT: u16 = 443;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Lowercased; an IPv6 literal without its brackets.
    pub host: String,
    pub port: u16,
    /// Origin-form: starts with `/`, query included.
    pub path: String,
}

impl Target {
    pub fn parse(url: &str) -> Result<Target, UpstreamError> {
        let bad = || UpstreamError::Invalid(format!("upstream URL {url:?}"));
        let rest = url
            .get(..8)
            .filter(|s| s.eq_ignore_ascii_case("https://"))
            .map(|_| &url[8..])
            .ok_or_else(bad)?;
        if rest.bytes().any(|b| b <= b' ' || b >= 0x7f || b == b'#') {
            return Err(bad());
        }
        let split = rest.find(['/', '?']).unwrap_or(rest.len());
        let (authority, path) = rest.split_at(split);
        let path = match path.as_bytes().first() {
            None => "/".to_string(),
            Some(b'?') => format!("/{path}"),
            Some(_) => path.to_string(),
        };
        if authority.contains('@') {
            return Err(bad());
        }
        let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
            let (addr, after) = v6.split_once(']').ok_or_else(bad)?;
            addr.parse::<Ipv6Addr>().map_err(|_| bad())?;
            let port = match after {
                "" => None,
                a => Some(a.strip_prefix(':').ok_or_else(bad)?),
            };
            (addr, port)
        } else {
            match authority.split_once(':') {
                Some((h, p)) => (h, Some(p)),
                None => (authority, None),
            }
        };
        let port = match port {
            None => DEFAULT_PORT,
            Some(p) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => {
                p.parse::<u16>().ok().filter(|&p| p != 0).ok_or_else(bad)?
            }
            Some(_) => return Err(bad()),
        };
        if !authority.starts_with('[')
            && (host.is_empty()
                || !host.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_')))
        {
            return Err(bad());
        }
        Ok(Target {
            host: host.to_ascii_lowercase(),
            port,
            path,
        })
    }

    /// The `Host` field: brackets around an IPv6 literal, the port only when
    /// it isn't 443.
    pub fn authority(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        if self.port == DEFAULT_PORT {
            host
        } else {
            format!("{host}:{}", self.port)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(host: &str, port: u16, path: &str) -> Target {
        Target {
            host: host.into(),
            port,
            path: path.into(),
        }
    }

    #[test]
    fn parses() {
        assert_eq!(Target::parse("https://sentry.io").unwrap(), t("sentry.io", 443, "/"));
        assert_eq!(
            Target::parse("HTTPS://API.GitHub.com/repos/x?per_page=1").unwrap(),
            t("api.github.com", 443, "/repos/x?per_page=1")
        );
        assert_eq!(Target::parse("https://h:8443?q").unwrap(), t("h", 8443, "/?q"));
        assert_eq!(Target::parse("https://[2001:db8::1]:444/p").unwrap(), t("2001:db8::1", 444, "/p"));
        assert_eq!(Target::parse("https://[::1]/").unwrap(), t("::1", 443, "/"));
        assert_eq!(Target::parse("https://10.0.0.1/").unwrap(), t("10.0.0.1", 443, "/"));
    }

    #[test]
    fn refuses() {
        for url in [
            "http://h/",
            "h/",
            "https://",
            "https:///p",
            "https://u:p@h/",
            "https://h/#frag",
            "https://h/a b",
            "https://h/a\r\nX: y",
            "https://h:/",
            "https://h:0/",
            "https://h:65536/",
            "https://h:+1/",
            "https://h:1:2/",
            "https://[::1/",
            "https://[nope]/",
            "https://[::1]x/",
            "https://h\u{e9}/",
            "https://h%2f/",
        ] {
            assert!(Target::parse(url).is_err(), "{url:?}");
        }
    }

    #[test]
    fn authority() {
        assert_eq!(t("h", 443, "/").authority(), "h");
        assert_eq!(t("h", 8443, "/").authority(), "h:8443");
        assert_eq!(t("::1", 443, "/").authority(), "[::1]");
        assert_eq!(t("::1", 1, "/").authority(), "[::1]:1");
    }
}
