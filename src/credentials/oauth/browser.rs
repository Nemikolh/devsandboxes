//! The browser login: authorization code with PKCE `S256` (RFC 7636) and a
//! loopback redirect (RFC 8252 §7.3). The home listens on
//! `127.0.0.1:<random>`, opens the provider's authorize page, and takes
//! the one callback carrying the `state` it sent; the code is then
//! exchanged with the verifier, so a code caught on the way is useless.
//! Needs a browser on this host: the redirect lands on its loopback.

use std::io::{BufReader, ErrorKind, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::{Clock, Http, Reply, Ui, form_decode, form_encode, pkce_challenge, random};
use crate::broker::head::{self, RequestHead};
use crate::credentials::store::OAuthToken;

pub const TIMEOUT: Duration = Duration::from_secs(5 * 60);

pub const CALLBACK: &str = "/callback";

/// The authorize page's URL: `authorize_url` with the request's query
/// appended (after its own query, if it has one).
pub fn authorize_url(
    authorize_url: &str,
    client_id: &str,
    scope: Option<&str>,
    redirect: &str,
    state: &str,
    challenge: &str,
) -> String {
    let mut params = vec![
        ("response_type", "code"),
        ("client_id", client_id),
        ("redirect_uri", redirect),
        ("state", state),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
    ];
    params.extend(scope.map(|s| ("scope", s)));
    let query = form_encode(&params);
    let sep = if authorize_url.contains('?') { '&' } else { '?' };
    format!("{authorize_url}{sep}{query}")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Callback {
    Code(String),
    /// The provider's error, with our `state`: the user said no.
    Denied(String),
    /// Not our callback (another path, another state, no code): answered
    /// with this status and ignored.
    Refused(u16, &'static str),
}

/// One request to the listener, `state` the one sent. Only a `GET` of
/// [`CALLBACK`] with exactly that `state` counts, a duplicated parameter
/// being no answer.
pub fn callback(method: &str, target: &str, state: &str) -> Callback {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path != CALLBACK {
        return Callback::Refused(404, "not the login callback");
    }
    if method != "GET" {
        return Callback::Refused(405, "the callback is a GET");
    }
    let params = form_decode(query);
    let one = |key: &str| -> Result<Option<&str>, ()> {
        let mut found = params.iter().filter(|(k, _)| k == key).map(|(_, v)| v.as_str());
        match (found.next(), found.next()) {
            (v, None) => Ok(v),
            _ => Err(()),
        }
    };
    let (Ok(got), Ok(code), Ok(error)) = (one("state"), one("code"), one("error")) else {
        return Callback::Refused(400, "a parameter given twice");
    };
    if !got.is_some_and(|g| same(g, state)) {
        return Callback::Refused(400, "not this login's state");
    }
    if let Some(error) = error {
        let why = match one("error_description") {
            Ok(Some(d)) => format!("{error} ({d})"),
            _ => error.to_string(),
        };
        return Callback::Denied(crate::open::printable(&why));
    }
    match code {
        Some(c) if !c.is_empty() => Callback::Code(c.to_string()),
        _ => Callback::Refused(400, "no code"),
    }
}

/// Constant-time in the length of `a`: the state is a secret of this login.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub struct Endpoints<'a> {
    pub client_id: &'a str,
    pub authorize_url: &'a str,
    pub token_url: &'a str,
    pub scope: Option<&'a str>,
}

pub fn login(
    http: &Http,
    at: &Endpoints<'_>,
    timeout: Duration,
    clock: &mut dyn Clock,
    ui: &mut dyn Ui,
) -> Result<OAuthToken> {
    let listener = TcpListener::bind("127.0.0.1:0").context("cannot listen on 127.0.0.1 for the login callback")?;
    listener.set_nonblocking(true)?;
    let redirect = format!("http://127.0.0.1:{}{CALLBACK}", listener.local_addr()?.port());
    let state = random(32)?;
    let verifier = random(32)?;
    let url = authorize_url(at.authorize_url, at.client_id, at.scope, &redirect, &state, &pkce_challenge(&verifier));
    ui.show(&format!("Opening your browser to log in; if it doesn't open, visit:\n  {url}\nwaiting…\n"));
    ui.open(&url);

    let code = wait(&listener, &state, timeout, clock)?;
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("redirect_uri", redirect.as_str()),
        ("client_id", at.client_id),
        ("code_verifier", verifier.as_str()),
    ];
    match http.token(at.token_url, &form)? {
        Reply::Token(t) => Ok(t.into_record(at.token_url, at.client_id, clock.unix())),
        Reply::Error(e) => bail!("the token endpoint refused the code: {e}"),
    }
}

/// Answers every request to the listener until the callback with `state`.
fn wait(listener: &TcpListener, state: &str, timeout: Duration, clock: &mut dyn Clock) -> Result<String> {
    let deadline = clock.now() + timeout;
    loop {
        if clock.now() >= deadline {
            bail!("no login callback within {}s: run `devsandbox login` again", timeout.as_secs());
        }
        let tcp = match listener.accept() {
            Ok((tcp, _)) => tcp,
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                clock.sleep(Duration::from_millis(50));
                continue;
            }
            Err(e) => return Err(e).context("accepting the login callback"),
        };
        match one(tcp, state) {
            Ok(Some(Ok(code))) => return Ok(code),
            Ok(Some(Err(why))) => bail!("the login was denied at the provider: {why}"),
            // A stray or malformed request isn't the login's end.
            Ok(None) | Err(_) => {}
        }
    }
}

/// `Some`: the callback, a code or a denial.
fn one(tcp: TcpStream, state: &str) -> std::io::Result<Option<Result<String, String>>> {
    tcp.set_nonblocking(false)?;
    tcp.set_read_timeout(Some(Duration::from_secs(10)))?;
    tcp.set_write_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(tcp.try_clone()?);
    let Some(RequestHead { method, target, .. }) = head::read_head(&mut reader)? else {
        return Ok(None);
    };
    let (status, page, out) = match callback(&method, target.path(), state) {
        Callback::Code(code) => (200, "Logged in: you can close this tab.", Some(Ok(code))),
        Callback::Denied(why) => (200, "The login was denied: you can close this tab.", Some(Err(why))),
        Callback::Refused(status, why) => (status, why, None),
    };
    let mut w = tcp;
    write!(
        w,
        "HTTP/1.1 {status} {}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n{page}\n",
        if status == 200 { "OK" } else { "Refused" },
        page.len() + 1
    )?;
    Ok(out)
}
