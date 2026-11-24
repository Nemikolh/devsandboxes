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

use crate::ask::{Answer, Choice, Node, Style};
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
    /// A broad credential: its token may be one of the claim's
    /// `tokens.broad`, reaching every repo the account can.
    pub broad: bool,
    /// The orgs a new credential reaches, by slug, for a claim with an org
    /// fence.
    pub orgs: Vec<String>,
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
    /// Pages open in `profile`'s browser where `profiles.toml` maps it, as
    /// container URLs do (`open::open_argv`), else with the system opener.
    fn host(profile: Option<String>, term: &mut dyn Term) -> Flows {
        let os = std::env::consts::OS;
        let profiles = match &profile {
            Some(p) => crate::open::profiles_path().and_then(|path| crate::open::load_profiles(&path)).unwrap_or_else(|e| {
                let e = crate::open::printable(&format!("{e:#}"));
                term.show(&format!("warning: {e}; opening pages for profile `{p}` with the system opener\n"));
                crate::open::HostProfiles::new()
            }),
            None => crate::open::HostProfiles::new(),
        };
        let argv = move |url: &str| {
            crate::open::open_argv(os, profile.as_deref(), &profiles, url)
                .filter(|_| crate::open::has_display(os, |k| std::env::var_os(k)))
        };
        Flows {
            http: Box::new(Http::from_host),
            clock: Box::new(oauth::SystemClock::new()),
            browser: argv("").is_some(),
            open: Box::new(move |url| {
                if let Some(argv) = argv(url) {
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
    fn choose(&mut self, question: &str, choices: &[Choice], default: usize) -> Result<Answer>;
    /// `question` under `tree`, the details it is about.
    fn review(&mut self, tree: Vec<Node>, question: &str, choices: &[Choice], default: usize) -> Result<Answer>;
    /// The secret from stdin: unechoed on a terminal, else all of it.
    fn secret(&mut self, prompt: &str) -> Result<Secret>;
}

pub(crate) struct Stdio;

impl Term for Stdio {
    fn tty(&self) -> bool {
        std::io::stdin().is_terminal()
    }

    fn show(&mut self, text: &str) {
        eprint!("{}", styled(text, Style::detect()));
    }

    fn choose(&mut self, question: &str, choices: &[Choice], default: usize) -> Result<Answer> {
        crate::ask::select(question, choices, default)
    }

    fn review(&mut self, tree: Vec<Node>, question: &str, choices: &[Choice], default: usize) -> Result<Answer> {
        crate::ask::review(tree, question, choices, default)
    }

    fn secret(&mut self, prompt: &str) -> Result<Secret> {
        let mut text = String::new();
        if self.tty() {
            let style = Style::detect();
            eprint!("{} {}: ", style.accent("?"), style.heading(prompt));
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

/// `text`'s lines styled by what they start with. By prefix so the callers
/// stay one plain `show`, which the tests' `Script` records as is.
fn styled(text: &str, style: Style) -> String {
    const CREATE: &str = "Create the token at ";
    text.split_inclusive('\n')
        .map(|line| {
            if let Some(rest) = line.strip_prefix("warning:") {
                format!("{}{rest}", style.warn("warning:"))
            } else if let Some(rest) = line.strip_prefix("note:") {
                format!("{}{rest}", style.note("note:"))
            } else if line.starts_with("approved claim version ") {
                let body = line.trim_end_matches('\n');
                format!("{}{}", style.ok(body), &line[body.len()..])
            } else if let Some(rest) = line.strip_prefix(CREATE) {
                let url = rest.trim_end_matches('\n');
                format!("{CREATE}{}{}", style.heading(url), &rest[url.len()..])
            } else {
                line.to_string()
            }
        })
        .collect()
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
    let data = data_dir()?;
    let existing = home::load(&home::path(&data))?.remove(&args.credential);
    let profile = login_profile(existing.as_ref(), &args.profiles);
    let mut flows = Flows::host(profile, &mut Stdio);
    let done = login_at(&data, &args, claims, &mut Stdio, &mut flows)?;
    println!("{}", crate::render::green(&done));
    Ok(())
}

/// The profile whose browser login pages open in: the credential's only
/// profile, the existing `entry`'s, else among the `--profile` `flags` of a
/// new one. Several or every profile: none.
fn login_profile(entry: Option<&Entry>, flags: &[String]) -> Option<String> {
    let profiles = match entry {
        Some(e) => e.profiles.as_deref()?,
        None => flags,
    };
    match profiles {
        [one] => Some(one.clone()),
        _ => None,
    }
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

/// A `broad` login only with `broad` (`--broad`), which takes no other but
/// paste.
fn fits(login: &Login, broad: bool) -> bool {
    login.broad() == broad || matches!(login, Login::Paste { .. })
}

const TTY_ORDER: [&str; 3] = ["device", "browser", "paste"];
const PIPED_ORDER: [&str; 3] = ["paste", "device", "browser"];

/// The first login of each method in `order` that works here: browser
/// only where a browser opens, a login needing `--client-id` only with one.
fn usable<'a>(logins: &'a [Login], order: &[&str], client_id: bool, broad: bool, browser: bool) -> Vec<&'a Login> {
    order
        .iter()
        .filter(|m| browser || **m != "browser")
        .filter_map(|m| logins.iter().find(|l| fits(l, broad) && method_name(l) == *m))
        .filter(|l| client_id || !needs_client_id(l))
        .collect()
}

/// The login method: `wanted` if the claim lists it, else the first of
/// the claim's that works here ([`usable`]), in this order: off a terminal
/// paste (stdin), device, browser; on one device, browser, paste. A claim's
/// own order isn't kept (its version sorts them).
fn choose<'a>(
    logins: &'a [Login],
    wanted: Option<&str>,
    client_id: bool,
    broad: bool,
    tty: bool,
    browser: bool,
    kind: &str,
) -> Result<&'a Login> {
    if logins.is_empty() {
        bail!("the `{kind}` claim has no secret to log in (no `inject` slot)");
    }
    let fit = |l: &&Login| fits(l, broad);
    let listed = || {
        let mut names: Vec<&str> = logins.iter().filter(fit).map(method_name).collect();
        names.dedup();
        names.join(", ")
    };
    let find = |m: &str| logins.iter().filter(fit).find(|l| method_name(l) == m);
    if let Some(m) = wanted {
        let l = find(m).with_context(|| match logins.iter().any(|l| method_name(l) == m) {
            true if broad => format!("the `{kind}` claim's {m} login isn't broad: drop --broad, or log in by {}", listed()),
            true => format!(
                "the `{kind}` claim's {m} login is broad (its token reaches every repo your account can access): \
                 pass --broad for a new broad credential"
            ),
            false => format!("the `{kind}` claim logs in by {}, not {m}", listed()),
        })?;
        if needs_client_id(l) && !client_id {
            return Err(client_id_required(l, kind));
        }
        return Ok(l);
    }
    let order: &[&str] = if tty { &TTY_ORDER } else { &PIPED_ORDER };
    if let Some(l) = usable(logins, order, client_id, broad, browser).first() {
        return Ok(l);
    }
    if let Some(l) = order.iter().filter_map(|m| find(m)).find(|l| needs_client_id(l) && !client_id) {
        return Err(client_id_required(l, kind));
    }
    if find("browser").is_some() {
        bail!("the `{kind}` claim logs in by {}: no browser opens here (pass --method browser to try anyway)", listed());
    }
    if broad {
        bail!("the `{kind}` claim has no broad login");
    }
    if logins.iter().any(Login::broad) && listed().is_empty() {
        bail!("the `{kind}` claim's only logins are broad (their token reaches every repo your account can access): pass --broad");
    }
    bail!("the `{kind}` claim logs in by {}: {}", listed(), crate::NOT_SUPPORTED_YET)
}

/// What a terminal's method question offers: [`usable`] in its terminal
/// order, less what the flags rule out, so a pick never fails their
/// checks: `--client-id` paste, `--owner` all but a paste with a token
/// page.
fn offered(logins: &[Login], client_id: bool, broad: bool, browser: bool, owner: bool) -> Vec<&Login> {
    let page = |l: &Login| matches!(l, Login::Paste { create: Some(_), .. }) && !broad;
    let mut offered = usable(logins, &TTY_ORDER, client_id, broad, browser);
    offered.retain(|l| !(client_id && matches!(l, Login::Paste { .. })) && (!owner || page(l)));
    offered
}

fn method_choice(login: &Login) -> Choice {
    let hint = match login {
        Login::Device { .. } => "enter a code on the provider's page".to_string(),
        Login::Browser { .. } => "sign in in this host's browser".to_string(),
        Login::Paste { hint: Some(hint), .. } => format!("paste {hint}"),
        Login::Paste { hint: None, .. } => "paste a token".to_string(),
        Login::GithubAppManifest {} => unreachable!("never usable"),
    };
    let name = method_name(login);
    Choice::new(name).hint(crate::open::printable(&hint)).hotkey(name.chars().next().expect("a name"))
}

/// `picked` unless `offered` has another to pick, then the user's pick,
/// `picked` preselected.
fn pick_method<'a>(term: &mut dyn Term, name: &str, offered: &[&'a Login], picked: &'a Login) -> Result<&'a Login> {
    if offered.len() < 2 {
        return Ok(picked);
    }
    let choices: Vec<Choice> = offered.iter().map(|l| method_choice(l)).collect();
    let default = offered.iter().position(|l| std::ptr::eq(*l, picked)).unwrap_or(0);
    match term.choose(&format!("Log in `{name}` by"), &choices, default)? {
        Answer::Picked(i) => Ok(offered[i]),
        Answer::Cancelled => bail!("no login method picked: nothing changed"),
    }
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

/// `--org`'s slugs, deduped case-insensitively (the fence matches them so),
/// the first spelling kept. An all-digit value is refused: the fence reads
/// one as an id, never a slug.
fn org_slugs(flags: &[String]) -> Result<Vec<String>> {
    let mut slugs: Vec<String> = Vec::new();
    for slug in flags {
        let segment = !slug.is_empty() && slug.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'));
        if !segment {
            bail!("--org `{}`: a slug is one segment of letters, digits, `_` and `-`", crate::open::printable(slug));
        }
        if crate::broker::orgs::is_id(slug) {
            bail!("--org `{slug}`: that's an org id; name the org by its slug");
        }
        if !slugs.iter().any(|s| s.eq_ignore_ascii_case(slug)) {
            slugs.push(slug.clone());
        }
    }
    Ok(slugs)
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
        Login::Device { client_id, device_url, token_url, scope, .. } => {
            let http = (flows.http)()?;
            let client_id = client(client_id);
            let at = device::Endpoints { client_id: &client_id, device_url, token_url, scope: scope.as_deref() };
            let mut ui = TermUi { term, open: &mut *flows.open };
            Ok(Record::OAuth(device::login(&http, &at, &mut *flows.clock, &mut ui)?))
        }
        Login::Browser { client_id, authorize_url, token_url, scope, .. } => {
            let http = (flows.http)()?;
            let client_id = client(client_id);
            let at = browser::Endpoints { client_id: &client_id, authorize_url, token_url, scope: scope.as_deref() };
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
    let (url, headers, private) = credentialed(pinned, path, token, authority)?;
    Some((method.as_str(), url, headers, private))
}

/// A call to `path` on the claim's first upstream at `authority` with
/// `token` in its first slot: URL, headers, whether that upstream is
/// private. `None`: no slot or no upstream.
fn credentialed(pinned: &Pinned, path: &str, token: &str, authority: &dyn Fn(&str) -> String) -> Option<(String, Vec<Header>, bool)> {
    let slot = pinned.inject.first()?;
    let upstream = pinned.upstreams.first()?;
    let mut path = path.to_string();
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
    Some((url, headers, upstream.private))
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

/// What the claim's `orgs.resolve` said of an org with the token.
#[derive(Debug, PartialEq)]
enum OrgChecked {
    Reachable,
    /// A 403 or 404: the account isn't in it, or the token doesn't reach it.
    Unreachable(u16),
    /// A 401, as [`Checked::Refused`].
    Refused,
    Unknown(String),
}

/// A 2xx counts only with an id in `orgs.id`: that's what the broker's
/// fence later learns from the same route.
fn read_org(orgs: &claim::Orgs, status: u16, body: &[u8]) -> OrgChecked {
    match status {
        401 => OrgChecked::Refused,
        403 | 404 => OrgChecked::Unreachable(status),
        200..=299 if crate::broker::orgs::parse_id(body, &orgs.id).is_some() => OrgChecked::Reachable,
        200..=299 => OrgChecked::Unknown(format!("its answer names no `{}`", orgs.id)),
        _ => OrgChecked::Unknown(format!("it answered {status}")),
    }
}

/// `orgs.resolve` for `slug`, sent as [`check_token`] sends the check.
fn check_org(pinned: &Pinned, orgs: &claim::Orgs, slug: &str, token: &str, flows: &Flows) -> OrgChecked {
    let Some(path) = crate::broker::orgs::resolve_path(orgs, slug) else {
        unreachable!("org_slugs keeps only one-segment slugs")
    };
    let Some((url, headers, private)) = credentialed(pinned, &path, token, &*flows.authority) else {
        return OrgChecked::Unknown("the claim has no upstream or `inject` slot".into());
    };
    match (flows.http)().and_then(|http| http.fetch("GET", &url, &headers, private)) {
        Ok((status, _, body)) => read_org(orgs, status, &body),
        Err(e) => OrgChecked::Unknown(format!("{e:#}")),
    }
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
    let orgs = org_slugs(&args.orgs)?;
    if existing.is_some() && !orgs.is_empty() {
        bail!("credential `{name}` exists: its orgs are `orgs` in {}", path.display());
    }
    match &existing {
        Some(e) if e.broad && !args.broad => bail!(
            "credential `{name}` is broad (`broad` in {}): its token reaches every repo your account can access; \
             log in to it with --broad",
            path.display()
        ),
        Some(e) if !e.broad && args.broad => bail!(
            "credential `{name}` exists and isn't broad: --broad makes a new credential (`broad` in {})",
            path.display()
        ),
        _ => {}
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
    if args.broad && pinned.tokens.as_ref().is_none_or(|t| t.broad.is_empty()) {
        bail!("the `{kind}` claim takes no broad token (no `tokens.broad`): --broad changes nothing");
    }
    if !orgs.is_empty() && pinned.orgs.is_none() {
        bail!("the `{kind}` claim has no org fence: --org changes nothing");
    }
    let login =
        choose(&pinned.login, args.method.as_deref(), args.client_id.is_some(), args.broad, term.tty(), flows.browser, &kind)?;
    if let Some(id) = &args.client_id {
        if matches!(login, Login::Paste { .. }) {
            bail!("--client-id is for the device and browser logins, not paste");
        }
        check_client_id(id)?;
    }
    let page_of = |login: &Login| match login {
        // Its page makes a narrow token; a broad paste is a token at hand
        // (`gh auth token`).
        Login::Paste { create: Some(create), .. } if !args.broad => {
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
        if page_of(login).is_none() {
            bail!("--owner is for a paste login that opens a token page; the `{kind}` claim's {} login has none", method_name(login));
        }
        check_owner(owner)?;
    }
    let approving = candidate
        .as_ref()
        .filter(|c| existing.as_ref().and_then(|e| e.approved_claim(&c.version)).is_none());
    if let Some(c) = approving {
        approve_or_refuse(term, args.yes, shown.fold(c.short()), &format!("Approve this claim for `{name}`?"))?;
    }
    // A broad entry's reach was agreed when it was made.
    if args.broad && existing.is_none() {
        term.show(&format!(
            "warning: `{name}` is broad: its token reaches every repo your account can access. The broker refuses writes \
             outside an instance's repo fence; GraphQL reads aren't fenced.\n"
        ));
        approve_or_refuse(term, args.yes, Vec::new(), &format!("Make `{name}` a broad credential?"))?;
    }
    // Asked after the approvals: a refused claim never asks for a method.
    // `--yes` approves; it doesn't pick.
    let login = match &args.method {
        None if term.tty() => {
            let offered = offered(&pinned.login, args.client_id.is_some(), args.broad, flows.browser, args.owner.is_some());
            pick_method(term, name, &offered, login)?
        }
        _ => login,
    };
    let page = page_of(login);

    let record = obtain(login, name, page.as_deref(), args, term, flows)?;
    crate::credentials::tokens::check_accepted(pinned.tokens.as_ref(), record.access_token().expose(), &kind, args.broad)?;
    let host = pinned.upstreams.first().map_or("", |u| u.host.as_str());
    let (whose, expires_at) = match check_token(&pinned, record.access_token().expose(), flows) {
        None => (None, None),
        Some(Checked::Refused) => bail!("the token was refused by {host} (401): nothing changed"),
        // A home may be offline.
        Some(Checked::Unknown(why)) => {
            term.show(&format!("warning: cannot check the token, stored unchecked: {}\n", crate::open::printable(&why)));
            (None, None)
        }
        // An OAuth record carries its own expiry.
        Some(Checked::Valid { shows, expires_at }) => (shows, expires_at.filter(|_| matches!(record, Record::Static { .. }))),
    };
    // A slug the token can't reach would only ever be refused upstream.
    if let Some(fence) = &pinned.orgs {
        for slug in &orgs {
            match check_org(&pinned, fence, slug, record.access_token().expose(), flows) {
                OrgChecked::Reachable => {}
                OrgChecked::Unreachable(status) => bail!("org `{slug}` isn't reachable with this token ({status}): nothing changed"),
                OrgChecked::Refused => bail!("the token was refused by {host} (401): nothing changed"),
                OrgChecked::Unknown(why) => term.show(&format!(
                    "warning: cannot check org `{slug}`, stored unchecked: {}\n",
                    crate::open::printable(&why)
                )),
            }
        }
    }
    // The secret first: a claim approved without one serves nothing.
    Store::in_data(data).put(name, &record)?;
    let profiles = (!args.profiles.is_empty()).then(|| args.profiles.clone());
    let approved = home::update(&path, |h: &mut Home| {
        let entry =
            h.entry(name.to_string()).or_insert_with(|| Entry { broad: args.broad, orgs: orgs.clone(), ..Entry::new(&kind, profiles) });
        if entry.kind != kind {
            bail!("credential `{name}` is a `{}` credential, not `{kind}`", entry.kind);
        }
        entry.broken = None;
        entry.expires_at = expires_at.map(crate::inbox::rfc3339);
        Ok(approving.cloned().map(|c| entry.approve(c)).unwrap_or(false))
    })?;
    if existing.as_ref().is_none_or(|e| e.orgs.is_empty()) && orgs.is_empty() && pinned.orgs.is_some() {
        term.show(&format!("note: {}\n", no_orgs_note(name, &path)));
    }
    let whose = whose.map(|w| format!(" as {}", crate::open::printable(&w))).unwrap_or_default();
    Ok(match (approved, approving) {
        (true, Some(c)) => format!("logged in `{name}` ({kind}){whose}; claim version {} approved", c.short()),
        _ => format!("logged in `{name}` ({kind}){whose}: secret replaced"),
    })
}

/// Shown wherever a credential under an org fence ends up naming no orgs:
/// the broker refuses its org routes from then on.
pub(crate) fn no_orgs_note(name: &str, path: &Path) -> String {
    format!(
        "`{name}` names no orgs, so the broker refuses its org routes until `orgs` is set in {} \
         (or log in again as a new credential with --org)",
        path.display()
    )
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

/// `--yes` approves; else ask on a terminal, under `tree` when it has
/// one, and refuse off one. Unasked, `tree` is still shown: what `--yes`
/// approved, or what a later `--yes` would.
pub(crate) fn approve_or_refuse(term: &mut dyn Term, yes: bool, tree: Vec<Node>, question: &str) -> Result<()> {
    if yes || !term.tty() {
        let lines = crate::ask::fold::Fold::new(tree).plain_lines(Style::plain());
        term.show(&lines.iter().map(|l| format!("{l}\n")).collect::<String>());
        if yes {
            return Ok(());
        }
        bail!("no terminal to confirm on: pass --yes to approve");
    }
    if !approved(term, tree, question)? {
        bail!("not approved: nothing changed");
    }
    Ok(())
}

/// Approve or Refuse, Refuse preselected; a cancel refuses.
pub(crate) fn approved(term: &mut dyn Term, tree: Vec<Node>, question: &str) -> Result<bool> {
    let choices = [Choice::new("Approve").hotkey('a'), Choice::new("Refuse").hotkey('r')];
    let answer = if tree.is_empty() { term.choose(question, &choices, 1)? } else { term.review(tree, question, &choices, 1)? };
    Ok(answer == Answer::Picked(0))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::credentials::home::tests::sentry;
    use std::path::PathBuf;

    /// A scripted answer that cancels the question, as Esc does.
    pub(crate) const ESC: &str = "<esc>";

    /// A scripted terminal: answers in order, each the label of the choice
    /// picked (or [`ESC`]); everything shown kept, a review's tree as the
    /// plain fallback prints it, each question's choices and default in
    /// `asked`.
    pub(crate) struct Script {
        pub tty: bool,
        pub answers: Vec<String>,
        pub secret: Option<String>,
        pub shown: String,
        pub asked: Vec<(Vec<Choice>, usize)>,
    }

    impl Script {
        pub(crate) fn new(tty: bool, answers: &[&str], secret: Option<&str>) -> Script {
            Script {
                tty,
                answers: answers.iter().map(|a| a.to_string()).collect(),
                secret: secret.map(String::from),
                shown: String::new(),
                asked: Vec::new(),
            }
        }
    }

    impl Term for Script {
        fn tty(&self) -> bool {
            self.tty
        }
        fn show(&mut self, text: &str) {
            self.shown.push_str(text);
        }
        fn choose(&mut self, question: &str, choices: &[Choice], default: usize) -> Result<Answer> {
            assert!(default < choices.len(), "the default is a choice");
            self.shown.push_str(question);
            self.asked.push((choices.to_vec(), default));
            let answer = self.answers.remove(0);
            if answer == ESC {
                return Ok(Answer::Cancelled);
            }
            let labels: Vec<&str> = choices.iter().map(|c| c.label.as_str()).collect();
            let i = labels.iter().position(|l| *l == answer);
            Ok(Answer::Picked(i.unwrap_or_else(|| panic!("`{answer}` isn't one of {labels:?}"))))
        }
        fn review(&mut self, tree: Vec<Node>, question: &str, choices: &[Choice], default: usize) -> Result<Answer> {
            for line in crate::ask::fold::Fold::new(tree).plain_lines(Style::plain()) {
                self.shown.push_str(&line);
                self.shown.push('\n');
            }
            self.choose(question, choices, default)
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
            broad: false,
            orgs: Vec::new(),
        }
    }

    fn other(kind: &str) -> Claim {
        let mut c = sentry();
        c.kind = kind.into();
        c
    }

    #[test]
    fn pages_open_in_the_credentials_only_profile() {
        let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let entry = |p: Option<&[&str]>| Entry::new("github", p.map(names));
        assert_eq!(login_profile(None, &names(&["work"])), Some("work".into()));
        assert_eq!(login_profile(None, &names(&["work", "oss"])), None, "several");
        assert_eq!(login_profile(None, &[]), None, "every profile");
        assert_eq!(login_profile(Some(&entry(Some(&["work"]))), &[]), Some("work".into()));
        assert_eq!(login_profile(Some(&entry(Some(&["work", "oss"]))), &[]), None);
        assert_eq!(login_profile(Some(&entry(None)), &[]), None, "an entry for every profile");
        assert_eq!(login_profile(Some(&entry(Some(&["work"]))), &names(&["oss"])), Some("work".into()), "the entry's");
        assert_eq!(login_profile(Some(&entry(None)), &names(&["oss"])), None, "the entry's, every profile");
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
        a.method = Some("paste".into());
        let mut term = Script::new(true, &["Approve"], Some("tok-1\n"));
        let done = login_at(&data, &a, Some(vec![sentry()]), &mut term).unwrap();
        assert!(done.contains("claim version") && done.contains("approved"), "{done}");
        assert!(term.shown.contains("claim `sentry` (intercept)"), "{}", term.shown);
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
        assert!(term.shown.starts_with("warning: cannot check the token, stored unchecked: no network in this test\nnote: `sentry-work` names no orgs"), "{}", term.shown);
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
    fn shown_lines_are_styled_by_what_they_start_with() {
        let on = Style { on: true };
        let text = "warning: cannot check\nnote: x\nCreate the token at https://h/new\n  select\napproved claim version v\nplain";
        assert_eq!(
            styled(text, on),
            "\x1b[33mwarning:\x1b[0m cannot check\n\x1b[36mnote:\x1b[0m x\nCreate the token at \x1b[1mhttps://h/new\x1b[0m\n  select\n\
             \x1b[32mapproved claim version v\x1b[0m\nplain"
        );
        assert_eq!(styled(text, Style::plain()), text);
    }

    #[test]
    fn approval_picks_between_approve_and_refuse() {
        let mut term = Script::new(true, &["Approve", "Refuse", ESC], None);
        assert!(approved(&mut term, Vec::new(), "Q1?").unwrap());
        assert!(!approved(&mut term, vec![Node::new("detail")], "Q2?").unwrap());
        assert!(!approved(&mut term, Vec::new(), "Q3?").unwrap(), "a cancel refuses");
        assert_eq!(term.shown, "Q1?detail\nQ2?Q3?");
    }

    #[test]
    fn refusals_change_nothing() {
        let data = temp("refuse");
        let err = login_at(&data, &args("sentry-work"), None, &mut Script::new(true, &[], None)).unwrap_err();
        assert!(err.to_string().contains("--feature <ref>"), "{err}");
        let mut term = Script::new(false, &[], Some("t"));
        let err = login_at(&data, &args("s"), Some(vec![sentry()]), &mut term).unwrap_err();
        assert!(err.to_string().contains("pass --yes"), "{err}");
        assert!(term.shown.starts_with("claim `sentry` (intercept) (version "), "what --yes would approve: {}", term.shown);
        let err = login_at(&data, &args("s"), Some(vec![sentry()]), &mut Script::new(true, &["Refuse"], Some("t"))).unwrap_err();
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
            scope: None,
            broad: false,
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
        let mut term = Script::new(true, &["Approve"], Some("t"));
        let paste = Args { method: Some("paste".into()), ..args("s") };
        let done = login_at(&data, &paste, None, &mut term).unwrap();
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
            scope: None,
            broad: false,
        }
    }

    fn browser(port: u16) -> Login {
        Login::Browser {
            client_id: Some("Iv1.claims".into()),
            authorize_url: "https://github.example/login/oauth/authorize".into(),
            token_url: format!("https://127.0.0.1:{port}/token"),
            scope: None,
            broad: false,
        }
    }

    /// gh's own app's device login: a broad token, a scope.
    fn broad_device(port: u16) -> Login {
        let Login::Device { device_url, token_url, .. } = device(port) else { unreachable!() };
        Login::Device { client_id: Some("178c6fc778ccc68e1d6a".into()), device_url, token_url, scope: Some("repo read:org".into()), broad: true }
    }

    /// A device login whose claim leaves the OAuth app to the home.
    fn home_device(port: u16) -> Login {
        let Login::Device { device_url, token_url, .. } = device(port) else { unreachable!() };
        Login::Device { client_id: None, device_url, token_url, scope: None, broad: false }
    }

    #[test]
    fn the_method_is_asked_for_else_the_first_that_works_here() {
        let paste = Login::Paste { hint: None, create: None };
        let all = [paste.clone(), device(1), browser(1)];
        let name = |l: Result<&Login>| method_name(l.unwrap());
        assert_eq!(name(choose(&all, None, false, false, false, true, "k")), "paste");
        assert_eq!(name(choose(&all, None, false, false, true, true, "k")), "device");
        assert_eq!(name(choose(&[paste.clone(), browser(1)], None, false, false, true, true, "k")), "browser");
        assert_eq!(name(choose(&[paste.clone(), browser(1)], None, false, false, true, false, "k")), "paste");
        assert_eq!(name(choose(&[device(1)], None, false, false, false, false, "k")), "device");
        assert_eq!(name(choose(&all, Some("browser"), false, false, false, false, "k")), "browser");
        let err = choose(&[browser(1)], None, false, false, true, false, "k").unwrap_err().to_string();
        assert!(err.contains("no browser opens here"), "{err}");
        assert_eq!(name(choose(&[browser(1)], None, false, false, false, true, "k")), "browser");
        let err = choose(&[Login::GithubAppManifest {}], None, false, false, true, true, "k").unwrap_err().to_string();
        assert!(err.contains("by github-app-manifest: not supported yet! open an issue in"), "{err}");
        let err = choose(&[], None, false, false, true, true, "k").unwrap_err().to_string();
        assert!(err.contains("no `inject` slot"), "{err}");
    }

    #[test]
    fn a_login_naming_no_app_needs_a_client_id() {
        let paste = Login::Paste { hint: None, create: None };
        let gh = [paste.clone(), home_device(1)];
        let name = |l: Result<&Login>| method_name(l.unwrap());
        assert_eq!(name(choose(&gh, None, false, false, true, true, "k")), "paste", "skipped by default without one");
        assert_eq!(name(choose(&gh, None, true, false, true, true, "k")), "device");
        assert_eq!(name(choose(&gh, Some("device"), true, false, false, false, "k")), "device");
        let err = choose(&gh, Some("device"), false, false, true, true, "k").unwrap_err().to_string();
        assert!(err.contains("device login names no OAuth app: pass --client-id"), "{err}");
        let err = choose(&[home_device(1)], None, false, false, true, true, "k").unwrap_err().to_string();
        assert!(err.contains("pass --client-id"), "{err}");

        let data = temp("no-client-id");
        let mut c = other("github");
        c.pinned.login = gh.to_vec();
        let a = Args { yes: true, method: Some("device".into()), ..args("gh") };
        let err = login_at(&data, &a, Some(vec![c]), &mut Script::new(true, &[], None)).unwrap_err().to_string();
        assert!(err.contains("pass --client-id"), "refused before any network: {err}");
        assert!(!home::path(&data).exists() && !Store::in_data(&data).path().exists());
    }

    #[test]
    fn broad_logins_only_with_broad_which_takes_them_and_paste() {
        let paste = Login::Paste { hint: None, create: None };
        let gh = [paste.clone(), home_device(1), broad_device(1)];
        let name = |l: Result<&Login>| l.unwrap().clone();
        assert_eq!(name(choose(&gh, None, false, false, true, true, "k")), paste, "the narrow device needs a client id");
        assert_eq!(name(choose(&gh, None, true, false, true, true, "k")), home_device(1));
        assert_eq!(name(choose(&gh, None, false, true, true, true, "k")), broad_device(1));
        assert_eq!(name(choose(&gh, None, true, true, true, true, "k")), broad_device(1));
        assert_eq!(name(choose(&gh, None, false, true, false, true, "k")), paste, "off a terminal");
        assert_eq!(name(choose(&gh, Some("device"), false, true, false, true, "k")), broad_device(1));
        assert_eq!(name(choose(&gh, Some("paste"), false, true, true, true, "k")), paste);

        let err = choose(&[paste.clone(), broad_device(1)], Some("device"), false, false, true, true, "k").unwrap_err().to_string();
        assert!(err.contains("the `k` claim's device login is broad") && err.contains("pass --broad"), "{err}");
        let err = choose(&[paste.clone(), device(1)], Some("device"), false, true, true, true, "k").unwrap_err().to_string();
        assert!(err.contains("device login isn't broad: drop --broad, or log in by paste"), "{err}");
        let err = choose(&[device(1)], None, false, true, true, true, "k").unwrap_err().to_string();
        assert_eq!(err, "the `k` claim has no broad login");
        let err = choose(&[broad_device(1)], None, false, false, true, true, "k").unwrap_err().to_string();
        assert!(err.starts_with("the `k` claim's only logins are broad") && err.ends_with("pass --broad"), "{err}");
    }

    #[test]
    fn a_terminal_offers_each_usable_method_the_flags_allow() {
        let paste = Login::Paste { hint: None, create: None };
        let page = Login::Paste { hint: None, create: Some(Create { url: "https://h/new".into(), permissions: false }) };
        let all = [paste.clone(), device(1), browser(1), home_device(1), broad_device(1)];
        let names = |v: Vec<&Login>| v.into_iter().map(method_name).collect::<Vec<_>>();
        assert_eq!(names(offered(&all, false, false, true, false)), ["device", "browser", "paste"]);
        assert_eq!(names(offered(&all, false, false, false, false)), ["device", "paste"], "no browser opens");
        assert_eq!(names(offered(&all, true, false, true, false)), ["device", "browser"], "--client-id takes no paste");
        assert_eq!(names(offered(&[paste.clone(), home_device(1)], false, false, true, false)), ["paste"], "no app");
        assert_eq!(offered(&all, false, true, true, false), [&broad_device(1), &paste], "--broad");
        assert_eq!(names(offered(&all, false, false, true, true)), Vec::<&str>::new(), "--owner needs a token page");
        assert_eq!(offered(&[page.clone(), device(1)], false, false, true, true), [&page]);
        assert!(offered(&[page, device(1)], false, true, true, true).is_empty(), "a broad paste has no page");

        let hinted = Login::Paste { hint: Some("a PAT".into()), create: None };
        let hints: Vec<_> = [device(1), browser(1), paste, hinted].iter().map(method_choice).collect();
        let want = [
            ("device", "enter a code on the provider's page", 'd'),
            ("browser", "sign in in this host's browser", 'b'),
            ("paste", "paste a token", 'p'),
            ("paste", "paste a PAT", 'p'),
        ];
        let got: Vec<_> = hints.iter().map(|c| (c.label.as_str(), c.hint.as_deref().unwrap(), c.key.unwrap())).collect();
        assert_eq!(got, want);
    }

    /// [`sentry`] logs in by paste or device: a terminal asks which, after
    /// the approval, device preselected.
    #[test]
    fn a_terminal_asks_the_method_among_several() {
        let data = temp("ask-method");
        let a = args("s");
        let mut term = Script::new(true, &["Approve", "paste"], Some("sntryu_1"));
        let done = login_at(&data, &a, Some(vec![sentry()]), &mut term).unwrap();
        assert!(done.contains("approved"), "{done}");
        assert_eq!(Store::in_data(&data).token_now("s").unwrap().unwrap().expose(), "sntryu_1");
        let (choices, default) = &term.asked[1];
        let labels: Vec<&str> = choices.iter().map(|c| c.label.as_str()).collect();
        assert_eq!((labels, *default), (vec!["device", "paste"], 0));
        assert!(choices[1].hint.as_deref().unwrap().starts_with("paste a user auth token"), "{choices:?}");
        assert!(term.shown.contains("Approve this claim for `s`?Log in `s` by"), "{}", term.shown);

        // --yes still asks; the pick is honored.
        let yes = Args { yes: true, ..args("s") };
        let err = login_at(&data, &yes, None, &mut Script::new(true, &["device"], None)).unwrap_err();
        assert!(err.to_string().contains("no network in this test"), "the device login: {err}");
        // --method, or off a terminal: not asked.
        let paste = Args { method: Some("paste".into()), ..args("s") };
        let mut term = Script::new(true, &[], Some("sntryu_2"));
        login_at(&data, &paste, None, &mut term).unwrap();
        assert!(term.asked.is_empty());
        let mut term = Script::new(false, &[], Some("sntryu_3"));
        login_at(&data, &args("s"), None, &mut term).unwrap();
        assert!(term.asked.is_empty());
        assert_eq!(Store::in_data(&data).token_now("s").unwrap().unwrap().expose(), "sntryu_3");
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn one_usable_method_is_not_asked_and_a_cancel_changes_nothing() {
        let data = temp("ask-one");
        let mut c = sentry();
        c.pinned.login.retain(|l| matches!(l, Login::Paste { .. }));
        let mut term = Script::new(true, &["Approve"], Some("sntryu_1"));
        login_at(&data, &args("s"), Some(vec![c]), &mut term).unwrap();
        assert_eq!(term.asked.len(), 1, "the approval only");

        let fresh = temp("ask-cancel");
        let mut term = Script::new(true, &["Approve", ESC], Some("sntryu_1"));
        let err = login_at(&fresh, &args("s"), Some(vec![sentry()]), &mut term).unwrap_err();
        assert_eq!(err.to_string(), "no login method picked: nothing changed");
        assert!(!home::path(&fresh).exists() && !Store::in_data(&fresh).path().exists());
        // Refused: never asked.
        let mut term = Script::new(true, &["Refuse"], None);
        login_at(&fresh, &args("s"), Some(vec![sentry()]), &mut term).unwrap_err();
        assert_eq!(term.asked.len(), 1);
        let _ = std::fs::remove_dir_all(&data);
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
        let err = login_at(&data, &yes, Some(vec![github()]), &mut Script::new(false, &[], Some("ghp_classic"))).unwrap_err();
        assert!(err.to_string().contains("not `ghp_…`"), "{err}");
        let err = login_at(&data, &yes, Some(vec![github()]), &mut Script::new(false, &[], Some("gho_oauth"))).unwrap_err();
        assert!(err.to_string().contains("a `gho_…` token reaches every repo") && err.to_string().contains("--broad"), "{err}");
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
        gh.pinned.tokens = Some(claim::Tokens { as_: vec![], accept: vec!["github_pat_".into()], broad: vec![], expiry: None });
        let mut f = flows(|_| {}, Box::new(FakeClock::new()));
        let a = Args { yes: true, ..args("gh") };
        let err = super::login_at(&data, &a, Some(vec![gh]), &mut Script::new(true, &[], None), &mut f).unwrap_err();
        assert!(err.to_string().contains("takes only `github_pat_…` tokens, not `ghu_…`"), "{err}");
        assert!(!home::path(&data).exists() && !Store::in_data(&data).path().exists());
        let _ = std::fs::remove_dir_all(&data);
    }

    /// gh's own device flow on a new broad credential: asked about, its
    /// scope sent, its `gho_` stored with no expiry; the entry stays broad.
    #[test]
    fn a_broad_login_asks_then_takes_a_broad_token() {
        use crate::credentials::home::tests::github;
        use crate::credentials::oauth::tests::{FakeClock, GH_OAUTH_TOKEN, device_provider_issuing, field, form};
        let (port, seen) = device_provider_issuing(GH_OAUTH_TOKEN);
        let data = temp("broad");
        let mut gh = github();
        gh.pinned.login = vec![Login::Paste { hint: None, create: None }, home_device(port), broad_device(port)];
        let a = Args { broad: true, ..args("gh") };
        let mut term = Script::new(true, &["Approve", "Approve", "device"], None);
        let mut f = flows(|_| {}, Box::new(FakeClock::new()));
        let done = super::login_at(&data, &a, Some(vec![gh.clone()]), &mut term, &mut f).unwrap();
        assert!(done.contains("approved"), "{done}");
        let methods: Vec<&str> = term.asked[2].0.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(methods, ["device", "paste"], "the broad device and paste");
        assert!(term.shown.contains("`gh` is broad: its token reaches every repo your account can access."), "{}", term.shown);
        assert!(term.shown.contains("Make `gh` a broad credential?"), "{}", term.shown);
        let Some(Record::OAuth(rec)) = Store::in_data(&data).get("gh").unwrap() else { panic!("an OAuth record") };
        assert_eq!((rec.access_token.expose(), rec.client_id.as_str()), ("gho_x", "178c6fc778ccc68e1d6a"));
        assert_eq!((rec.expires_at, rec.refresh_token.is_none()), (None, true));
        assert_eq!(field(&form(&seen.lock().unwrap()[0].1), "scope"), Some("repo read:org"));
        assert!(home::load(&home::path(&data)).unwrap()["gh"].broad);

        // A re-login keeps it broad, unasked; a plain one is refused.
        let piped = Args { broad: true, method: Some("paste".into()), ..args("gh") };
        let done = login_at(&data, &piped, None, &mut Script::new(false, &[], Some("gho_y"))).unwrap();
        assert!(done.ends_with("secret replaced"), "{done}");
        let pat = login_at(&data, &piped, None, &mut Script::new(false, &[], Some("github_pat_1")));
        assert!(pat.is_ok(), "a broad credential takes `accept`'s too: {pat:?}");
        let err = login_at(&data, &args("gh"), None, &mut Script::new(false, &[], Some("github_pat_2"))).unwrap_err();
        assert!(err.to_string().contains("credential `gh` is broad") && err.to_string().contains("--broad"), "{err}");
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn broad_refusals_change_nothing() {
        use crate::credentials::home::tests::github;
        let data = temp("broad-refuse");
        let broad = Args { broad: true, yes: true, ..args("gh") };
        let err = login_at(&data, &broad, Some(vec![sentry()]), &mut Script::new(false, &[], Some("t"))).unwrap_err();
        assert!(err.to_string().contains("the `sentry` claim takes no broad token"), "{err}");
        let unconfirmed = Args { broad: true, ..args("gh") };
        let err = login_at(&data, &unconfirmed, Some(vec![github()]), &mut Script::new(true, &["Approve", "Refuse"], Some("gho_x"))).unwrap_err();
        assert!(err.to_string().contains("not approved"), "{err}");
        let piped = Args { method: Some("paste".into()), ..args("gh") };
        let err = login_at(&data, &Args { broad: true, ..piped }, Some(vec![github()]), &mut Script::new(false, &[], Some("gho_x")))
            .unwrap_err();
        assert!(err.to_string().contains("pass --yes"), "{err}");
        assert!(!home::path(&data).exists() && !Store::in_data(&data).path().exists());

        login_at(&data, &Args { yes: true, ..args("gh") }, Some(vec![github()]), &mut Script::new(false, &[], Some("github_pat_1"))).unwrap();
        let err = login_at(&data, &broad, None, &mut Script::new(false, &[], Some("gho_x"))).unwrap_err();
        assert!(err.to_string().contains("exists and isn't broad: --broad makes a new credential"), "{err}");
        assert_eq!(Store::in_data(&data).token_now("gh").unwrap().unwrap().expose(), "github_pat_1");
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
        let mut pageless = sentry();
        pageless.pinned.login = vec![Login::Paste { hint: None, create: None }];
        let err = login_at(&data, &owner, Some(vec![pageless]), &mut Script::new(false, &[], Some("t"))).unwrap_err();
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
        assert!(head.starts_with("GET /api/0/users/me/ HTTP/1.1\r\n"), "{head}");
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
        assert_eq!((method, url.as_str(), private), ("GET", "https://sentry.io:8443/api/0/users/me/", false));
        assert_eq!(headers, [Header { name: "authorization".into(), value: b"Bearer t".to_vec() }]);
        p.inject = vec![Slot::Query { param: "key".into() }];
        p.upstreams[0].private = true;
        let (_, url, headers, private) = check_request(&p, "a b", &host).unwrap();
        assert_eq!((url.as_str(), private), ("https://sentry.io:8443/api/0/users/me/?key=a%20b", true));
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

    /// Flows whose claim upstream answers the check with `{}` and each
    /// `orgs.resolve` of `/api/0/organizations/<slug>/` as `orgs` says.
    fn orgs_answered(orgs: &'static [(&'static str, u16, &'static str)]) -> (Flows, crate::broker::serve::tests::Seen) {
        let (port, seen) = crate::broker::serve::tests::upstream_server(move |head, _| {
            let org = orgs.iter().find(|(slug, ..)| head.starts_with(&format!("GET /api/0/organizations/{slug}/ ")));
            match org {
                Some((_, status, body)) => user(*status, body),
                None => user(200, "{}"),
            }
        });
        let mut f = offline();
        f.http = Box::new(|| Ok(crate::credentials::oauth::tests::http()));
        f.authority = Box::new(move |_| format!("127.0.0.1:{port}"));
        (f, seen)
    }

    /// [`sentry`] without its org fence.
    fn unfenced() -> Claim {
        let mut c = sentry();
        c.pinned.orgs = None;
        c
    }

    fn with_orgs(name: &str, orgs: &[&str]) -> Args {
        Args { yes: true, orgs: orgs.iter().map(|s| s.to_string()).collect(), ..args(name) }
    }

    #[test]
    fn org_slugs_are_one_segment_not_an_id_and_deduped() {
        let flags = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(org_slugs(&flags(&["acme", "Beta-2", "ACME", "b_c"])).unwrap(), ["acme", "Beta-2", "b_c"]);
        assert_eq!(org_slugs(&[]).unwrap(), Vec::<String>::new());
        assert_eq!(org_slugs(&flags(&["1a"])).unwrap(), ["1a"], "digits and a letter: a slug");
        for bad in ["", "a/b", "a b", "a.b", "é"] {
            let err = org_slugs(&flags(&[bad])).unwrap_err().to_string();
            assert!(err.ends_with("a slug is one segment of letters, digits, `_` and `-`"), "{bad}: {err}");
        }
        let err = org_slugs(&flags(&["acme", "12"])).unwrap_err().to_string();
        assert_eq!(err, "--org `12`: that's an org id; name the org by its slug");
    }

    #[test]
    fn login_stores_the_orgs_each_checked_with_the_token() {
        use crate::broker::policy::tests::org_fenced;
        let data = temp("orgs");
        let (mut f, seen) = orgs_answered(&[("acme", 200, r#"{"id":"7","slug":"acme"}"#), ("beta", 200, r#"{"id":8}"#)]);
        let mut term = Script::new(false, &[], Some("sntryu_1"));
        let a = with_orgs("s", &["acme", "beta", "Acme"]);
        let done = super::login_at(&data, &a, Some(vec![org_fenced()]), &mut term, &mut f).unwrap();
        assert!(done.contains("approved"), "{done}");
        assert!(!term.shown.contains("warning") && !term.shown.contains("note"), "{}", term.shown);
        assert_eq!(home::load(&home::path(&data)).unwrap()["s"].orgs, ["acme", "beta"]);
        let heads: Vec<String> = seen.lock().unwrap().iter().map(|(h, _)| h.clone()).collect();
        assert_eq!(heads.len(), 3, "the check, then one per org: {heads:?}");
        assert!(heads[1].starts_with("GET /api/0/organizations/acme/ HTTP/1.1\r\n"), "{}", heads[1]);
        assert!(heads[1].contains("\r\nauthorization: Bearer sntryu_1\r\n"), "{}", heads[1]);
        assert!(heads[2].starts_with("GET /api/0/organizations/beta/ "), "{}", heads[2]);
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn an_org_the_token_cant_reach_changes_nothing() {
        use crate::broker::policy::tests::org_fenced;
        let data = temp("orgs-out");
        for (status, err) in [
            (404, "org `beta` isn't reachable with this token (404): nothing changed"),
            (403, "org `beta` isn't reachable with this token (403): nothing changed"),
            (401, "the token was refused by sentry.io (401): nothing changed"),
        ] {
            let answers: &'static [(&str, u16, &str)] = match status {
                404 => &[("acme", 200, r#"{"id":"7"}"#), ("beta", 404, "{}")],
                403 => &[("acme", 200, r#"{"id":"7"}"#), ("beta", 403, "{}")],
                _ => &[("acme", 200, r#"{"id":"7"}"#), ("beta", 401, "{}")],
            };
            let (mut f, _) = orgs_answered(answers);
            let mut term = Script::new(false, &[], Some("sntryu_1"));
            let a = with_orgs("s", &["acme", "beta"]);
            let got = super::login_at(&data, &a, Some(vec![org_fenced()]), &mut term, &mut f).unwrap_err().to_string();
            assert_eq!(got, err);
            assert!(!home::path(&data).exists() && !Store::in_data(&data).path().exists());
        }
    }

    #[test]
    fn an_unchecked_org_is_stored_with_a_warning() {
        use crate::broker::policy::tests::org_fenced;
        let data = temp("orgs-unknown");
        let mut term = Script::new(false, &[], Some("sntryu_1"));
        let a = with_orgs("s", &["acme"]);
        login_at(&data, &a, Some(vec![org_fenced()]), &mut term).unwrap();
        assert!(term.shown.ends_with("warning: cannot check org `acme`, stored unchecked: no network in this test\n"), "{}", term.shown);
        assert_eq!(home::load(&home::path(&data)).unwrap()["s"].orgs, ["acme"]);
        let _ = std::fs::remove_dir_all(&data);

        let (mut f, _) = orgs_answered(&[("acme", 200, r#"{"slug":"acme"}"#)]);
        let mut term = Script::new(false, &[], Some("sntryu_1"));
        super::login_at(&data, &a, Some(vec![org_fenced()]), &mut term, &mut f).unwrap();
        assert!(term.shown.ends_with("cannot check org `acme`, stored unchecked: its answer names no `id`\n"), "{}", term.shown);
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn org_refusals_change_nothing() {
        use crate::broker::policy::tests::org_fenced;
        let data = temp("orgs-refuse");
        let err = login_at(&data, &with_orgs("s", &["acme"]), Some(vec![unfenced()]), &mut Script::new(false, &[], Some("t"))).unwrap_err();
        assert_eq!(err.to_string(), "the `sentry` claim has no org fence: --org changes nothing");
        let err = login_at(&data, &with_orgs("s", &["a/b"]), Some(vec![org_fenced()]), &mut Script::new(false, &[], Some("t"))).unwrap_err();
        assert!(err.to_string().starts_with("--org `a/b`"), "{err}");
        assert!(!home::path(&data).exists() && !Store::in_data(&data).path().exists());

        login_at(&data, &with_orgs("s", &["acme"]), Some(vec![org_fenced()]), &mut Script::new(false, &[], Some("t"))).unwrap();
        let err = login_at(&data, &with_orgs("s", &["beta"]), None, &mut Script::new(false, &[], Some("t2"))).unwrap_err();
        let want = format!("credential `s` exists: its orgs are `orgs` in {}", home::path(&data).display());
        assert_eq!(err.to_string(), want);
        assert_eq!(Store::in_data(&data).token_now("s").unwrap().unwrap().expose(), "t");
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn a_credential_naming_no_org_gets_a_note() {
        use crate::broker::policy::tests::org_fenced;
        let data = temp("orgs-note");
        let yes = Args { yes: true, ..args("s") };
        let mut term = Script::new(false, &[], Some("t"));
        login_at(&data, &yes, Some(vec![org_fenced()]), &mut term).unwrap();
        let note = format!(
            "note: `s` names no orgs, so the broker refuses its org routes until `orgs` is set in {} \
             (or log in again as a new credential with --org)\n",
            home::path(&data).display()
        );
        assert!(term.shown.ends_with(&note), "{}", term.shown);
        assert!(home::load(&home::path(&data)).unwrap()["s"].orgs.is_empty());

        let mut term = Script::new(false, &[], Some("t2"));
        login_at(&data, &args("s"), None, &mut term).unwrap();
        assert!(term.shown.ends_with(&note), "a re-login, still none: {}", term.shown);
        home::update(&home::path(&data), |h| {
            h.get_mut("s").unwrap().orgs = vec!["acme".into()];
            Ok(())
        })
        .unwrap();
        let mut term = Script::new(false, &[], Some("t3"));
        login_at(&data, &args("s"), None, &mut term).unwrap();
        assert!(!term.shown.contains("note"), "orgs set by hand: {}", term.shown);
        let mut term = Script::new(false, &[], Some("t"));
        login_at(&data, &Args { yes: true, ..args("plain") }, Some(vec![unfenced()]), &mut term).unwrap();
        assert!(!term.shown.contains("note"), "no org fence: {}", term.shown);
        let _ = std::fs::remove_dir_all(&data);
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
