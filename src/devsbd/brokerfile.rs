//! The broker file: the credential-broker listeners and phantom files the
//! helper's daemon sets up in the container (docs/architecture/helper/in-container.md,
//! *Broker listeners*). Written by the host, read by `devsbd daemon` at
//! every start; one file shared by both crates (devsbd includes it via
//! `#[path]`) so the two sides can't drift. std-only and hand-parsed: the
//! helper has no dependencies.
//!
//! Format, line-based UTF-8, one directive per line, `<key> <value>`:
//!
//! ```text
//! listen 41001 base-url sentry.io        a loopback listener: port, mode, upstream
//! proxy-listen 41000 api.github.com github.com
//!                                        a loopback proxy port: port, the hosts it
//!                                        hands the broker (intercept)
//! file vscode /home/vscode/.sentryclirc\0sntrys_x\0[auth]\ntoken=sntrys_x\n
//!                                        a phantom file: owner, path, phantom, content
//! proxy HTTPS_PROXY=http://proxy:3128    the container's own proxy env, in order
//! ```
//!
//! `file`'s owner is `name|uid[:group|gid]` (no spaces), resolved in the
//! container at write time; then, split on NUL, the absolute path, the
//! phantom the content holds, and the content. Values are escaped
//! (`escape.rs`). `proxy-listen` may name no host (a proxy that dials
//! every target itself). Anything else (an unknown key, a bad escape, a
//! port twice across `listen` and `proxy-listen`, a host twice on one
//! proxy port, a file twice, a relative path or one with `.`/`..`, content
//! without its phantom) is a parse error: host and helper ship together.

use super::escape::{escape, unescape};

/// Where the host writes the broker file (0644).
pub const PATH: &str = "/run/devsandbox/broker";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BrokerSpec {
    pub listeners: Vec<Listener>,
    /// The shared proxy port and git's own, each with its own bound hosts.
    pub proxy_listeners: Vec<ProxyListener>,
    pub files: Vec<PhantomFile>,
    /// The proxy the container had: how a proxy port dials what it doesn't
    /// hand the broker.
    pub proxy_env: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyListener {
    /// On `127.0.0.1`.
    pub port: u16,
    /// Bare lowercase hostnames whose `CONNECT host:443` goes to the broker.
    pub hosts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listener {
    /// On `127.0.0.1`.
    pub port: u16,
    pub mode: Mode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// Plain HTTP standing for `upstream` (a bare lowercase hostname).
    BaseUrl { upstream: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhantomFile {
    /// `name|uid[:group|gid]`: the file is written as this user.
    pub owner: String,
    /// Absolute.
    pub path: String,
    /// What makes an existing file at `path` ours to overwrite.
    pub phantom: String,
    pub content: String,
}

pub fn serialize(spec: &BrokerSpec) -> String {
    let mut out = String::new();
    for l in &spec.listeners {
        match &l.mode {
            Mode::BaseUrl { upstream } => {
                out.push_str(&format!("listen {} base-url ", l.port));
                escape(upstream, &mut out);
            }
        }
        out.push('\n');
    }
    for l in &spec.proxy_listeners {
        out.push_str(&format!("proxy-listen {}", l.port));
        for host in &l.hosts {
            out.push(' ');
            escape(host, &mut out);
        }
        out.push('\n');
    }
    for f in &spec.files {
        out.push_str("file ");
        escape(&f.owner, &mut out);
        out.push(' ');
        for (i, part) in [&f.path, &f.phantom, &f.content].into_iter().enumerate() {
            if i > 0 {
                out.push('\0');
            }
            escape(part, &mut out);
        }
        out.push('\n');
    }
    for (key, value) in &spec.proxy_env {
        out.push_str("proxy ");
        escape(key, &mut out);
        out.push('=');
        escape(value, &mut out);
        out.push('\n');
    }
    out
}

pub fn parse(text: &str) -> Result<BrokerSpec, String> {
    let mut spec = BrokerSpec::default();
    for (n, line) in text.split('\n').enumerate() {
        if line.is_empty() {
            continue;
        }
        let at = |e: String| format!("line {}: {e}", n + 1);
        let (key, rest) = line.split_once(' ').unwrap_or((line, ""));
        let taken = |spec: &BrokerSpec, port: u16| {
            spec.listeners.iter().any(|l| l.port == port) || spec.proxy_listeners.iter().any(|l| l.port == port)
        };
        match key {
            "listen" => {
                let listener = parse_listener(rest).map_err(at)?;
                if taken(&spec, listener.port) {
                    return Err(at(format!("port {} listed twice", listener.port)));
                }
                spec.listeners.push(listener);
            }
            "proxy-listen" => {
                let listener = parse_proxy_listener(rest).map_err(at)?;
                if taken(&spec, listener.port) {
                    return Err(at(format!("port {} listed twice", listener.port)));
                }
                spec.proxy_listeners.push(listener);
            }
            "file" => {
                let file = parse_file(rest).map_err(at)?;
                if spec.files.iter().any(|f| f.path == file.path) {
                    return Err(at(format!("`{}` listed twice", file.path)));
                }
                spec.files.push(file);
            }
            "proxy" => {
                // Split before unescaping: only `\\`, `\n`, `\0` are escapes,
                // so the first raw `=` ends the key.
                let (k, v) = rest.split_once('=').ok_or_else(|| at("proxy without `=`".into()))?;
                let k = unescape(k).map_err(at)?;
                if k.is_empty() || k.contains(['\0', '\n']) {
                    return Err(at(format!("proxy variable `{k}`")));
                }
                spec.proxy_env.push((k, unescape(v).map_err(at)?));
            }
            other => return Err(at(format!("unknown key `{other}`"))),
        }
    }
    Ok(spec)
}

fn parse_listener(rest: &str) -> Result<Listener, String> {
    let fields: Vec<&str> = rest.split(' ').collect();
    let [port, mode, upstream] = fields[..] else {
        return Err(format!("listen `{rest}`: want `<port> base-url <host>`"));
    };
    let port = parse_port(port).ok_or_else(|| format!("listen port `{port}`"))?;
    if mode != "base-url" {
        return Err(format!("listen mode `{mode}`"));
    }
    if !is_hostname(upstream) {
        return Err(format!("listen upstream `{upstream}` isn't a lowercase hostname"));
    }
    Ok(Listener { port, mode: Mode::BaseUrl { upstream: upstream.to_string() } })
}

fn parse_port(port: &str) -> Option<u16> {
    match port.parse::<u16>() {
        Ok(p) if p != 0 && port.bytes().all(|b| b.is_ascii_digit()) && !port.starts_with('0') => Some(p),
        _ => None,
    }
}

fn parse_proxy_listener(rest: &str) -> Result<ProxyListener, String> {
    let mut fields = rest.split(' ');
    let port = fields.next().unwrap_or_default();
    let port = parse_port(port).ok_or_else(|| format!("proxy-listen port `{port}`"))?;
    let mut hosts: Vec<String> = Vec::new();
    for host in fields {
        if !is_hostname(host) {
            return Err(format!("proxy-listen host `{host}` isn't a lowercase hostname"));
        }
        if hosts.iter().any(|h| h == host) {
            return Err(format!("proxy-listen host `{host}` listed twice"));
        }
        hosts.push(host.to_string());
    }
    Ok(ProxyListener { port, hosts })
}

/// The claim's own rule for upstreams (`credentials::claim`), restated
/// std-only: dot-separated labels of lowercase letters, digits and inner
/// `-`, no trailing dot.
fn is_hostname(host: &str) -> bool {
    host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

fn parse_file(rest: &str) -> Result<PhantomFile, String> {
    let (owner, rest) = rest.split_once(' ').ok_or("file without an owner")?;
    let owner = unescape(owner)?;
    if owner.is_empty() || owner.chars().any(|c| c.is_whitespace() || c == '\0') {
        return Err(format!("file owner `{owner}`"));
    }
    let parts: Vec<String> = rest.split('\0').map(unescape).collect::<Result<_, _>>()?;
    let Ok([path, phantom, content]) = <[String; 3]>::try_from(parts) else {
        return Err("file wants a path, a phantom and the content".into());
    };
    let plain = path.starts_with('/')
        && !path.contains('\0')
        && path.split('/').skip(1).all(|c| !c.is_empty() && c != "." && c != "..");
    if !plain {
        return Err(format!("file path `{path}` isn't absolute and plain"));
    }
    if phantom.is_empty() || !content.contains(&phantom) {
        return Err(format!("`{path}`: the content must hold its phantom"));
    }
    Ok(PhantomFile { owner, path, phantom, content })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Checked from both crates (this module is compiled into each), so the
    /// host's serializer and the helper's parser agree on the exact bytes.
    const FIXTURE: &str = "listen 41001 base-url sentry.io\n\
listen 41002 base-url us.sentry.io\n\
proxy-listen 41000 api.github.com github.com\n\
proxy-listen 41003 github.com\n\
proxy-listen 41004\n\
file vscode /home/vscode/.sentryclirc\0sntrys_x\0[auth]\\ntoken=sntrys_x\\n\n\
file 1000:1000 /home/u/a b/c\\\\d\0p\0p\n\
proxy HTTPS_PROXY=http://proxy:3128\n\
proxy NO_PROXY=\n";

    fn fixture_spec() -> BrokerSpec {
        BrokerSpec {
            listeners: vec![
                Listener { port: 41001, mode: Mode::BaseUrl { upstream: "sentry.io".into() } },
                Listener { port: 41002, mode: Mode::BaseUrl { upstream: "us.sentry.io".into() } },
            ],
            proxy_listeners: vec![
                ProxyListener { port: 41000, hosts: vec!["api.github.com".into(), "github.com".into()] },
                ProxyListener { port: 41003, hosts: vec!["github.com".into()] },
                ProxyListener { port: 41004, hosts: vec![] },
            ],
            files: vec![
                PhantomFile {
                    owner: "vscode".into(),
                    path: "/home/vscode/.sentryclirc".into(),
                    phantom: "sntrys_x".into(),
                    content: "[auth]\ntoken=sntrys_x\n".into(),
                },
                PhantomFile {
                    owner: "1000:1000".into(),
                    path: "/home/u/a b/c\\d".into(),
                    phantom: "p".into(),
                    content: "p".into(),
                },
            ],
            proxy_env: vec![("HTTPS_PROXY".into(), "http://proxy:3128".into()), ("NO_PROXY".into(), "".into())],
        }
    }

    #[test]
    fn fixture_round_trips() {
        assert_eq!(serialize(&fixture_spec()), FIXTURE);
        assert_eq!(parse(FIXTURE), Ok(fixture_spec()));
        assert_eq!(serialize(&BrokerSpec::default()), "");
        assert_eq!(parse(""), Ok(BrokerSpec::default()));
    }

    #[test]
    fn rejects_malformed() {
        for (text, want) in [
            ("bogus x\n", "unknown key `bogus`"),
            ("listen 41001 base-url a.io\nlisten 41001 base-url b.io\n", "line 2: port 41001 listed twice"),
            ("listen 0 base-url a.io\n", "listen port `0`"),
            ("listen 041001 base-url a.io\n", "listen port"),
            ("listen 70000 base-url a.io\n", "listen port"),
            ("listen 41001 intercept a.io\n", "listen mode `intercept`"),
            ("listen 41001 base-url\n", "want `<port> base-url <host>`"),
            ("listen 41001 base-url  a.io\n", "want"),
            ("listen 41001 base-url A.io\n", "isn't a lowercase hostname"),
            ("listen 41001 base-url a.io.\n", "isn't a lowercase hostname"),
            ("listen 41001 base-url -a.io\n", "isn't a lowercase hostname"),
            ("listen 41001 base-url a.io:443\n", "isn't a lowercase hostname"),
            ("proxy-listen\n", "proxy-listen port ``"),
            ("proxy-listen 0 a.io\n", "proxy-listen port `0`"),
            ("proxy-listen 08 a.io\n", "proxy-listen port"),
            ("proxy-listen 41000 \n", "proxy-listen host `` isn't"),
            ("proxy-listen 41000 a.io  b.io\n", "proxy-listen host `` isn't"),
            ("proxy-listen 41000 GitHub.com\n", "isn't a lowercase hostname"),
            ("proxy-listen 41000 github.com.\n", "isn't a lowercase hostname"),
            ("proxy-listen 41000 github.com:443\n", "isn't a lowercase hostname"),
            ("proxy-listen 41000 a.io b.io a.io\n", "host `a.io` listed twice"),
            ("listen 41001 base-url a.io\nproxy-listen 41001\n", "line 2: port 41001 listed twice"),
            ("proxy-listen 41001\nlisten 41001 base-url a.io\n", "line 2: port 41001 listed twice"),
            ("proxy-listen 41001\nproxy-listen 41001 a.io\n", "line 2: port 41001 listed twice"),
            ("file vscode\n", "without an owner"),
            ("file  /a\0p\0p\n", "file owner ``"),
            ("file u /a\0p\n", "a path, a phantom and the content"),
            ("file u /a\0p\0p\0x\n", "a path, a phantom and the content"),
            ("file u a/b\0p\0p\n", "isn't absolute and plain"),
            ("file u /a/../etc/passwd\0p\0p\n", "isn't absolute and plain"),
            ("file u /a/./b\0p\0p\n", "isn't absolute and plain"),
            ("file u /a//b\0p\0p\n", "isn't absolute and plain"),
            ("file u /a/\0p\0p\n", "isn't absolute and plain"),
            ("file u /a\0\0x\n", "must hold its phantom"),
            ("file u /a\0p\0x\n", "must hold its phantom"),
            ("file u /a\0p\0p\nfile v /a\0q\0q\n", "line 2: `/a` listed twice"),
            ("file u /a\\t\0p\0p\n", "bad escape"),
            ("proxy NOEQUALS\n", "proxy without `=`"),
            ("proxy =x\n", "proxy variable ``"),
        ] {
            let err = parse(text).unwrap_err();
            assert!(err.contains(want), "{text:?}: {err}");
        }
    }
}
