//! The device flow (RFC 8628): the home asks the provider for a code, the
//! user enters it at the provider's page from any browser, and the home
//! polls the token endpoint until they approved, denied, or the code
//! expired. Works on a headless home: nothing here needs a browser.

use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::{Clock, Http, Reply, TokenSet, Ui, fields, openable, seconds, text};
use crate::credentials::store::{OAuthToken, Secret};

const GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// RFC 8628 §3.2's default when the provider sends none.
const DEFAULT_INTERVAL: Duration = Duration::from_secs(5);

/// §3.5: `slow_down` adds this to the interval.
const SLOW_DOWN: Duration = Duration::from_secs(5);

/// The device authorization response (§3.2).
pub struct Authorization {
    pub device_code: Secret,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    pub expires_in: u64,
    pub interval: Option<u64>,
}

/// Errors name the status and the provider's error, never the body.
pub fn parse_authorization(status: u16, content_type: Option<&str>, body: &[u8]) -> Result<Authorization> {
    let map = fields(content_type, body).with_context(|| format!("the device endpoint answered {status}"))?;
    if let Some(e) = super::error(&map) {
        bail!("the device endpoint refused: {e}");
    }
    if !(200..300).contains(&status) {
        bail!("the device endpoint answered {status}");
    }
    let need = |key: &str| text(&map, key).with_context(|| format!("the device endpoint's reply has no `{key}`"));
    Ok(Authorization {
        device_code: Secret::new(need("device_code")?),
        user_code: need("user_code")?,
        // Google's pre-RFC name.
        verification_uri: text(&map, "verification_uri")
            .or_else(|| text(&map, "verification_url"))
            .context("the device endpoint's reply has no `verification_uri`")?,
        verification_uri_complete: text(&map, "verification_uri_complete"),
        expires_in: seconds(&map, "expires_in").context("the device endpoint's reply has no `expires_in`")?,
        interval: seconds(&map, "interval"),
    })
}

/// The poll's state between two token requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Poller {
    pub interval: Duration,
    /// On the [`Clock`]'s scale: past it the code is dead.
    pub deadline: Duration,
}

pub enum Step {
    Again,
    Done(TokenSet),
}

impl Poller {
    pub fn new(auth: &Authorization, now: Duration) -> Poller {
        Poller {
            interval: auth.interval.map_or(DEFAULT_INTERVAL, Duration::from_secs).max(Duration::from_secs(1)),
            deadline: now + Duration::from_secs(auth.expires_in),
        }
    }

    /// One token reply: wait and ask again, done, or the login's end.
    pub fn next(&mut self, reply: Reply) -> Result<Step> {
        let e = match reply {
            Reply::Token(t) => return Ok(Step::Done(t)),
            Reply::Error(e) => e,
        };
        match e.code.as_str() {
            "authorization_pending" => Ok(Step::Again),
            "slow_down" => {
                let theirs = e.interval.map_or(Duration::ZERO, Duration::from_secs);
                self.interval = (self.interval + SLOW_DOWN).max(theirs);
                Ok(Step::Again)
            }
            "access_denied" => bail!("the login was denied at the provider"),
            "expired_token" => bail!("the code expired before the login was approved: run `devsandbox login` again"),
            _ => bail!("the token endpoint refused: {e}"),
        }
    }
}

/// Polls with `ask` every interval until a token, a refusal, or the code's
/// expiry.
pub fn poll(auth: &Authorization, clock: &mut dyn Clock, mut ask: impl FnMut() -> Result<Reply>) -> Result<TokenSet> {
    let mut p = Poller::new(auth, clock.now());
    loop {
        clock.sleep(p.interval);
        if clock.now() >= p.deadline {
            bail!("the code expired before the login was approved: run `devsandbox login` again");
        }
        if let Step::Done(t) = p.next(ask()?)? {
            return Ok(t);
        }
    }
}

pub struct Endpoints<'a> {
    pub client_id: &'a str,
    pub device_url: &'a str,
    pub token_url: &'a str,
    pub scope: Option<&'a str>,
}

pub fn login(http: &Http, at: &Endpoints<'_>, clock: &mut dyn Clock, ui: &mut dyn Ui) -> Result<OAuthToken> {
    let mut code = vec![("client_id", at.client_id)];
    code.extend(at.scope.map(|s| ("scope", s)));
    let (status, ct, body) = http.post(at.device_url, &code)?;
    let auth = parse_authorization(status, ct.as_deref(), &body).with_context(|| format!("calling {}", at.device_url))?;
    let uri = crate::open::printable(&auth.verification_uri);
    ui.show(&format!(
        "Open {uri} and enter the code {} (it expires in {} min); waiting…\n",
        crate::open::printable(&auth.user_code),
        auth.expires_in.div_ceil(60)
    ));
    let page = auth.verification_uri_complete.as_deref().unwrap_or(&auth.verification_uri);
    if openable(page) {
        ui.open(page);
    }
    let form = [("grant_type", GRANT), ("device_code", auth.device_code.expose()), ("client_id", at.client_id)];
    let set = poll(&auth, clock, || http.token(at.token_url, &form))?;
    Ok(set.into_record(at.token_url, at.client_id, clock.unix()))
}
