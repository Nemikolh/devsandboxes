//! `devsandbox login <credential>` (docs/reference/credentials-cli.md): store
//! a credential's secret in the home's file store and approve the claim it
//! is for (credential-broker, *`credentials.toml`*; github-and-sentry-access,
//! *The claim contract*). Local only: this host is its credentials' home.
//! The secret comes by paste, the device flow or the browser's OAuth login,
//! per the claim's `login` (docs/architecture/credentials/login.md).

use std::io::{IsTerminal, Read, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::credentials::claim::{self, Claim, Login};
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
    /// owns); kept in the token record, not the claim.
    pub client_id: Option<String>,
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

/// The login method: `wanted` if the claim lists it, else the first of
/// the claim's that works here, in this order: off a terminal paste (stdin),
/// device, browser; on one device, browser, paste. Browser only where a
/// browser opens. A claim's own order isn't kept (its version sorts them).
fn choose<'a>(logins: &'a [Login], wanted: Option<&str>, tty: bool, browser: bool, kind: &str) -> Result<&'a Login> {
    if logins.is_empty() {
        bail!("the `{kind}` claim has no secret to log in (no `inject` slot)");
    }
    let listed = || logins.iter().map(method_name).collect::<Vec<_>>().join(", ");
    let find = |m: &str| logins.iter().find(|l| method_name(l) == m);
    if let Some(m) = wanted {
        return find(m).with_context(|| format!("the `{kind}` claim logs in by {}, not {m}", listed()));
    }
    let order: &[&str] = if tty { &["device", "browser", "paste"] } else { &["paste", "device", "browser"] };
    if let Some(l) = order.iter().filter(|m| browser || **m != "browser").find_map(|m| find(m)) {
        return Ok(l);
    }
    if find("browser").is_some() {
        bail!("the `{kind}` claim logs in by {}: no browser opens here (pass --method browser to try anyway)", listed());
    }
    bail!("the `{kind}` claim logs in by {}: not yet (github-and-sentry-access step 3)", listed())
}

fn check_client_id(id: &str) -> Result<()> {
    if id.is_empty() || id.chars().any(|c| c.is_control() || c.is_whitespace()) {
        bail!("--client-id must be non-empty, no whitespace");
    }
    Ok(())
}

/// The secret by `login`'s method.
fn obtain(login: &Login, name: &str, args: &Args, term: &mut dyn Term, flows: &mut Flows) -> Result<Record> {
    let client = |claims: &str| args.client_id.clone().unwrap_or_else(|| claims.to_string());
    match login {
        Login::Paste { hint } => {
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
    let login = choose(&pinned.login, args.method.as_deref(), term.tty(), flows.browser, &kind)?;
    if let Some(id) = &args.client_id {
        if matches!(login, Login::Paste { .. }) {
            bail!("--client-id is for the device and browser logins, not paste");
        }
        check_client_id(id)?;
    }
    let approving = candidate
        .as_ref()
        .filter(|c| existing.as_ref().and_then(|e| e.approved_claim(&c.version)).is_none());
    if let Some(c) = approving {
        term.show(&format!("{}  version {}\n", shown.summary(), c.short()));
        approve_or_refuse(term, args.yes, &format!("Approve this claim for `{name}`?"))?;
    }

    let record = obtain(login, name, args, term, flows)?;
    crate::credentials::tokens::check_accepted(pinned.tokens.as_ref(), record.access_token().expose(), &kind)?;
    // The secret first: a claim approved without one serves nothing.
    Store::in_data(data).put(name, &record)?;
    let profiles = (!args.profiles.is_empty()).then(|| args.profiles.clone());
    let approved = home::update(&path, |h: &mut Home| {
        let entry = h.entry(name.to_string()).or_insert_with(|| Entry::new(&kind, profiles));
        if entry.kind != kind {
            bail!("credential `{name}` is a `{}` credential, not `{kind}`", entry.kind);
        }
        entry.broken = None;
        entry.expires_at = None;
        Ok(approving.cloned().map(|c| entry.approve(c)).unwrap_or(false))
    })?;
    Ok(match (approved, approving) {
        (true, Some(c)) => format!("logged in `{name}` ({kind}); claim version {} approved", c.short()),
        _ => format!("logged in `{name}` ({kind}): secret replaced"),
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
        assert!(term.shown.is_empty());
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
            client_id: "c".into(),
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
            client_id: "Iv1.claims".into(),
            device_url: format!("https://127.0.0.1:{port}/device"),
            token_url: format!("https://127.0.0.1:{port}/token"),
        }
    }

    fn browser(port: u16) -> Login {
        Login::Browser {
            client_id: "Iv1.claims".into(),
            authorize_url: "https://github.example/login/oauth/authorize".into(),
            token_url: format!("https://127.0.0.1:{port}/token"),
        }
    }

    #[test]
    fn the_method_is_asked_for_else_the_first_that_works_here() {
        let paste = Login::Paste { hint: None };
        let all = [paste.clone(), device(1), browser(1)];
        let name = |l: Result<&Login>| method_name(l.unwrap());
        assert_eq!(name(choose(&all, None, false, true, "k")), "paste");
        assert_eq!(name(choose(&all, None, true, true, "k")), "device");
        assert_eq!(name(choose(&[paste.clone(), browser(1)], None, true, true, "k")), "browser");
        assert_eq!(name(choose(&[paste.clone(), browser(1)], None, true, false, "k")), "paste");
        assert_eq!(name(choose(&[device(1)], None, false, false, "k")), "device");
        assert_eq!(name(choose(&all, Some("browser"), false, false, "k")), "browser");
        let err = choose(&[browser(1)], None, true, false, "k").unwrap_err().to_string();
        assert!(err.contains("no browser opens here"), "{err}");
        assert_eq!(name(choose(&[browser(1)], None, false, true, "k")), "browser");
        let err = choose(&[Login::GithubAppManifest {}], None, true, true, "k").unwrap_err().to_string();
        assert!(err.contains("by github-app-manifest: not yet"), "{err}");
        let err = choose(&[], None, true, true, "k").unwrap_err().to_string();
        assert!(err.contains("no `inject` slot"), "{err}");
    }

    fn flows(open: impl FnMut(&str) + 'static, clock: Box<dyn Clock>) -> Flows {
        Flows {
            http: Box::new(|| Ok(crate::credentials::oauth::tests::http())),
            clock,
            browser: true,
            open: Box::new(open),
            browser_timeout: Duration::from_secs(20),
        }
    }

    /// The device flow against a local provider, with the home's own app;
    /// a re-login by paste then replaces the record and nothing else.
    #[test]
    fn device_login_stores_the_token_set_under_the_home_client_id() {
        use crate::credentials::oauth::tests::{FakeClock, UNIX, device_provider, field, form};
        let (port, seen) = device_provider();
        let data = temp("device");
        let mut gh = other("github");
        gh.pinned.login = vec![Login::Paste { hint: None }, device(port)];
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
