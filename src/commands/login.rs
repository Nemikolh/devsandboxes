//! `devsandbox login <credential>` (docs/reference/credentials-cli.md): store
//! a credential's secret in the home's file store and approve the claim it
//! is for (credential-broker, *`credentials.toml`*; github-and-sentry-access,
//! *The claim contract*). Local only: this host is its credentials' home.
//! Paste is the only login method so far.

use std::io::{IsTerminal, Read, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::credentials::claim::Claim;
use crate::credentials::home::{self, Entry, Home, Version};
use crate::credentials::store::{Secret, Store};

pub struct Args {
    pub credential: String,
    pub feature: Option<String>,
    pub kind: Option<String>,
    pub profiles: Vec<String>,
    pub yes: bool,
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
    let done = login_at(&data_dir()?, &args, claims, &mut Stdio)?;
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

/// Returns the line to print.
pub(crate) fn login_at(data: &Path, args: &Args, claims: Option<Vec<Claim>>, term: &mut dyn Term) -> Result<String> {
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
    let shown = match (&candidate, existing.as_ref().and_then(Entry::latest_approved)) {
        (Some(c), _) => c.view()?,
        (None, Some(v)) => v.view()?,
        (None, None) => unreachable!("pending_one refuses an entry with nothing approved"),
    };
    let hint = shown.paste()?;
    let approving = candidate
        .as_ref()
        .filter(|c| existing.as_ref().and_then(|e| e.approved_claim(&c.version)).is_none());
    if let Some(c) = approving {
        term.show(&format!("{}  version {}\n", shown.summary(), c.short()));
        approve_or_refuse(term, args.yes, &format!("Approve this claim for `{name}`?"))?;
    }

    let secret = term.secret(&match hint {
        Some(hint) => format!("Paste {hint}"),
        None => format!("Paste the `{name}` secret"),
    })?;
    let secret = Secret::new(secret.expose().trim().to_string());
    if secret.expose().is_empty() {
        bail!("no secret given: nothing changed");
    }
    // The secret first: a claim approved without one serves nothing.
    Store::in_data(data).put(name, &secret)?;
    let profiles = (!args.profiles.is_empty()).then(|| args.profiles.clone());
    let approved = home::update(&path, |h: &mut Home| {
        let entry = h.entry(name.to_string()).or_insert_with(|| Entry::new(&kind, profiles));
        if entry.kind != kind {
            bail!("credential `{name}` is a `{}` credential, not `{kind}`", entry.kind);
        }
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

    fn args(name: &str) -> Args {
        Args { credential: name.into(), feature: None, kind: None, profiles: Vec::new(), yes: false }
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
        assert_eq!(Store::in_data(&data).get("sentry-work").unwrap().unwrap().expose(), "tok-1");
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
        // And without one.
        let mut term = Script::new(false, &[], Some("tok-3"));
        login_at(&data, &args("sentry-work"), None, &mut term).unwrap();
        assert_eq!(Store::in_data(&data).get("sentry-work").unwrap().unwrap().expose(), "tok-3");
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
        let err = login_at(&data, &yes, Some(vec![device]), &mut Script::new(false, &[], Some("t"))).unwrap_err();
        assert!(err.to_string().contains("not yet (credential-broker step 2)"), "{err}");

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
}
