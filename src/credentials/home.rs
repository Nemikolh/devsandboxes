//! `credentials.toml`, next to `state.toml`: the home's credentials
//! (credential-broker, *`credentials.toml`*). One table per credential,
//! named as `devsandbox login <credential>` named it (`sentry-work`), holding
//! its kind, where its secret is (`source`), its grant, and the claim
//! versions approved for it and pending approval.
//!
//! An approved version keeps the canonical pinned claim
//! ([`Claim::canonical`]) under its hash ([`Claim::version`]): the broker
//! uses this copy, never the instance's, and `access approve` diffs a
//! pending one against it. Each is re-hashed at load, so a hand edit to a
//! claim without its version fails the file instead of approving content
//! nobody saw. `approved` is in approval order: the last is the latest.
//!
//! Written by `login` and `access approve` (and `run` recording a pending
//! version) under the sibling lock, temp file and rename
//! (`serve::locked_file`). Kept out of every container by `key_guard`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::claim::Claim;
use super::review::View;

/// Every credential by name.
pub type Home = BTreeMap<String, Entry>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    /// The claim kind it serves (`sentry`): what a config's `access.<kind>`
    /// and a feature's claim name.
    pub kind: String,
    #[serde(default)]
    pub source: Source,
    /// The profiles whose instances may use it; unset: every profile, as a
    /// sandbox's unset `profiles` allows every declared one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profiles: Option<Vec<String>>,
    /// The scopes it grants at most; unset: every scope of the claim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<Vec<String>>,
    // Read from credential-broker step 4 (`approvers`), 5 (`members`) and 7
    // (`delegate`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub approvers: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub delegate: Vec<String>,
    /// The token kinds it offers (`credentials::tokens::pick`); unset:
    /// `["user"]`.
    #[serde(default, rename = "as", skip_serializing_if = "Vec::is_empty")]
    pub as_: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub approved: Vec<Version>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending: Vec<Version>,
    /// Set by the broker ([`mark_broken`]), cleared by a login or a
    /// successful refresh. Refuses no call: it's for `access ls` (and, at
    /// credential-broker step 4, the approvers' notice).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub broken: Option<Broken>,
    /// RFC 3339, UTC: a pasted secret's expiry as the upstream reported it
    /// to the broker (the claim's `tokens.expiry` header); cleared by a
    /// login.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

/// Why the broker stopped trusting the secret: a refused refresh, an
/// unreadable store, or a 401 on the claim's `check` route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Broken {
    /// RFC 3339, UTC: when it was first marked.
    pub at: String,
    /// One line.
    pub reason: String,
}

impl Entry {
    pub fn new(kind: &str, profiles: Option<Vec<String>>) -> Entry {
        Entry {
            kind: kind.to_string(),
            source: Source::File,
            profiles,
            max: None,
            members: Vec::new(),
            approvers: Vec::new(),
            delegate: Vec::new(),
            as_: Vec::new(),
            approved: Vec::new(),
            pending: Vec::new(),
            broken: None,
            expires_at: None,
        }
    }

    /// [`Entry::expires_at`] in unix seconds; checked at load.
    pub fn expires(&self) -> Option<u64> {
        self.expires_at.as_deref().and_then(crate::broker::lfs::parse_rfc3339)
    }

    /// The approved claim of `version`, canonical: what the broker enforces
    /// for an instance that ran with it.
    pub fn approved_claim(&self, version: &str) -> Option<&str> {
        self.approved.iter().find(|v| v.version == version).map(|v| v.claim.as_str())
    }

    pub fn latest_approved(&self) -> Option<&Version> {
        self.approved.last()
    }

    /// Drops it from `pending` too. Whether it was new.
    pub fn approve(&mut self, version: Version) -> bool {
        self.pending.retain(|p| p.version != version.version);
        if self.approved_claim(&version.version).is_some() {
            return false;
        }
        self.approved.push(version);
        true
    }

    fn check(&self, name: &str) -> Result<()> {
        super::check_name(name)?;
        super::claim::check_kind(&self.kind).context("`kind`")?;
        for p in self.profiles.iter().flatten() {
            crate::config::check_profile_name(p).context("`profiles`")?;
        }
        super::tokens::offered(&self.as_).context("`as`")?;
        if let Some(at) = &self.expires_at
            && self.expires().is_none()
        {
            bail!("`expires_at`: `{at}` isn't an RFC 3339 date-time");
        }
        for (field, versions) in [("approved", &self.approved), ("pending", &self.pending)] {
            for v in versions {
                v.check(&self.kind).with_context(|| format!("`{field}` version {}", v.short()))?;
            }
        }
        Ok(())
    }
}

/// Where the secret is. Only the file store so far.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Source {
    /// `credentials::store`.
    #[default]
    File,
}

impl TryFrom<String> for Source {
    type Error = String;

    fn try_from(s: String) -> Result<Source, String> {
        match s.as_str() {
            "file" => Ok(Source::File),
            "keyring" => Err("`source = \"keyring\"`: not yet; only \"file\" for now".into()),
            s if s.contains("://") => Err(format!("`source = \"{s}\"`: external stores are not yet supported; only \"file\" for now")),
            s => Err(format!("unknown `source` `{s}` (only \"file\")")),
        }
    }
}

impl From<Source> for String {
    fn from(s: Source) -> String {
        match s {
            Source::File => "file".into(),
        }
    }
}

/// A claim version: its hash and the canonical pinned claim it hashes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Version {
    pub version: String,
    pub claim: String,
}

impl Version {
    pub fn of(claim: &Claim) -> Version {
        Version { version: claim.version(), claim: claim.canonical() }
    }

    /// From canonical JSON a host sent, re-hashed here.
    pub fn from_canonical(claim: &str) -> Version {
        Version { version: hash(claim), claim: claim.to_string() }
    }

    /// Enough of the hash to tell versions apart on screen.
    pub fn short(&self) -> &str {
        &self.version[..self.version.len().min(12)]
    }

    pub fn view(&self) -> Result<View> {
        View::parse(&self.claim)
    }

    fn check(&self, kind: &str) -> Result<()> {
        if hash(&self.claim) != self.version {
            bail!("its claim doesn't hash to its version");
        }
        let view = self.view()?;
        if view.kind != kind {
            bail!("a `{}` claim, not `{kind}`", view.kind);
        }
        Ok(())
    }
}

/// [`Claim::version`]'s hash.
fn hash(canonical: &str) -> String {
    hex::encode(&Sha256::digest(canonical.as_bytes())[..])
}

/// Records `claim` (canonical) as pending for the credential `name`, unless
/// already approved or pending. Whether it was new.
pub fn record_pending(home: &mut Home, name: &str, claim: &str) -> Result<bool> {
    let entry = home.get_mut(name).with_context(|| format!("no credential `{name}`"))?;
    let version = Version::from_canonical(claim);
    version.check(&entry.kind)?;
    if entry.approved_claim(&version.version).is_some() || entry.pending.iter().any(|p| p.version == version.version) {
        return Ok(false);
    }
    entry.pending.push(version);
    Ok(true)
}

/// The credential an instance in `profile` takes for claim kind `kind`: the
/// one its config's `access.<kind>.credential` names (`named`), else the one
/// of that kind granted to `profile` (an entry without `profiles` is granted
/// to every profile, and to an instance without one). `Ok(None)`: none here.
/// Resolved per use, never kept: profiles and configs change.
pub fn resolve<'a>(
    home: &'a Home,
    kind: &str,
    named: Option<&str>,
    profile: Option<&str>,
) -> Result<Option<(&'a str, &'a Entry)>> {
    if let Some(name) = named {
        return match home.get_key_value(name) {
            None => Ok(None),
            Some((_, e)) if e.kind != kind => {
                bail!("`access.{kind}.credential` names `{name}`, a `{}` credential", e.kind)
            }
            Some((n, e)) => Ok(Some((n.as_str(), e))),
        };
    }
    let granted = |e: &Entry| match &e.profiles {
        None => true,
        Some(list) => profile.is_some_and(|p| list.iter().any(|l| l == p)),
    };
    let found: Vec<(&String, &Entry)> = home.iter().filter(|(_, e)| e.kind == kind && granted(e)).collect();
    match found[..] {
        [] => Ok(None),
        [(n, e)] => Ok(Some((n.as_str(), e))),
        _ => {
            let names: Vec<String> = found.iter().map(|(n, _)| format!("`{n}`")).collect();
            bail!(
                "credentials {} are all `{kind}` credentials granted here: name one in `access.{kind}.credential`",
                names.join(", ")
            )
        }
    }
}

/// `<data>/credentials.toml`.
pub fn path(data: &Path) -> PathBuf {
    data.join(crate::daemon_config::CREDENTIALS)
}

pub fn parse(text: &str) -> Result<Home> {
    let home: Home = toml::from_str(text)?;
    for (name, entry) in &home {
        entry.check(name).with_context(|| format!("credential `{name}`"))?;
    }
    Ok(home)
}

pub fn serialize(home: &Home) -> Result<String> {
    toml::to_string_pretty(home).context("cannot serialize credentials")
}

/// A missing file is no credentials.
pub fn load(path: &Path) -> Result<Home> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text).with_context(|| format!("invalid {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Home::new()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// Read-modify-write under the lock, saved only when `f` changed it; `f`'s
/// error leaves the file alone.
pub fn update<T>(path: &Path, f: impl FnOnce(&mut Home) -> Result<T>) -> Result<T> {
    crate::serve::locked_file::update(path, load, serialize, |home| {
        let before = home.clone();
        let out = f(home);
        if out.is_err() {
            *home = before;
        }
        out
    })?
}

/// Marks `name` broken unless it already is (the first reason stays until a
/// login). No such credential: nothing.
pub fn mark_broken(path: &Path, name: &str, broken: Broken) -> Result<()> {
    update(path, |h| {
        if let Some(e) = h.get_mut(name)
            && e.broken.is_none()
        {
            e.broken = Some(broken);
        }
        Ok(())
    })
}

/// Records `name`'s expiry (unix seconds). No such credential: nothing.
pub fn set_expires_at(path: &Path, name: &str, at: u64) -> Result<()> {
    update(path, |h| {
        if let Some(e) = h.get_mut(name) {
            e.expires_at = Some(crate::inbox::rfc3339(at));
        }
        Ok(())
    })
}

pub fn clear_broken(path: &Path, name: &str) -> Result<()> {
    update(path, |h| {
        if let Some(e) = h.get_mut(name) {
            e.broken = None;
        }
        Ok(())
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::credentials::claim;

    pub(crate) fn sentry() -> Claim {
        fixture("sentry")
    }

    /// The repo-fenced intercept claim of `tests/fixtures/features/github`.
    pub(crate) fn github() -> Claim {
        fixture("github")
    }

    fn fixture(kind: &str) -> Claim {
        let path = format!("{}/tests/fixtures/features/{kind}/devcontainer-feature.json", env!("CARGO_MANIFEST_DIR"));
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        claim::parse(kind, &v["customizations"]["devsandbox"]["credentials"][kind]).unwrap()
    }

    #[test]
    fn round_trips_and_rehashes() {
        let mut entry = Entry::new("sentry", Some(vec!["work".into()]));
        entry.max = Some(vec!["releases.write".into()]);
        entry.approvers = vec!["laptop".into()];
        entry.as_ = vec!["user".into()];
        assert!(entry.approve(Version::of(&sentry())));
        assert!(!entry.approve(Version::of(&sentry())), "approved once");
        let home = Home::from([("sentry-work".to_string(), entry)]);
        let text = serialize(&home).unwrap();
        assert!(text.starts_with("[sentry-work]\nkind = \"sentry\"\nsource = \"file\"\nprofiles = [\"work\"]\n"), "{text}");
        assert!(text.contains("as = [\"user\"]"), "{text}");
        assert!(!text.contains("pending") && !text.contains("members"), "{text}");
        assert_eq!(parse(&text).unwrap(), home);
        let v = &home["sentry-work"].approved[0];
        assert_eq!(v.version, sentry().version());
        assert_eq!(home["sentry-work"].approved_claim(&v.version), Some(sentry().canonical().as_str()));

        let tampered = text.replace("de.sentry.io", "evil.example");
        let err = format!("{:#}", parse(&tampered).unwrap_err());
        assert!(err.contains("doesn't hash to its version"), "{err}");
        let other_kind = text.replace("kind = \"sentry\"", "kind = \"github\"");
        let err = format!("{:#}", parse(&other_kind).unwrap_err());
        assert!(err.contains("a `sentry` claim, not `github`"), "{err}");
    }

    #[test]
    fn fields_and_sources_are_checked() {
        assert_eq!(parse("").unwrap(), Home::new());
        let min = parse("[sentry-work]\nkind = \"sentry\"\n").unwrap();
        assert_eq!(min["sentry-work"], Entry::new("sentry", None));
        for (bad, wants) in [
            ("[x]\nkind = \"sentry\"\nsource = \"keyring\"\n", "not yet"),
            ("[x]\nkind = \"sentry\"\nsource = \"op://Private/x\"\n", "not yet supported"),
            ("[x]\nkind = \"sentry\"\nsource = \"vault\"\n", "unknown `source`"),
            ("[x]\nkind = \"sentry\"\nbogus = 1\n", "bogus"),
            ("[x]\nsource = \"file\"\n", "kind"),
            ("[x]\nkind = \"Sentry\"\n", "the kind must be"),
            ("[X]\nkind = \"sentry\"\n", "credential name"),
            ("[x]\nkind = \"sentry\"\nprofiles = [\"Work\"]\n", "profile name"),
            ("[x]\nkind = \"github\"\nas = [\"bot\"]\n", "`as`"),
            ("[x]\nkind = \"github\"\nexpires_at = \"soon\"\n", "`expires_at`"),
        ] {
            let err = format!("{:#}", parse(bad).unwrap_err());
            assert!(err.contains(wants), "{bad}: {err}");
        }
    }

    #[test]
    fn pending_versions_are_recorded_once_and_move_on_approval() {
        let mut home = Home::from([("s".to_string(), Entry::new("sentry", None))]);
        let canonical = sentry().canonical();
        assert!(record_pending(&mut home, "nope", &canonical).is_err());
        assert!(record_pending(&mut home, "s", &canonical).unwrap());
        assert!(!record_pending(&mut home, "s", &canonical).unwrap());
        assert!(record_pending(&mut home, "s", "{\"kind\":\"github\",\"claim\":{}}").is_err());
        let v = home["s"].pending[0].clone();
        assert_eq!(v.version, sentry().version());
        let entry = home.get_mut("s").unwrap();
        assert!(entry.approve(v));
        assert!(entry.pending.is_empty());
        assert!(!record_pending(&mut home, "s", &canonical).unwrap(), "already approved");
    }

    #[test]
    fn resolve_takes_the_named_entry_else_the_one_granted() {
        let mut work = Entry::new("sentry", Some(vec!["work".into()]));
        work.approve(Version::of(&sentry()));
        let home = Home::from([
            ("sentry-work".to_string(), work),
            ("sentry-oss".to_string(), Entry::new("sentry", Some(vec!["oss".into()]))),
            ("gh".to_string(), Entry::new("github", None)),
        ]);
        let name = |r: Result<Option<(&str, &Entry)>>| r.unwrap().map(|(n, _)| n.to_string());
        assert_eq!(name(resolve(&home, "sentry", None, Some("work"))), Some("sentry-work".into()));
        assert_eq!(name(resolve(&home, "sentry", None, Some("home"))), None);
        assert_eq!(name(resolve(&home, "sentry", None, None)), None);
        assert_eq!(name(resolve(&home, "github", None, None)), Some("gh".into()));
        assert_eq!(name(resolve(&home, "sentry", Some("sentry-oss"), Some("work"))), Some("sentry-oss".into()));
        assert_eq!(name(resolve(&home, "sentry", Some("nope"), None)), None);
        let err = resolve(&home, "sentry", Some("gh"), None).unwrap_err().to_string();
        assert!(err.contains("a `github` credential"), "{err}");

        let mut both = home.clone();
        both.get_mut("sentry-oss").unwrap().profiles = None;
        let err = resolve(&both, "sentry", None, Some("work")).unwrap_err().to_string();
        assert!(err.contains("`sentry-oss`, `sentry-work`") && err.contains("access.sentry.credential"), "{err}");
    }

    #[test]
    fn broken_is_marked_once_and_cleared() {
        let dir = std::env::temp_dir().join(format!("dsv-cred-home-broken-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let p = path(&dir);
        update(&p, |h| Ok(h.insert("s".into(), Entry::new("sentry", None)))).unwrap();
        let broken = |reason: &str| Broken { at: "2026-11-21T10:00:00Z".into(), reason: reason.into() };
        mark_broken(&p, "s", broken("refused")).unwrap();
        mark_broken(&p, "s", broken("later")).unwrap();
        mark_broken(&p, "nope", broken("x")).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("[s.broken]\nat = \"2026-11-21T10:00:00Z\"\nreason = \"refused\"\n"), "{text}");
        assert_eq!(load(&p).unwrap()["s"].broken, Some(broken("refused")));
        clear_broken(&p, "s").unwrap();
        assert_eq!(load(&p).unwrap()["s"].broken, None);
        set_expires_at(&p, "s", 1_800_000_000).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("expires_at = \"2027-01-15T08:00:00Z\""), "{text}");
        assert_eq!(load(&p).unwrap()["s"].expires(), Some(1_800_000_000));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn update_is_locked_and_atomic() {
        let dir = std::env::temp_dir().join(format!("dsv-cred-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let p = path(&dir);
        update(&p, |h| Ok(h.insert("s".into(), Entry::new("sentry", None)))).unwrap();
        assert!(update(&p, |h| -> Result<()> {
            h.clear();
            bail!("no")
        })
        .is_err());
        assert_eq!(load(&p).unwrap().len(), 1, "a failed update leaves the file");
        assert!(dir.join("credentials.lock").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
