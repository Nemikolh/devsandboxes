//! OAuth logins for `devsandbox login` (docs/architecture/credentials/login.md):
//! the device flow ([`device`], RFC 8628) and the browser's authorization
//! code with PKCE ([`browser`], RFC 7636 / 8252), for a claim whose `login`
//! lists them. Both end in an [`OAuthToken`] for the store.
//!
//! The provider's endpoints are called over the broker's upstream client
//! ([`Http`]), so they pass its public-address check like any upstream: a
//! claim's token URL can't point the home at its LAN. Replies are parsed
//! here, pure, and never quoted back: a token endpoint's body may hold the
//! token.

pub mod browser;
pub mod device;
pub mod refresh;

use std::io::Read;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::broker::head::{self, Header};
use crate::broker::upstream::{Body, Request, Upstream};
use crate::credentials::store::{OAuthToken, Secret};

/// An endpoint's reply past this is refused, unread.
const MAX_REPLY: u64 = 64 * 1024;

/// What a flow tells the user and opens for them.
pub trait Ui {
    fn show(&mut self, text: &str);
    /// A URL for the user's browser; a no-op where none can open.
    fn open(&mut self, url: &str);
}

/// A provider or claim chose this URL: only an `https` one without control
/// characters reaches the opener.
pub fn openable(url: &str) -> bool {
    url.starts_with("https://") && !crate::open::has_bad_chars(url)
}

/// Monotonic time and waiting, injectable so the device poll runs in tests
/// without sleeping.
pub trait Clock {
    /// Since some fixed start.
    fn now(&self) -> Duration;
    fn sleep(&mut self, d: Duration);
    /// Wall-clock unix seconds: what the store's expiries are in.
    fn unix(&self) -> u64;
}

pub struct SystemClock(Instant);

impl SystemClock {
    pub fn new() -> SystemClock {
        SystemClock(Instant::now())
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }

    fn sleep(&mut self, d: Duration) {
        std::thread::sleep(d);
    }

    fn unix(&self) -> u64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
    }
}

pub struct TokenSet {
    pub access_token: Secret,
    pub refresh_token: Option<Secret>,
    pub expires_in: Option<u64>,
    /// GitHub's, for an app user token's refresh token (6 months).
    pub refresh_token_expires_in: Option<u64>,
    pub scope: Option<String>,
}

impl TokenSet {
    /// What the store keeps, the lifetimes made absolute from `now` (unix
    /// seconds).
    pub fn into_record(self, token_url: &str, client_id: &str, now: u64) -> OAuthToken {
        OAuthToken {
            access_token: self.access_token,
            refresh_token: self.refresh_token,
            expires_at: self.expires_in.map(|s| now.saturating_add(s)),
            refresh_expires_at: self.refresh_token_expires_in.map(|s| now.saturating_add(s)),
            token_url: token_url.to_string(),
            client_id: client_id.to_string(),
            scope: self.scope,
        }
    }
}

/// An RFC 6749 §5.2 error (GitHub's too, which answers it with a 200).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthError {
    pub code: String,
    pub description: Option<String>,
    /// `slow_down`'s new interval, where the provider sends one (GitHub).
    pub interval: Option<u64>,
}

impl std::fmt::Display for OAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&crate::open::printable(&self.code))?;
        if let Some(d) = &self.description {
            write!(f, " ({})", crate::open::printable(d))?;
        }
        Ok(())
    }
}

pub enum Reply {
    Token(TokenSet),
    Error(OAuthError),
}

/// An endpoint's body as fields: JSON (asked for with `Accept`), else a
/// form, which GitHub answers when not asked for JSON.
fn fields(content_type: Option<&str>, body: &[u8]) -> Result<Map<String, Value>> {
    let ct = content_type.unwrap_or_default().to_ascii_lowercase();
    if ct.starts_with("application/x-www-form-urlencoded") {
        let text = std::str::from_utf8(body).context("a form reply isn't UTF-8")?;
        return Ok(form_decode(text).into_iter().map(|(k, v)| (k, Value::String(v))).collect());
    }
    match serde_json::from_slice(body) {
        Ok(Value::Object(map)) => Ok(map),
        _ => bail!("the reply is neither a JSON object nor a form"),
    }
}

fn text(map: &Map<String, Value>, key: &str) -> Option<String> {
    match map.get(key)? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

/// A number, or a string holding one (as a form carries it).
fn seconds(map: &Map<String, Value>, key: &str) -> Option<u64> {
    match map.get(key)? {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn error(map: &Map<String, Value>) -> Option<OAuthError> {
    Some(OAuthError {
        code: text(map, "error")?,
        description: text(map, "error_description"),
        interval: seconds(map, "interval"),
    })
}

/// A token endpoint's answer: an error by its `error` field whatever the
/// status, a token only on a 2xx carrying `access_token`. Errors name the
/// status, never the body.
pub fn parse_token_reply(status: u16, content_type: Option<&str>, body: &[u8]) -> Result<Reply> {
    let map = fields(content_type, body).with_context(|| format!("the token endpoint answered {status}"))?;
    if let Some(e) = error(&map) {
        return Ok(Reply::Error(e));
    }
    let access_token = match text(&map, "access_token") {
        Some(t) if (200..300).contains(&status) => t,
        _ => bail!("the token endpoint answered {status} with no access token and no error"),
    };
    if let Some(kind) = text(&map, "token_type")
        && !kind.eq_ignore_ascii_case("bearer")
    {
        bail!("the token endpoint issued a `{}` token, not a bearer one", crate::open::printable(&kind));
    }
    Ok(Reply::Token(TokenSet {
        access_token: Secret::new(access_token),
        refresh_token: text(&map, "refresh_token").map(Secret::new),
        expires_in: seconds(&map, "expires_in"),
        refresh_token_expires_in: seconds(&map, "refresh_token_expires_in"),
        scope: text(&map, "scope"),
    }))
}

/// The OAuth endpoints' and login's token check's client: the broker's
/// upstream one, with its address check (the private override only for the
/// check, on the claim's own private upstream, as the broker calls it).
pub struct Http {
    upstream: Upstream,
    private: bool,
}

impl Http {
    pub fn from_host() -> Result<Http> {
        Ok(Http::new(Upstream::from_host()?))
    }

    pub fn new(upstream: Upstream) -> Http {
        Http { upstream, private: false }
    }

    /// Loopback let through, for a test server.
    #[cfg(test)]
    pub fn loopback(upstream: Upstream) -> Http {
        Http { upstream, private: true }
    }

    /// POSTs `form` to `url`: the status, `Content-Type` and body.
    fn post(&self, url: &str, form: &[(&str, &str)]) -> Result<(u16, Option<String>, Vec<u8>)> {
        let body = form_encode(form);
        let headers = [
            Header { name: "Accept".into(), value: b"application/json".to_vec() },
            Header { name: "Content-Type".into(), value: b"application/x-www-form-urlencoded".to_vec() },
            Header { name: "User-Agent".into(), value: USER_AGENT.into() },
        ];
        let mut reader = body.as_bytes();
        let resp = self
            .upstream
            .send(Request {
                method: "POST",
                url,
                headers: &headers,
                body: Body::Length(body.len() as u64, &mut reader),
                private: self.private,
            })
            .with_context(|| format!("cannot reach {url}"))?;
        let content_type = head::header(&resp.headers, "content-type").map(|v| String::from_utf8_lossy(v).into_owned());
        let status = resp.status;
        Ok((status, content_type, bounded(url, resp.body)?))
    }

    /// A bodiless `method` (`GET`, `HEAD`) to `url` with `headers` (the
    /// token's slot among them): the status, the response's headers and
    /// body. `private`: the claim marks the upstream private.
    pub fn fetch(&self, method: &str, url: &str, headers: &[Header], private: bool) -> Result<(u16, Vec<Header>, Vec<u8>)> {
        let mut sent = vec![Header { name: "User-Agent".into(), value: USER_AGENT.into() }];
        sent.extend(headers.iter().cloned());
        let resp = self
            .upstream
            .send(Request { method, url, headers: &sent, body: Body::Empty, private: self.private || private })
            .with_context(|| format!("cannot reach {url}"))?;
        let body = bounded(url, resp.body)?;
        Ok((resp.status, resp.headers, body))
    }

    pub fn token(&self, url: &str, form: &[(&str, &str)]) -> Result<Reply> {
        let (status, ct, body) = self.post(url, form)?;
        parse_token_reply(status, ct.as_deref(), &body).with_context(|| format!("calling {url}"))
    }
}

const USER_AGENT: &str = concat!("devsandbox/", env!("CARGO_PKG_VERSION"));

/// Refused past [`MAX_REPLY`], unread.
fn bounded(url: &str, body: impl Read) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    body.take(MAX_REPLY + 1).read_to_end(&mut out).with_context(|| format!("reading {url}'s reply"))?;
    if out.len() as u64 > MAX_REPLY {
        bail!("{url} answered over {MAX_REPLY} bytes");
    }
    Ok(out)
}

/// `application/x-www-form-urlencoded`, also a URL's query: everything but
/// RFC 3986's unreserved characters percent-encoded.
pub fn form_encode(fields: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (i, (k, v)) in fields.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        percent_encode(k, &mut out);
        out.push('=');
        percent_encode(v, &mut out);
    }
    out
}

fn percent_encode(s: &str, out: &mut String) {
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
}

/// A form or query's pairs, in order; `+` is a space, a bad escape kept as
/// is, a key without `=` an empty value.
pub fn form_decode(s: &str) -> Vec<(String, String)> {
    s.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                match (hex(b[i + 1]), hex(b[i + 2])) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 2;
                    }
                    _ => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// RFC 4648 §5, unpadded.
pub fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (chunk[0] as u32) << 16 | (*chunk.get(1).unwrap_or(&0) as u32) << 8 | *chunk.get(2).unwrap_or(&0) as u32;
        for i in 0..=chunk.len() {
            out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

/// `n` bytes from the OS random source, base64url.
pub fn random(n: usize) -> Result<String> {
    let mut bytes = vec![0u8; n];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("cannot read the OS random source: {e}"))?;
    Ok(base64url(&bytes))
}

/// RFC 7636 §4.2's `S256` challenge for `verifier`.
pub fn pkce_challenge(verifier: &str) -> String {
    base64url(&Sha256::digest(verifier.as_bytes()))
}

#[cfg(test)]
pub(crate) mod tests;
