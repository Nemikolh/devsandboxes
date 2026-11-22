//! `devsandbox login <credential>` (docs/reference/credentials-cli.md): store
//! a credential's secret in the home's file store and approve the claim it
//! is for (credential-broker, *`credentials.toml`*; github-and-sentry-access,
//! *The claim contract*). Local only: this host is its credentials' home.
//! The secret comes by paste, the device flow or the browser's OAuth login,
//! per the claim's `login` (docs/architecture/credentials/login.md).

use std::collections::BTreeMap;
use std::io::{IsTerminal, Read, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::broker::head::Header;
use crate::credentials::claim::{self, Claim, Create, Level, Login, Pinned, Route, Scope, Slot};
use crate::credentials::home::{self, Entry, Home, Version};
use crate::credentials::oauth::{self, Clock, Http, browser, device};
use crate::credentials::store::{Record, Secret, Store};

pub struct Args {
    pub credential: String,
    pub feature: Option<String>,
    pub kind: Option<String>,
    pub profiles: Vec<String>,
    pub yes: bool,
    /// `paste`, `device` or `browser`: one of the claim's methods.
    pub method: Option<String>,
    /// The OAuth app to log in to instead of the claim's (one the home
    /// owns), required when the claim names none; kept in the token
    /// record, not the claim.
    pub client_id: Option<String>,
    /// The user or org a paste login's token page prefills as the token's
    /// resource owner.
    pub owner: Option<String>,
}

/// What the OAuth logins reach beyond the terminal, a seam for tests.
pub(crate) struct Flows {
    /// Called only once an OAuth method is picked.
    pub http: Box<dyn Fn() -> Result<Http>>,
    pub clock: Box<dyn Clock>,
    /// Whether a browser opens on this host: the browser login's redirect
    /// lands on this host's loopback.
    pub browser: bool,
    pub open: Box<dyn FnMut(&str)>,
    pub browser_timeout: Duration,
    /// The authority dialed for a claim's upstream host: the host itself,
    /// a local server in tests.
    pub authority: Box<dyn Fn(&str) -> String>,
}

impl Flows {
    fn host() -> Flows {
        let os = std::env::consts::OS;
        Flows {
            http: Box::new(Http::from_host),
            clock: Box::new(oauth::SystemClock::new()),
            browser: crate::open::has_display(os, |k| std::env::var_os(k)) && crate::open::opener_argv(os, "").is_some(),
            open: Box::new(move |url| {
                if crate::open::has_display(os, |k| std::env::var_os(k))
                    && let Some(argv) = crate::open::opener_argv(os, url)
                {
                    let _ = std::process::Command::new(&argv[0])
                        .args(&argv[1..])
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn();
                }
            }),
            browser_timeout: browser::TIMEOUT,
            authority: Box::new(str::to_string),
        }
    }
}

struct TermUi<'a> {
    term: &'a mut dyn Term,
    open: &'a mut dyn FnMut(&str),
}

impl oauth::Ui for TermUi<'_> {
    fn show(&mut self, text: &str) {
        self.term.show(text);
    }

    fn open(&mut self, url: &str) {
        (self.open)(url);
    }
}

/// The terminal side of `login` and `access approve`, a seam for tests.
pub(crate) trait Term {
    /// Whether stdin is a terminal to ask on.
    fn tty(&self) -> bool;
    fn show(&mut self, text: &str);
    fn confirm(&mut self, question: &str) -> Result<bool>;
    /// The secret from stdin: unechoed on a terminal, else all of it.
    fn secret(&mut self, prompt: &str) -> Result<Secret>;
}

pub(crate) struct Stdio;

impl Term for Stdio {
    fn tty(&self) -> bool {
        std::io::stdin().is_terminal()
    }

    fn show(&mut self, text: &str) {
        eprint!("{text}");
    }

    fn confirm(&mut self, question: &str) -> Result<bool> {
        super::confirm(question)
    }

    fn secret(&mut self, prompt: &str) -> Result<Secret> {
        let mut text = String::new();
        if self.tty() {
            eprint!("{prompt}: ");
            std::io::stderr().flush()?;
            let _echo = EchoOff::new()?;
            std::io::stdin().read_line(&mut text)?;
            eprintln!();
        } else {
            std::io::stdin().read_to_string(&mut text).context("cannot read the secret from stdin")?;
        }
        Ok(Secret::new(text))
    }
}

/// Stdin's terminal with echo off (line editing kept) until dropped.
struct EchoOff(libc::termios);

impl EchoOff {
    fn new() -> Result<EchoOff> {
        // SAFETY: tcgetattr fills the struct on success; it is read only then.
        let mut term: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut term) } != 0 {
            return Err(std::io::Error::last_os_error()).context("cannot read the terminal's mode");
        }
        let saved = term;
        term.c_lflag &= !(libc::ECHO | libc::ECHONL);
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &term) } != 0 {
            return Err(std::io::Error::last_os_error()).context("cannot turn the terminal's echo off");
        }
        Ok(EchoOff(saved))
    }
}

impl Drop for EchoOff {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.0) };
    }
}

/// `<data>/devsandbox` over the process env.
pub(crate) fn data_dir() -> Result<std::path::PathBuf> {
    crate::daemon_config::data_dir_from(std::env::var_os("XDG_DATA_HOME"), std::env::var_os("HOME"))
}

pub fn login(args: Args) -> Result<()> {
    let claims = match &args.feature {
        Some(reference) => Some(feature_claims(reference)?),
        None => None,
    };
    let done = login_at(&data_dir()?, &args, claims, &mut Stdio, &mut Flows::host())?;
    println!("{done}");
    Ok(())
}

/// The claims of the feature `reference`: an OCI ref, or a local folder
/// (`./…`, `../…`, `/…`, as a config writes it) resolved against the
/// current dir.
fn feature_claims(reference: &str) -> Result<Vec<Claim>> {
    let source = crate::features::FeatureSource::parse(reference)?;
    let cwd = std::env::current_dir().context("cannot read the current dir")?;
    let feature = crate::features::resolve(&source, &cwd)?;
    if feature.metadata.credentials.is_empty() {
        bail!("feature `{reference}` claims no credential");
    }
    Ok(feature.metadata.credentials)
}

/// The claim of `claims` the credential `name` is for: `kind`'s, else the
/// only one, else the one whose kind is `name`.
fn pick(claims: Vec<Claim>, name: &str, kind: Option<&str>) -> Result<Claim> {
    let kinds = || claims.iter().map(|c| format!("`{}`", c.kind)).collect::<Vec<_>>().join(", ");
    if let Some(kind) = kind {
        let found = kinds();
        return claims
            .into_iter()
            .find(|c| c.kind == kind)
            .with_context(|| format!("the feature claims no `{kind}` credential (it claims {found})"));
    }
    if claims.len() == 1 {
        return Ok(claims.into_iter().next().expect("one claim"));
    }
    let found = kinds();
    claims
        .into_iter()
        .find(|c| c.kind == name)
        .with_context(|| format!("the feature claims {found}: pass --kind to pick the one `{name}` is for"))
}

fn method_name(login: &Login) -> &'static str {
    match login {
        Login::Paste { .. } => "paste",
        Login::Device { .. } => "device",
        Login::Browser { .. } => "browser",
        Login::GithubAppManifest {} => "github-app-manifest",
    }
}

/// A device or browser login whose claim names no OAuth app.
fn needs_client_id(login: &Login) -> bool {
    matches!(login, Login::Device { client_id: None, .. } | Login::Browser { client_id: None, .. })
}

fn client_id_required(login: &Login, kind: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "the `{kind}` claim's {} login names no OAuth app: pass --client-id <your app's client id>",
        method_name(login)
    )
}

/// The login method: `wanted` if the claim lists it, else the first of
/// the claim's that works here, in this order: off a terminal paste (stdin),
/// device, browser; on one device, browser, paste. Browser only where a
/// browser opens, a login needing `--client-id` only with one. A claim's
/// own order isn't kept (its version sorts them).
fn choose<'a>(
    logins: &'a [Login],
    wanted: Option<&str>,
    client_id: bool,
    tty: bool,
    browser: bool,
    kind: &str,
) -> Result<&'a Login> {
    if logins.is_empty() {
        bail!("the `{kind}` claim has no secret to log in (no `inject` slot)");
    }
    let listed = || logins.iter().map(method_name).collect::<Vec<_>>().join(", ");
    let find = |m: &str| logins.iter().find(|l| method_name(l) == m);
    if let Some(m) = wanted {
        let l = find(m).with_context(|| format!("the `{kind}` claim logs in by {}, not {m}", listed()))?;
        if needs_client_id(l) && !client_id {
            return Err(client_id_required(l, kind));
        }
        return Ok(l);
    }
    let order: &[&str] = if tty { &["device", "browser", "paste"] } else { &["paste", "device", "browser"] };
    let usable = |l: &&Login| client_id || !needs_client_id(l);
    if let Some(l) = order.iter().filter(|m| browser || **m != "browser").find_map(|m| find(m).filter(usable)) {
        return Ok(l);
    }
    if let Some(l) = order.iter().filter_map(|m| find(m)).find(|l| needs_client_id(l) && !client_id) {
        return Err(client_id_required(l, kind));
    }
    if find("browser").is_some() {
        bail!("the `{kind}` claim logs in by {}: no browser opens here (pass --method browser to try anyway)", listed());
    }
    bail!("the `{kind}` claim logs in by {}: {}", listed(), crate::NOT_SUPPORTED_YET)
}

fn check_client_id(id: &str) -> Result<()> {
    if id.is_empty() || id.chars().any(|c| c.is_control() || c.is_whitespace()) {
        bail!("--client-id must be non-empty, no whitespace");
    }
    Ok(())
}

fn check_owner(owner: &str) -> Result<()> {
    if owner.is_empty() || owner.chars().any(|c| c.is_control() || c.is_whitespace()) {
        bail!("--owner must be non-empty, no whitespace");
    }
    Ok(())
}

/// GitHub's cap on a token's name.
const TOKEN_NAME_MAX: usize = 40;

/// The page a paste login's `create` opens: its `url` with the token's
/// name, description and owner prefilled and, with `permissions`, every
/// permission the granted scopes (the claim's within `max`; unset: all)
/// ask, at the highest level any of them asks.
fn create_url(
    create: &Create,
    scopes: &BTreeMap<String, Scope>,
    max: Option<&[String]>,
    name: &str,
    profiles: Option<&[String]>,
    owner: Option<&str>,
) -> String {
    let title: String = format!("devsandbox {name}").chars().take(TOKEN_NAME_MAX).collect();
    let description = match profiles {
        Some(p) => format!("devsandbox credential {name}, profiles: {}", p.join(", ")),
        None => format!("devsandbox credential {name}, every profile"),
    };
    let mut params = vec![("name", title.as_str()), ("description", description.as_str())];
    if let Some(owner) = owner {
        params.push(("target_name", owner));
    }
    let mut levels: BTreeMap<&str, Level> = BTreeMap::new();
    if create.permissions {
        let granted = scopes.iter().filter(|(s, _)| max.is_none_or(|m| m.contains(s)));
        for (p, l) in granted.flat_map(|(_, scope)| &scope.permissions) {
            let at = levels.entry(p.as_str()).or_insert(*l);
            *at = (*at).max(*l);
        }
    }
    params.extend(levels.iter().map(|(p, l)| (*p, l.as_str())));
    format!("{}?{}", create.url, oauth::form_encode(&params))
}

/// The secret by `login`'s method; `page`: where a paste login's token is
/// created ([`create_url`]).
fn obtain(
    login: &Login,
    name: &str,
    page: Option<&str>,
    args: &Args,
    term: &mut dyn Term,
    flows: &mut Flows,
) -> Result<Record> {
    let client = |claims: &Option<String>| {
        args.client_id.clone().or_else(|| claims.clone()).expect("choose requires --client-id when the claim has none")
    };
    match login {
        Login::Paste { hint, .. } => {
            // Piped, the secret exists already: no page to send the user to.
            if let Some(url) = page
                && term.tty()
            {
                let mut text = format!("Create the token at {}\n  select the repositories it reaches", crate::open::printable(url));
                if let Some(owner) = &args.owner {
                    let owner = crate::open::printable(owner);
                    text.push_str(&format!("; check its resource owner is {owner} (the page may show {owner} yet create it under your user)"));
                }
                text.push('\n');
                term.show(&text);
                if oauth::openable(url) {
                    (flows.open)(url);
                }
            }
            let secret = term.secret(&match hint {
                Some(hint) => format!("Paste {hint}"),
                None => format!("Paste the `{name}` secret"),
            })?;
            let secret = Secret::new(secret.expose().trim().to_string());
            if secret.expose().is_empty() {
                bail!("no secret given: nothing changed");
            }
            Ok(Record::token(secret))
        }
        Login::Device { client_id, device_url, token_url } => {
            let http = (flows.http)()?;
            let client_id = client(client_id);
            let at = device::Endpoints { client_id: &client_id, device_url, token_url };
            let mut ui = TermUi { term, open: &mut *flows.open };
            Ok(Record::OAuth(device::login(&http, &at, &mut *flows.clock, &mut ui)?))
        }
        Login::Browser { client_id, authorize_url, token_url } => {
            let http = (flows.http)()?;
            let client_id = client(client_id);
            let at = browser::Endpoints { client_id: &client_id, authorize_url, token_url };
            let mut ui = TermUi { term, open: &mut *flows.open };
            Ok(Record::OAuth(browser::login(&http, &at, flows.browser_timeout, &mut *flows.clock, &mut ui)?))
        }
        Login::GithubAppManifest {} => unreachable!("choose never picks it"),
    }
}

/// What the claim's `check` route said of a token.
#[derive(Debug, PartialEq)]
enum Checked {
    /// A 401, as the broker's `check_401` reads it: a 403 may be a rate
    /// limit or an org policy, not a bad token.
    Refused,
    /// `shows`: the claim's `checkShows` field; `expires_at`: its expiry
    /// header's (unix seconds).
    Valid { shows: Option<String>, expires_at: Option<u64> },
    /// Anything else: says nothing of the token.
    Unknown(String),
}

/// The claim's `check` call with `token` in its first slot, as the broker
/// makes it (`check_401`): method, URL on the first upstream at
/// `authority`, headers, whether that upstream is private. `None`: no
/// check, or no slot.
fn check_request(pinned: &Pinned, token: &str, authority: &dyn Fn(&str) -> String) -> Option<(&'static str, String, Vec<Header>, bool)> {
    let Some(Route::Http { method, path }) = &pinned.check else { return None };
    let slot = pinned.inject.first()?;
    let upstream = pinned.upstreams.first()?;
    let mut path = path.clone();
    let mut headers = Vec::new();
    match slot {
        Slot::Header { name, .. } => headers.push(Header { name: name.clone(), value: Vec::new() }),
        Slot::Basic { .. } => headers.push(Header { name: "Authorization".into(), value: Vec::new() }),
        Slot::Query { param } => {
            path.push(if path.contains('?') { '&' } else { '?' });
            path.push_str(param);
            path.push('=');
        }
    }
    crate::broker::policy::inject(&mut headers, &mut path, slot, token);
    let url = format!("https://{}{path}", authority(&upstream.host));
    Some((method.as_str(), url, headers, upstream.private))
}

/// The check's answer. A 2xx's body is read for `checkShows` only; a
/// missing or non-string field shows nothing.
fn read_check(pinned: &Pinned, status: u16, headers: &[Header], body: &[u8]) -> Checked {
    match status {
        401 => Checked::Refused,
        200..=299 => {
            let shows = pinned.check_shows.as_deref().and_then(|field| {
                let doc: serde_json::Value = serde_json::from_slice(body).ok()?;
                doc.get(field)?.as_str().filter(|s| !s.is_empty()).map(String::from)
            });
            let headers = headers.iter().map(|h| (h.name.as_str(), h.value.as_slice()));
            let expires_at = crate::credentials::tokens::captured(pinned.tokens.as_ref(), headers, None);
            Checked::Valid { shows, expires_at }
        }
        _ => Checked::Unknown(format!("it answered {status}")),
    }
}

/// `None` when the claim has no check.
fn check_token(pinned: &Pinned, token: &str, flows: &Flows) -> Option<Checked> {
    let (method, url, headers, private) = check_request(pinned, token, &*flows.authority)?;
    let answer = (flows.http)().and_then(|http| http.fetch(method, &url, &headers, private));
    Some(match answer {
        Ok((status, headers, body)) => read_check(pinned, status, &headers, &body),
        Err(e) => Checked::Unknown(format!("{e:#}")),
    })
}

/// Returns the line to print.
pub(crate) fn login_at(
    data: &Path,
    args: &Args,
    claims: Option<Vec<Claim>>,
    term: &mut dyn Term,
    flows: &mut Flows,
) -> Result<String> {
    let name = args.credential.as_str();
    crate::credentials::check_name(name)?;
    for p in &args.profiles {
        crate::config::check_profile_name(p)?;
    }
    let path = home::path(data);
    let existing = home::load(&path)?.remove(name);
    if existing.is_some() && !args.profiles.is_empty() {
        bail!("credential `{name}` exists: its profiles are `profiles` in {}", path.display());
    }
    let candidate = match claims {
        Some(claims) => Some(Version::of(&pick(claims, name, args.kind.as_deref())?)),
        None => pending_one(name, existing.as_ref(), &path)?,
    };
    let kind = match (&candidate, &existing) {
        (Some(c), _) => c.view()?.kind,
        (None, Some(e)) => e.kind.clone(),
        (None, None) => unreachable!("pending_one refuses a missing entry"),
    };
    if let Some(e) = &existing
        && e.kind != kind
    {
        bail!("credential `{name}` is a `{}` credential, not `{kind}`", e.kind);
    }
    if let Some(k) = &args.kind
        && *k != kind
    {
        bail!("credential `{name}` is a `{kind}` credential, not `{k}`");
    }

    // What decides the login method: the claim about to be approved, else
    // the one already approved.
    let deciding = match (&candidate, existing.as_ref().and_then(Entry::latest_approved)) {
        (Some(c), _) => c,
        (None, Some(v)) => v,
        (None, None) => unreachable!("pending_one refuses an entry with nothing approved"),
    };
    let shown = deciding.view()?;
    let (_, pinned) = claim::from_canonical(&deciding.claim)?;
    let login = choose(&pinned.login, args.method.as_deref(), args.client_id.is_some(), term.tty(), flows.browser, &kind)?;
    if let Some(id) = &args.client_id {
        if matches!(login, Login::Paste { .. }) {
            bail!("--client-id is for the device and browser logins, not paste");
        }
        check_client_id(id)?;
    }
    let page = match login {
        Login::Paste { create: Some(create), .. } => {
            let profiles = match &existing {
                Some(e) => e.profiles.as_deref(),
                None => (!args.profiles.is_empty()).then_some(args.profiles.as_slice()),
            };
            let max = existing.as_ref().and_then(|e| e.max.as_deref());
            Some(create_url(create, &pinned.scopes, max, name, profiles, args.owner.as_deref()))
        }
        _ => None,
    };
    if let Some(owner) = &args.owner {
        if page.is_none() {
            bail!("--owner is for a paste login that opens a token page; the `{kind}` claim's {} login has none", method_name(login));
        }
        check_owner(owner)?;
    }
    let approving = candidate
        .as_ref()
        .filter(|c| existing.as_ref().and_then(|e| e.approved_claim(&c.version)).is_none());
    if let Some(c) = approving {
        term.show(&format!("{}  version {}\n", shown.summary(), c.short()));
        approve_or_refuse(term, args.yes, &format!("Approve this claim for `{name}`?"))?;
    }

    let record = obtain(login, name, page.as_deref(), args, term, flows)?;
    crate::credentials::tokens::check_accepted(pinned.tokens.as_ref(), record.access_token().expose(), &kind)?;
    let (whose, expires_at) = match check_token(&pinned, record.access_token().expose(), flows) {
        None => (None, None),
        Some(Checked::Refused) => {
            let host = pinned.upstreams.first().map_or("", |u| u.host.as_str());
            bail!("the token was refused by {host} (401): nothing changed");
        }
        // A home may be offline.
        Some(Checked::Unknown(why)) => {
            term.show(&format!("warning: cannot check the token, stored unchecked: {}\n", crate::open::printable(&why)));
            (None, None)
        }
        // An OAuth record carries its own expiry.
        Some(Checked::Valid { shows, expires_at }) => (shows, expires_at.filter(|_| matches!(record, Record::Static { .. }))),
    };
    // The secret first: a claim approved without one serves nothing.
    Store::in_data(data).put(name, &record)?;
    let profiles = (!args.profiles.is_empty()).then(|| args.profiles.clone());
    let approved = home::update(&path, |h: &mut Home| {
        let entry = h.entry(name.to_string()).or_insert_with(|| Entry::new(&kind, profiles));
        if entry.kind != kind {
            bail!("credential `{name}` is a `{}` credential, not `{kind}`", entry.kind);
        }
        entry.broken = None;
        entry.expires_at = expires_at.map(crate::inbox::rfc3339);
        Ok(approving.cloned().map(|c| entry.approve(c)).unwrap_or(false))
    })?;
    let whose = whose.map(|w| format!(" as {}", crate::open::printable(&w))).unwrap_or_default();
    Ok(match (approved, approving) {
        (true, Some(c)) => format!("logged in `{name}` ({kind}){whose}; claim version {} approved", c.short()),
        _ => format!("logged in `{name}` ({kind}){whose}: secret replaced"),
    })
}

/// The pending version `login` without `--feature` approves; `None` when
/// nothing is pending but a version is approved (a re-login).
fn pending_one(name: &str, entry: Option<&Entry>, path: &Path) -> Result<Option<Version>> {
    let Some(entry) = entry else {
        bail!(
            "no credential `{name}` in {}: name the feature whose claim it is for \
             (`devsandbox login {name} --feature <ref>`)",
            path.display()
        );
    };
    match entry.pending.as_slice() {
        [] if entry.approved.is_empty() => bail!(
            "credential `{name}` has no claim version, approved or pending: name the feature whose claim it is for \
             (`devsandbox login {name} --feature <ref>`)"
        ),
        [] => Ok(None),
        [one] => Ok(Some(one.clone())),
        many => bail!(
            "credential `{name}` has {} pending claim versions ({}): pass --feature, or approve one with \
             `devsandbox access approve {name} --version <v>`",
            many.len(),
            many.iter().map(Version::short).collect::<Vec<_>>().join(", ")
        ),
    }
}

/// `--yes` approves; else ask on a terminal, refuse off one.
pub(crate) fn approve_or_refuse(term: &mut dyn Term, yes: bool, question: &str) -> Result<()> {
    if yes {
        return Ok(());
    }
    if !term.tty() {
        bail!("no terminal to confirm on: pass --yes to approve");
    }
    if !term.confirm(question)? {
        bail!("not approved: nothing changed");
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::credentials::home::tests::sentry;
    use std::path::PathBuf;

    /// A scripted terminal: answers in order, everything shown kept.
    pub(crate) struct Script {
        pub tty: bool,
        pub answers: Vec<bool>,
        pub secret: Option<String>,
        pub shown: String,
    }

    impl Script {
        pub(crate) fn new(tty: bool, answers: &[bool], secret: Option<&str>) -> Script {
            Script { tty, answers: answers.to_vec(), secret: secret.map(String::from), shown: String::new() }
        }
    }

    impl Term for Script {
        fn tty(&self) -> bool {
            self.tty
        }
        fn show(&mut self, text: &str) {
            self.shown.push_str(text);
        }
        fn confirm(&mut self, question: &str) -> Result<bool> {
            self.shown.push_str(question);
            Ok(self.answers.remove(0))
        }
        fn secret(&mut self, _: &str) -> Result<Secret> {
            Ok(Secret::new(self.secret.take().expect("one secret read")))
        }
    }

    pub(crate) fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dsv-login-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// Flows that refuse to reach any network.
    pub(crate) fn offline() -> Flows {
        Flows {
            http: Box::new(|| bail!("no network in this test")),
            clock: Box::new(oauth::SystemClock::new()),
            browser: false,
            open: Box::new(|_| {}),
            browser_timeout: Duration::from_secs(1),
            authority: Box::new(str::to_string),
        }
    }

    fn login_at(data: &Path, args: &Args, claims: Option<Vec<Claim>>, term: &mut dyn Term) -> Result<String> {
        super::login_at(data, args, claims, term, &mut offline())
    }

    fn args(name: &str) -> Args {
        Args {
            credential: name.into(),
            feature: None,
            kind: None,
            profiles: Vec::new(),
            yes: false,
            method: None,
            client_id: None,
            owner: None,
        }
    }

    fn other(kind: &str) -> Claim {
        let mut c = sentry();
        c.kind = kind.into();
        c
    }

    #[test]
    fn picks_by_kind_then_the_only_one_then_the_name() {
        assert_eq!(pick(vec![sentry()], "sentry-work", None).unwrap().kind, "sentry");
        let two = || vec![sentry(), other("github")];
        assert_eq!(pick(two(), "x", Some("github")).unwrap().kind, "github");
        assert_eq!(pick(two(), "github", None).unwrap().kind, "github");
        let err = pick(two(), "x", None).unwrap_err().to_string();
        assert!(err.contains("claims `sentry`, `github`: pass --kind"), "{err}");
        let err = pick(two(), "x", Some("npm")).unwrap_err().to_string();
        assert!(err.contains("no `npm` credential"), "{err}");
    }

    #[test]
    fn first_login_approves_the_feature_claim_and_stores_the_secret() {
        let data = temp("first");
        let mut a = args("sentry-work");
        a.profiles = vec!["work".into()];
        let mut term = Script::new(true, &[true], Some("tok-1\n"));
        let done = login_at(&data, &a, Some(vec![sentry()]), &mut term).unwrap();
        assert!(done.contains("claim version") && done.contains("approved"), "{done}");
        assert!(term.shown.contains("claim `sentry` (base-url)"), "{}", term.shown);
        assert_eq!(Store::in_data(&data).token_now("sentry-work").unwrap().unwrap().expose(), "tok-1");
        let h = home::load(&home::path(&data)).unwrap();
        let e = &h["sentry-work"];
        assert_eq!(e.kind, "sentry");
        assert_eq!(e.profiles.as_deref(), Some(&["work".to_string()][..]));
        assert_eq!(e.approved_claim(&sentry().version()), Some(sentry().canonical().as_str()));

        // Again with the same feature: nothing to approve, the secret only.
        let mut term = Script::new(false, &[], Some("tok-2"));
        let done = login_at(&data, &args("sentry-work"), Some(vec![sentry()]), &mut term).unwrap();
        assert!(done.ends_with("secret replaced"), "{done}");
        assert_eq!(term.shown, "warning: cannot check the token, stored unchecked: no network in this test\n");
        // And without one, which clears a broken mark.
        let at = home::Broken { at: "2026-11-21T10:00:00Z".into(), reason: "refused".into() };
        home::mark_broken(&home::path(&data), "sentry-work", at).unwrap();
        let mut term = Script::new(false, &[], Some("tok-3"));
        login_at(&data, &args("sentry-work"), None, &mut term).unwrap();
        assert_eq!(Store::in_data(&data).token_now("sentry-work").unwrap().unwrap().expose(), "tok-3");
        assert_eq!(home::load(&home::path(&data)).unwrap(), h, "the entry untouched");
        let err = login_at(&data, &a, None, &mut Script::new(false, &[], Some("x"))).unwrap_err().to_string();
        assert!(err.contains("its profiles are `profiles` in"), "{err}");
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn refusals_change_nothing() {
        let data = temp("refuse");
        let err = login_at(&data, &args("sentry-work"), None, &mut Script::new(true, &[], None)).unwrap_err();
        assert!(err.to_string().contains("--feature <ref>"), "{err}");
        let err = login_at(&data, &args("s"), Some(vec![sentry()]), &mut Script::new(false, &[], Some("t"))).unwrap_err();
        assert!(err.to_string().contains("pass --yes"), "{err}");
        let err = login_at(&data, &args("s"), Some(vec![sentry()]), &mut Script::new(true, &[false], Some("t"))).unwrap_err();
        assert!(err.to_string().contains("not approved"), "{err}");
        let mut yes = args("s");
        yes.yes = true;
        let err = login_at(&data, &yes, Some(vec![sentry()]), &mut Script::new(false, &[], Some(" \n"))).unwrap_err();
        assert!(err.to_string().contains("no secret"), "{err}");
        assert!(!home::path(&data).exists() && !Store::in_data(&data).path().exists());

        let mut device = sentry();
        device.pinned.login = vec![crate::credentials::claim::Login::Device {
            client_id: Some("c".into()),
            device_url: "https://x/d".into(),
            token_url: "https://x/t".into(),
        }];
        let paste = Args { method: Some("paste".into()), ..args("s") };
        let err = login_at(&data, &paste, Some(vec![device.clone()]), &mut Script::new(false, &[], None)).unwrap_err();
        assert!(err.to_string().contains("logs in by device, not paste"), "{err}");
        let err = login_at(&data, &yes, Some(vec![device]), &mut Script::new(false, &[], None)).unwrap_err();
        assert!(err.to_string().contains("no network in this test"), "picked device off a terminal: {err}");
        let client = Args { client_id: Some("mine".into()), ..args("s") };
        let err = login_at(&data, &client, Some(vec![sentry()]), &mut Script::new(false, &[], None)).unwrap_err();
        assert!(err.to_string().contains("--client-id is for the device and browser logins"), "{err}");
        assert!(!home::path(&data).exists() && !Store::in_data(&data).path().exists());

        login_at(&data, &yes, Some(vec![sentry()]), &mut Script::new(false, &[], Some("t"))).unwrap();
        let gh = Args { kind: Some("github".into()), yes: true, ..args("s") };
        let err = login_at(&data, &gh, Some(vec![other("github")]), &mut Script::new(false, &[], Some("t"))).unwrap_err();
        assert!(err.to_string().contains("is a `sentry` credential, not `github`"), "{err}");
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn without_a_feature_the_pending_version_is_approved() {
        let data = temp("pending");
        let path = home::path(&data);
        home::update(&path, |h| {
            h.insert("s".into(), Entry::new("sentry", None));
            home::record_pending(h, "s", &sentry().canonical())
        })
        .unwrap();
        let mut term = Script::new(true, &[true], Some("t"));
        let done = login_at(&data, &args("s"), None, &mut term).unwrap();
        assert!(done.contains("approved"), "{done}");
        let e = &home::load(&path).unwrap()["s"];
        assert!(e.pending.is_empty() && e.approved.len() == 1);

        let mut c2 = sentry();
        c2.pinned.never.pop();
        let mut c3 = sentry();
        c3.pinned.hold = Some(5);
        home::update(&path, |h| {
            home::record_pending(h, "s", &c2.canonical())?;
            home::record_pending(h, "s", &c3.canonical())
        })
        .unwrap();
        let err = login_at(&data, &args("s"), None, &mut Script::new(true, &[], Some("t"))).unwrap_err().to_string();
        assert!(err.contains("2 pending claim versions") && err.contains("--version"), "{err}");
        let _ = std::fs::remove_dir_all(&data);
    }

    fn device(port: u16) -> Login {
        Login::Device {
            client_id: Some("Iv1.claims".into()),
            device_url: format!("https://127.0.0.1:{port}/device"),
            token_url: format!("https://127.0.0.1:{port}/token"),
        }
    }

    fn browser(port: u16) -> Login {
        Login::Browser {
            client_id: Some("Iv1.claims".into()),
            authorize_url: "https://github.example/login/oauth/authorize".into(),
            token_url: format!("https://127.0.0.1:{port}/token"),
        }
    }

    /// A device login whose claim leaves the OAuth app to the home.
    fn home_device(port: u16) -> Login {
        let Login::Device { device_url, token_url, .. } = device(port) else { unreachable!() };
        Login::Device { client_id: None, device_url, token_url }
    }

    #[test]
    fn the_method_is_asked_for_else_the_first_that_works_here() {
        let paste = Login::Paste { hint: None, create: None };
        let all = [paste.clone(), device(1), browser(1)];
        let name = |l: Result<&Login>| method_name(l.unwrap());
        assert_eq!(name(choose(&all, None, false, false, true, "k")), "paste");
        assert_eq!(name(choose(&all, None, false, true, true, "k")), "device");
        assert_eq!(name(choose(&[paste.clone(), browser(1)], None, false, true, true, "k")), "browser");
        assert_eq!(name(choose(&[paste.clone(), browser(1)], None, false, true, false, "k")), "paste");
        assert_eq!(name(choose(&[device(1)], None, false, false, false, "k")), "device");
        assert_eq!(name(choose(&all, Some("browser"), false, false, false, "k")), "browser");
        let err = choose(&[browser(1)], None, false, true, false, "k").unwrap_err().to_string();
        assert!(err.contains("no browser opens here"), "{err}");
        assert_eq!(name(choose(&[browser(1)], None, false, false, true, "k")), "browser");
        let err = choose(&[Login::GithubAppManifest {}], None, false, true, true, "k").unwrap_err().to_string();
        assert!(err.contains("by github-app-manifest: not supported yet! open an issue in"), "{err}");
        let err = choose(&[], None, false, true, true, "k").unwrap_err().to_string();
        assert!(err.contains("no `inject` slot"), "{err}");
    }

    #[test]
    fn a_login_naming_no_app_needs_a_client_id() {
        let paste = Login::Paste { hint: None, create: None };
        let gh = [paste.clone(), home_device(1)];
        let name = |l: Result<&Login>| method_name(l.unwrap());
        assert_eq!(name(choose(&gh, None, false, true, true, "k")), "paste", "skipped by default without one");
        assert_eq!(name(choose(&gh, None, true, true, true, "k")), "device");
        assert_eq!(name(choose(&gh, Some("device"), true, false, false, "k")), "device");
        let err = choose(&gh, Some("device"), false, true, true, "k").unwrap_err().to_string();
        assert!(err.contains("device login names no OAuth app: pass --client-id"), "{err}");
        let err = choose(&[home_device(1)], None, false, true, true, "k").unwrap_err().to_string();
        assert!(err.contains("pass --client-id"), "{err}");

        let data = temp("no-client-id");
        let mut c = other("github");
        c.pinned.login = gh.to_vec();
        let a = Args { yes: true, method: Some("device".into()), ..args("gh") };
        let err = login_at(&data, &a, Some(vec![c]), &mut Script::new(true, &[], None)).unwrap_err().to_string();
        assert!(err.contains("pass --client-id"), "refused before any network: {err}");
        assert!(!home::path(&data).exists() && !Store::in_data(&data).path().exists());
    }

    fn flows(open: impl FnMut(&str) + 'static, clock: Box<dyn Clock>) -> Flows {
        Flows {
            http: Box::new(|| Ok(crate::credentials::oauth::tests::http())),
            clock,
            browser: true,
            open: Box::new(open),
            browser_timeout: Duration::from_secs(20),
            // Nothing listens there: the check fails fast, a warning.
            authority: Box::new(|_| "127.0.0.1:1".into()),
        }
    }

    /// The device flow against a local provider, with the home's own app
    /// for a claim naming none; a re-login by paste then replaces the
    /// record and nothing else.
    #[test]
    fn device_login_stores_the_token_set_under_the_home_client_id() {
        use crate::credentials::oauth::tests::{FakeClock, UNIX, device_provider, field, form};
        let (port, seen) = device_provider();
        let data = temp("device");
        let mut gh = other("github");
        gh.pinned.login = vec![Login::Paste { hint: None, create: None }, home_device(port)];
        let a = Args { yes: true, client_id: Some("Iv1.mine".into()), ..args("gh") };
        let mut term = Script::new(true, &[], None);
        let mut f = flows(|_| {}, Box::new(FakeClock::new()));
        let done = super::login_at(&data, &a, Some(vec![gh.clone()]), &mut term, &mut f).unwrap();
        assert!(done.contains("approved"), "{done}");
        assert!(term.shown.contains("enter the code WDJB-MJHT"), "{}", term.shown);
        let Some(Record::OAuth(rec)) = Store::in_data(&data).get("gh").unwrap() else { panic!("an OAuth record") };
        assert_eq!((rec.access_token.expose(), rec.client_id.as_str()), ("ghu_live", "Iv1.mine"));
        assert_eq!(rec.expires_at, Some(UNIX + 28800));
        assert_eq!(rec.token_url, format!("https://127.0.0.1:{port}/token"));
        assert_eq!(field(&form(&seen.lock().unwrap()[0].1), "client_id"), Some("Iv1.mine"));
        let entry = home::load(&home::path(&data)).unwrap()["gh"].clone();
        assert!(!format!("{entry:?}").contains("Iv1.mine"), "the client id is the record's, not the claim's");

        let pasted = Args { method: Some("paste".into()), ..args("gh") };
        let done = login_at(&data, &pasted, None, &mut Script::new(false, &[], Some("pat"))).unwrap();
        assert!(done.ends_with("secret replaced"), "{done}");
        assert_eq!(Store::in_data(&data).get("gh").unwrap(), Some(Record::token(Secret::new("pat".into()))));
        assert_eq!(home::load(&home::path(&data)).unwrap()["gh"], entry);
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn only_accepted_tokens_are_stored_and_a_login_clears_the_expiry() {
        use crate::credentials::home::tests::github;
        let data = temp("accept");
        let yes = Args { yes: true, ..args("gh") };
        for bad in ["ghp_classic", "gho_oauth"] {
            let err = login_at(&data, &yes, Some(vec![github()]), &mut Script::new(false, &[], Some(bad))).unwrap_err();
            assert!(err.to_string().contains(&format!("not `{}…`", &bad[..4])), "{err}");
        }
        assert!(!home::path(&data).exists() && !Store::in_data(&data).path().exists());
        login_at(&data, &yes, Some(vec![github()]), &mut Script::new(false, &[], Some("github_pat_11A\n"))).unwrap();
        home::set_expires_at(&home::path(&data), "gh", 1_800_000_000).unwrap();
        login_at(&data, &args("gh"), None, &mut Script::new(false, &[], Some("ghu_x"))).unwrap();
        assert_eq!(home::load(&home::path(&data)).unwrap()["gh"].expires_at, None);
        assert_eq!(Store::in_data(&data).token_now("gh").unwrap().unwrap().expose(), "ghu_x");
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn an_oauth_token_of_another_kind_is_refused() {
        use crate::credentials::oauth::tests::{FakeClock, device_provider};
        let (port, _) = device_provider();
        let data = temp("accept-oauth");
        let mut gh = other("github");
        gh.pinned.login = vec![device(port)];
        gh.pinned.tokens = Some(claim::Tokens { as_: vec![], accept: vec!["github_pat_".into()], expiry: None });
        let mut f = flows(|_| {}, Box::new(FakeClock::new()));
        let a = Args { yes: true, ..args("gh") };
        let err = super::login_at(&data, &a, Some(vec![gh]), &mut Script::new(true, &[], None), &mut f).unwrap_err();
        assert!(err.to_string().contains("takes only `github_pat_…` tokens, not `ghu_…`"), "{err}");
        assert!(!home::path(&data).exists() && !Store::in_data(&data).path().exists());
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn the_token_page_prefills_the_granted_permissions_at_their_highest_level() {
        let scope = |p: &[(&str, Level)]| Scope {
            routes: vec![],
            permissions: p.iter().map(|(k, l)| (k.to_string(), *l)).collect(),
        };
        let scopes: BTreeMap<String, Scope> = [
            ("a", scope(&[("contents", Level::Read), ("issues", Level::Write)])),
            ("b", scope(&[("contents", Level::Write), ("metadata", Level::Read)])),
            ("c", scope(&[("administration", Level::Admin), ("issues", Level::Read)])),
            ("d", scope(&[])),
        ]
        .into_iter()
        .map(|(k, s)| (k.to_string(), s))
        .collect();
        let create = Create { url: "https://github.com/settings/personal-access-tokens/new".into(), permissions: true };
        let url = create_url(&create, &scopes, None, "gh-work", Some(&["work".into(), "oss".into()]), Some("acme"));
        assert_eq!(
            url,
            "https://github.com/settings/personal-access-tokens/new?name=devsandbox%20gh-work\
             &description=devsandbox%20credential%20gh-work%2C%20profiles%3A%20work%2C%20oss&target_name=acme\
             &administration=admin&contents=write&issues=write&metadata=read"
        );
        let max = ["a".to_string(), "d".into()];
        let url = create_url(&create, &scopes, Some(&max), "gh", None, None);
        assert!(url.ends_with("?name=devsandbox%20gh&description=devsandbox%20credential%20gh%2C%20every%20profile&contents=read&issues=write"), "{url}");
        let bare = Create { permissions: false, ..create };
        let url = create_url(&bare, &scopes, None, "gh", None, None);
        assert!(url.ends_with("%20every%20profile"), "{url}");
        let long = "x".repeat(64);
        let url = create_url(&bare, &scopes, None, &long, None, None);
        assert!(url.contains(&format!("?name=devsandbox%20{}&", "x".repeat(29))), "40 chars: {url}");
    }

    #[test]
    fn a_paste_login_opens_its_token_page_on_a_terminal() {
        use crate::credentials::home::tests::github;
        use std::cell::RefCell;
        use std::rc::Rc;
        let data = temp("create");
        let opened = Rc::new(RefCell::new(Vec::<String>::new()));
        let mut f = offline();
        let seen = opened.clone();
        f.open = Box::new(move |url| seen.borrow_mut().push(url.to_string()));
        let a = Args { yes: true, owner: Some("acme".into()), method: Some("paste".into()), ..args("gh") };
        let mut term = Script::new(true, &[], Some("github_pat_1"));
        super::login_at(&data, &a, Some(vec![github()]), &mut term, &mut f).unwrap();
        let url = opened.borrow_mut().pop().expect("the page opened");
        assert!(url.starts_with("https://github.com/settings/personal-access-tokens/new?name=devsandbox%20gh&"), "{url}");
        assert!(url.contains("&target_name=acme&") && url.contains("&contents=write"), "{url}");
        assert!(term.shown.contains(&format!("Create the token at {url}\n")), "{}", term.shown);
        assert!(term.shown.contains("check its resource owner is acme"), "{}", term.shown);

        // The entry's `max` narrows the permissions; piped, nothing opens.
        home::update(&home::path(&data), |h| {
            h.get_mut("gh").unwrap().max = Some(vec!["user.read".into()]);
            Ok(())
        })
        .unwrap();
        let a = Args { method: Some("paste".into()), ..args("gh") };
        let mut term = Script::new(true, &[], Some("github_pat_2"));
        super::login_at(&data, &a, None, &mut term, &mut f).unwrap();
        let url = opened.borrow_mut().pop().expect("the page opened");
        assert!(url.ends_with("%20every%20profile"), "no permission asked: {url}");
        let mut term = Script::new(false, &[], Some("github_pat_3"));
        super::login_at(&data, &a, None, &mut term, &mut f).unwrap();
        assert!(opened.borrow().is_empty() && term.shown.starts_with("warning: cannot check"), "{}", term.shown);

        let owner = Args { yes: true, owner: Some("acme".into()), ..args("s") };
        let err = login_at(&data, &owner, Some(vec![sentry()]), &mut Script::new(false, &[], Some("t"))).unwrap_err();
        assert!(err.to_string().contains("--owner is for a paste login that opens a token page"), "{err}");
        let blank = Args { owner: Some(" ".into()), ..a };
        let err = login_at(&data, &blank, None, &mut Script::new(false, &[], Some("t"))).unwrap_err();
        assert!(err.to_string().contains("--owner must be non-empty"), "{err}");
        let _ = std::fs::remove_dir_all(&data);
    }

    const EXPIRY: &str = "2026-12-01 10:00:00 UTC";

    /// Offline flows whose claim upstreams are a local server answering
    /// `answer` to every request.
    fn checked_by(answer: Vec<u8>) -> (Flows, crate::broker::serve::tests::Seen) {
        let (port, seen) = crate::broker::serve::tests::upstream_server(move |_, _| answer.clone());
        let mut f = offline();
        f.http = Box::new(|| Ok(crate::credentials::oauth::tests::http()));
        f.authority = Box::new(move |_| format!("127.0.0.1:{port}"));
        (f, seen)
    }

    fn user(status: u16, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\ngithub-authentication-token-expiration: {EXPIRY}\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    #[test]
    fn login_checks_the_token_and_says_whose_it_is() {
        use crate::credentials::home::tests::github;
        let data = temp("check-ok");
        let (mut f, seen) = checked_by(user(200, r#"{"login":"alice","id":1}"#));
        let a = Args { yes: true, ..args("gh") };
        let mut term = Script::new(false, &[], Some("github_pat_1"));
        let done = super::login_at(&data, &a, Some(vec![github()]), &mut term, &mut f).unwrap();
        assert!(done.starts_with("logged in `gh` (github) as alice; claim version"), "{done}");
        assert!(!term.shown.contains("warning"), "{}", term.shown);
        let (head, _) = seen.lock().unwrap()[0].clone();
        assert!(head.starts_with("GET /user HTTP/1.1\r\n"), "{head}");
        assert!(head.contains("\r\nauthorization: token github_pat_1\r\n"), "{head}");
        let e = &home::load(&home::path(&data)).unwrap()["gh"];
        assert_eq!(e.expires(), crate::credentials::tokens::parse_expiry(EXPIRY));
        assert!(e.expires().is_some());
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn a_refused_token_is_not_stored() {
        use crate::credentials::home::tests::github;
        let data = temp("check-refused");
        let a = Args { yes: true, ..args("gh") };
        let (mut f, _) = checked_by(user(401, r#"{"message":"Bad credentials"}"#));
        let mut term = Script::new(false, &[], Some("github_pat_1"));
        let err = super::login_at(&data, &a, Some(vec![github()]), &mut term, &mut f).unwrap_err().to_string();
        assert_eq!(err, "the token was refused by api.github.com (401): nothing changed");
        assert!(!home::path(&data).exists() && !Store::in_data(&data).path().exists());
    }

    #[test]
    fn the_check_goes_to_the_first_upstream_declared() {
        let data = temp("check-host");
        let (port, seen) = crate::broker::serve::tests::upstream_server(|_, _| user(200, "{}"));
        let mut f = offline();
        f.http = Box::new(|| Ok(crate::credentials::oauth::tests::http()));
        let hosts = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        let asked = hosts.clone();
        f.authority = Box::new(move |h| {
            asked.borrow_mut().push(h.to_string());
            format!("127.0.0.1:{port}")
        });
        let a = Args { yes: true, ..args("s") };
        let mut term = Script::new(false, &[], Some("sntryu_1"));
        super::login_at(&data, &a, Some(vec![sentry()]), &mut term, &mut f).unwrap();
        assert_eq!(*hosts.borrow(), ["sentry.io"]);
        let (head, _) = seen.lock().unwrap()[0].clone();
        assert!(head.starts_with("GET /api/0/ HTTP/1.1\r\n"), "{head}");
        assert!(head.contains("\r\nauthorization: Bearer sntryu_1\r\n"), "{head}");
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn an_unchecked_token_is_stored_with_a_warning() {
        use crate::credentials::home::tests::github;
        let data = temp("check-unknown");
        let (mut f, _) = checked_by(user(403, r#"{"message":"API rate limit exceeded"}"#));
        let a = Args { yes: true, ..args("gh") };
        let mut term = Script::new(false, &[], Some("github_pat_1"));
        let done = super::login_at(&data, &a, Some(vec![github()]), &mut term, &mut f).unwrap();
        assert!(done.starts_with("logged in `gh` (github); claim version"), "{done}");
        assert!(term.shown.ends_with("\nwarning: cannot check the token, stored unchecked: it answered 403\n"), "{}", term.shown);
        assert_eq!(Store::in_data(&data).token_now("gh").unwrap().unwrap().expose(), "github_pat_1");
        assert_eq!(home::load(&home::path(&data)).unwrap()["gh"].expires_at, None);

        let mut term = Script::new(false, &[], Some("github_pat_2"));
        let done = super::login_at(&data, &args("gh"), None, &mut term, &mut offline()).unwrap();
        assert!(done.ends_with("secret replaced"), "{done}");
        assert!(term.shown.contains("stored unchecked: no network in this test"), "{}", term.shown);
        assert_eq!(Store::in_data(&data).token_now("gh").unwrap().unwrap().expose(), "github_pat_2");
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn the_check_puts_the_token_in_the_claim_s_slot() {
        let mut p = sentry().pinned;
        let host = |h: &str| format!("{h}:8443");
        let (method, url, headers, private) = check_request(&p, "t", &host).unwrap();
        assert_eq!((method, url.as_str(), private), ("GET", "https://sentry.io:8443/api/0/", false));
        assert_eq!(headers, [Header { name: "authorization".into(), value: b"Bearer t".to_vec() }]);
        p.inject = vec![Slot::Query { param: "key".into() }];
        p.upstreams[0].private = true;
        let (_, url, headers, private) = check_request(&p, "a b", &host).unwrap();
        assert_eq!((url.as_str(), private), ("https://sentry.io:8443/api/0/?key=a%20b", true));
        assert!(headers.is_empty());
        p.check = None;
        assert!(check_request(&p, "t", &host).is_none());
    }

    #[test]
    fn the_check_s_answer_reads_whose_and_until_when() {
        use crate::credentials::home::tests::github;
        let p = github().pinned;
        let expiry = [Header { name: "GitHub-Authentication-Token-Expiration".into(), value: EXPIRY.into() }];
        let valid = |shows: Option<&str>, expires_at| Checked::Valid { shows: shows.map(String::from), expires_at };
        let at = crate::credentials::tokens::parse_expiry(EXPIRY);
        assert_eq!(read_check(&p, 200, &expiry, br#"{"login":"alice"}"#), valid(Some("alice"), at));
        assert_eq!(read_check(&p, 204, &[], b""), valid(None, None));
        assert_eq!(read_check(&p, 200, &[], br#"{"login":7}"#), valid(None, None));
        assert_eq!(read_check(&p, 200, &[], b"not json"), valid(None, None));
        assert_eq!(read_check(&p, 401, &expiry, b""), Checked::Refused);
        assert_eq!(read_check(&p, 403, &[], b""), Checked::Unknown("it answered 403".into()));
        assert_eq!(read_check(&p, 500, &[], b""), Checked::Unknown("it answered 500".into()));
        assert_eq!(read_check(&p, 404, &[], b""), Checked::Unknown("it answered 404".into()));
        let mut quiet = p.clone();
        quiet.check_shows = None;
        assert_eq!(read_check(&quiet, 200, &[], br#"{"login":"alice"}"#), valid(None, None));
    }

    #[test]
    fn browser_login_stores_the_token_set() {
        use crate::credentials::oauth::tests::{browser_opener, code_provider};
        let challenge = std::sync::Arc::new(std::sync::Mutex::new(None));
        let (port, _) = code_provider(challenge.clone());
        let data = temp("browser");
        let mut gh = other("github");
        gh.pinned.login = vec![browser(port)];
        let a = Args { yes: true, ..args("gh") };
        let mut f = flows(browser_opener(challenge), Box::new(oauth::SystemClock::new()));
        super::login_at(&data, &a, Some(vec![gh]), &mut Script::new(true, &[], None), &mut f).unwrap();
        let Some(Record::OAuth(rec)) = Store::in_data(&data).get("gh").unwrap() else { panic!("an OAuth record") };
        assert_eq!((rec.access_token.expose(), rec.client_id.as_str()), ("ghu_live", "Iv1.claims"));
        assert_eq!(rec.refresh_token.unwrap().expose(), "ghr_live");
        let _ = std::fs::remove_dir_all(&data);
    }
}
