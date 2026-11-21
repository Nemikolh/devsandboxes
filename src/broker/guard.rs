//! What keeps a browser in the sandbox from driving the broker: a page can
//! make the browser send requests to a loopback listener, but it can't
//! drop `Origin` / `Sec-Fetch-Site` nor (past DNS rebinding) send a `Host`
//! naming the listener, so requests carrying either, or a `Host` other
//! than the expected authority, are refused before policy.

use super::HttpError;
use super::head::{Header, RequestHead, Target, header, header_count};

/// Whatever their value (`Sec-Fetch-Site: none` included: no tool sends
/// either).
pub fn refuse_browser(headers: &[Header]) -> Result<(), HttpError> {
    if header_count(headers, "origin") + header_count(headers, "sec-fetch-site") > 0 {
        return Err(HttpError::Browser);
    }
    Ok(())
}

/// `expected`: the listener's `127.0.0.1:<port>` in base-URL mode
/// (`default_port` 80), the `CONNECT` target in intercept mode (443). No
/// `Host` is refused too, HTTP/1.0 included.
pub fn check_host(head: &RequestHead, expected: &str, default_port: u16) -> Result<(), HttpError> {
    if header_count(&head.headers, "host") != 1 {
        return Err(HttpError::Host);
    }
    let host = header(&head.headers, "host")
        .and_then(|v| std::str::from_utf8(v).ok())
        .ok_or(HttpError::Host)?;
    let target_ok = match &head.target {
        Target::Origin(_) => true,
        Target::Absolute { authority, .. } => same_authority(authority, expected, default_port),
    };
    if target_ok && same_authority(host, expected, default_port) {
        Ok(())
    } else {
        Err(HttpError::Host)
    }
}

/// Host case-folded with one trailing dot dropped, a missing or empty port
/// read as `default_port`. An authority that isn't a plain host (userinfo,
/// percent-encoding, an unbracketed IPv6 address, …) matches nothing. Also
/// what the intercept TLS layer compares SNI with the `CONNECT` target by.
pub fn same_authority(a: &str, b: &str, default_port: u16) -> bool {
    match (normalize(a, default_port), normalize(b, default_port)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

fn normalize(authority: &str, default_port: u16) -> Option<(String, u16)> {
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let (ip, port) = rest.split_once(']')?;
        let port = match port {
            "" => "",
            p => p.strip_prefix(':')?,
        };
        if ip.is_empty() || !ip.chars().all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.') {
            return None;
        }
        (format!("[{}]", ip.to_ascii_lowercase()), port)
    } else {
        let (host, port) = authority.rsplit_once(':').unwrap_or((authority, ""));
        let host = host.strip_suffix('.').unwrap_or(host);
        if host.is_empty()
            || !host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'))
        {
            return None;
        }
        (host.to_ascii_lowercase(), port)
    };
    let port = match port {
        "" => default_port,
        p if p.chars().all(|c| c.is_ascii_digit()) => p.parse().ok()?,
        _ => return None,
    };
    Some((host, port))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broker::head::parse_head;

    fn head(s: &str) -> RequestHead {
        parse_head(s.as_bytes()).unwrap()
    }

    #[test]
    fn browser_headers_refused() {
        let ok = head("GET / HTTP/1.1\r\nHost: h\r\nUser-Agent: curl\r\n\r\n");
        assert_eq!(refuse_browser(&ok.headers), Ok(()));
        for h in ["Origin: null", "origin: http://127.0.0.1:3000", "Sec-Fetch-Site: none", "SEC-FETCH-SITE: cross-site"] {
            let r = head(&format!("GET / HTTP/1.1\r\nHost: h\r\n{h}\r\n\r\n"));
            assert_eq!(refuse_browser(&r.headers), Err(HttpError::Browser), "{h}");
        }
    }

    #[test]
    fn host_base_url() {
        let check = |host: &str| {
            check_host(&head(&format!("GET / HTTP/1.1\r\nHost: {host}\r\n\r\n")), "127.0.0.1:41000", 80)
        };
        assert_eq!(check("127.0.0.1:41000"), Ok(()));
        for bad in ["127.0.0.1", "localhost:41000", "127.0.0.1:41001", "evil.example:41000", "127.0.0.1:41000@x"] {
            assert_eq!(check(bad), Err(HttpError::Host), "{bad}");
        }
    }

    #[test]
    fn host_intercept() {
        let check = |host: &str| {
            check_host(&head(&format!("GET / HTTP/1.1\r\nHost: {host}\r\n\r\n")), "api.github.com:443", 443)
        };
        for ok in ["api.github.com", "API.GitHub.com:443", "api.github.com.", "api.github.com.:443", "api.github.com:"] {
            assert_eq!(check(ok), Ok(()), "{ok}");
        }
        for bad in ["api.github.com:80", "uploads.github.com", "api.github.com..", "api%2egithub.com", "api.github.com:0443x"] {
            assert_eq!(check(bad), Err(HttpError::Host), "{bad}");
        }
    }

    #[test]
    fn host_count() {
        let none = head("GET / HTTP/1.0\r\n\r\n");
        assert_eq!(check_host(&none, "h:443", 443), Err(HttpError::Host));
        let two = head("GET / HTTP/1.1\r\nHost: h\r\nHost: h\r\n\r\n");
        assert_eq!(check_host(&two, "h:443", 443), Err(HttpError::Host));
    }

    #[test]
    fn host_absolute_form_checks_the_target_too() {
        let ok = head("GET https://h/x HTTP/1.1\r\nHost: h\r\n\r\n");
        assert_eq!(check_host(&ok, "h:443", 443), Ok(()));
        let bad = head("GET https://other/x HTTP/1.1\r\nHost: h\r\n\r\n");
        assert_eq!(check_host(&bad, "h:443", 443), Err(HttpError::Host));
    }

    #[test]
    fn authorities() {
        // Textual: no address canonicalization beyond case.
        assert!(!same_authority("[::1]:8080", "[0:0::1]:8080", 80));
        assert!(same_authority("[::1]", "[::1]:80", 80));
        assert!(same_authority("[FE80::1]:443", "[fe80::1]", 443));
        assert!(!same_authority("::1", "::1", 80));
        assert!(!same_authority("[::1]x", "[::1]", 80));
        assert!(!same_authority("", "", 80));
        assert!(!same_authority("h:99999", "h:99999", 80));
        assert!(same_authority("h:0443", "h", 443));
    }
}
