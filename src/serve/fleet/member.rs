//! `member.toml`, next to `state.toml`: this host as a fleet member
//! (docs/architecture/daemon/fleet.md, *Member file*): the name it goes by,
//! its fleet's hub, its trust, the config roots it shows the fleet, and the
//! hand-edited `[allow]`, `[caps]` and `dev-builds`. Next to it,
//! `member.key` ([`key_path`]) is this member's private key
//! (`fleet::identity::FileKeyStore`), whose public half `member.get` answers.
//!
//! Written by the daemon only (decision 3 of the fleet plan), through
//! `member.set` ([`call`]; `devsandbox machine trust` and `machine roots`
//! are its clients), with [`update`]: under the sibling
//! `member.lock`, a temp file and a rename (`serve::locked_file`). `hub`
//! has no call: it's written from an accepted membership log
//! (`membership.adopt`, `super::membership_api`) and cleared on leaving,
//! both through [`set_fleet`].
//! `[allow]`, `[caps]` and `dev-builds` have no call: they're edited by
//! hand and re-read at each use (`worker_policy` reads `trust` and
//! `[allow]` itself, on every platform). The `[[agent]]` grants have no
//! `member.*` call either: `machines.set {agent}` and `machines.add
//! {agent}` write them (`super::grants`), read at each `AGENT` stream.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::serve::api::{ApiError, parse as parse_params};
use crate::serve::fleet::identity::{FileKeyStore, KeyStore, PublicKey};
use crate::serve::fleet::membership_api::KeyInfo;
use crate::serve::machines::check_name;
pub use crate::worker_policy::{Allow, Trust};

type Answer = Result<Value, ApiError>;

/// The file. Unknown keys are ignored, as in `daemon.config.toml`: an
/// allowance a newer binary knows is one an older one doesn't grant.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Member {
    /// The name this host goes by in the fleet; empty is [`default_name`]
    /// of the hostname.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// The fleet's hub ([`Member::hub`]): `self` on the hub, else the hub's
    /// member name; empty is in no fleet (fleet plan decision 1).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub hub: String,
    /// What a remote caller reaches here (`worker_policy::Trust`).
    pub trust: Trust,
    /// Take the hub's own binary when it runs an unpublished build
    /// (`fleet::update::Decision::Receive`): code the hub chooses, run
    /// outside a container. Edited by hand, as `[allow]` is; `full` implies
    /// it ([`Member::takes_dev_builds`]).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub dev_builds: bool,
    /// Config roots this host shows the fleet besides those its
    /// `state.toml` has instances in (picked at `machine add`; decision 17
    /// of the fleet plan). Absolute.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub roots: Vec<PathBuf>,
    /// The worker policy's allowances (`worker_policy`, which reads them
    /// itself from this file).
    pub allow: Allow,
    /// Soft caps, reported in this host's capacity
    /// (`serve::capacity::Capacity::caps`); never enforced.
    #[serde(skip_serializing_if = "SoftCaps::is_empty")]
    pub caps: SoftCaps,
    /// `[[agent]]`: the members this host's ssh-agent is granted to, by
    /// name and key (fleet plan decision 65; `super::grants`). None by
    /// default; one per name.
    #[serde(rename = "agent", skip_serializing_if = "Vec::is_empty")]
    pub agent: Vec<Grant>,
}

/// One `[[agent]]` grant: member `name`, as it proved `key` when granted
/// (`ed25519:<hex>`). It holds for `key` and the keys the membership log
/// rotates it to under `name` (`super::grants::granted`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    pub name: String,
    pub key: PublicKey,
}

/// `[caps]`: what this host means to give the fleet (fleet plan, *Placement*). All
/// optional; sizes as `16G`, `512M`, `1.5T` (binary units) or bytes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SoftCaps {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpus: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instances: Option<u64>,
}

impl SoftCaps {
    pub fn is_empty(&self) -> bool {
        *self == SoftCaps::default()
    }

    /// `mem` and `disk` must be sizes.
    pub fn check(&self) -> Result<()> {
        for (key, size) in [("mem", &self.mem), ("disk", &self.disk)] {
            if let Some(size) = size {
                crate::serve::capacity::parse_size(size).with_context(|| format!("`caps.{key}`"))?;
            }
        }
        Ok(())
    }
}

/// `hub` in `member.toml`, typed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Hub {
    /// In no fleet.
    #[default]
    None,
    /// This host is the hub (`self`).
    This,
    /// The hub is the member of this name.
    Other(String),
}

/// The `hub` value naming this host.
pub const HUB_SELF: &str = "self";

impl Hub {
    /// The file's `hub` value read (`""`: none). Unchecked: [`check`] is.
    pub fn of(field: &str) -> Hub {
        match field {
            "" => Hub::None,
            HUB_SELF => Hub::This,
            name => Hub::Other(name.to_string()),
        }
    }

    /// What the file and `member.get` say: `None` in no fleet.
    pub fn field(&self) -> Option<String> {
        match self {
            Hub::None => None,
            Hub::This => Some(HUB_SELF.into()),
            Hub::Other(name) => Some(name.clone()),
        }
    }

    pub fn is_this(&self) -> bool {
        *self == Hub::This
    }
}

/// A member name: a valid machine name but `self`, which `hub = "self"`
/// reads as this host.
pub fn check_member_name(name: &str) -> Result<()> {
    check_name(name)?;
    if name == HUB_SELF {
        bail!("`{HUB_SELF}` is reserved (`hub = \"{HUB_SELF}\"` means this host)");
    }
    Ok(())
}

/// `hub` is empty, `self` or a valid machine name.
fn check_hub(hub: &str) -> Result<()> {
    if !hub.is_empty() && hub != HUB_SELF {
        check_name(hub).context("`hub`")?;
    }
    Ok(())
}

impl Member {
    pub fn hub(&self) -> Hub {
        Hub::of(&self.hub)
    }

    /// Whether a hub's dev build is taken (fleet plan, *Versions*:
    /// `dev-builds`, or `full`).
    pub fn takes_dev_builds(&self) -> bool {
        self.dev_builds || self.trust == Trust::Full
    }

    /// `name`, else [`default_name`] of `hostname`.
    pub fn name_or(&self, hostname: &str) -> Result<String> {
        match self.name.as_str() {
            "" => default_name(hostname),
            name => Ok(name.to_string()),
        }
    }
}

/// [`path_from`] over the process env.
pub fn path() -> Result<PathBuf> {
    path_from(std::env::var_os("XDG_DATA_HOME"), std::env::var_os("HOME"))
}

/// `<data>/devsandbox/member.toml`.
pub fn path_from(xdg_data_home: Option<OsString>, home: Option<OsString>) -> Result<PathBuf> {
    Ok(crate::daemon_config::data_dir_from(xdg_data_home, home)?.join("member.toml"))
}

/// `member.key` next to the `member.toml` at `member_toml`: this member's
/// private key (`fleet::identity::FileKeyStore`).
pub fn key_path(member_toml: &Path) -> PathBuf {
    member_toml.with_file_name(crate::daemon_config::MEMBER_KEY)
}

/// `text` as a [`Member`], [`check`]ed.
pub fn parse(text: &str) -> Result<Member> {
    let member: Member = toml::from_str(text)?;
    check(&member)?;
    Ok(member)
}

/// A `name` that's set is a valid member name, `roots` are absolute, and
/// `[caps]` and `[allow]` are well-formed.
pub fn check(member: &Member) -> Result<()> {
    if !member.name.is_empty() {
        check_member_name(&member.name).context("`name`")?;
    }
    check_hub(&member.hub)?;
    if let Some(bad) = member.roots.iter().find(|r| !r.is_absolute()) {
        bail!("`roots`: `{}` is not an absolute path", bad.display());
    }
    member.caps.check()?;
    let mut names = std::collections::BTreeSet::new();
    for g in &member.agent {
        check_member_name(&g.name).with_context(|| format!("`agent`: `{}`", g.name))?;
        if !names.insert(&g.name) {
            bail!("`agent`: two grants for `{}`", g.name);
        }
    }
    member.allow.check()
}

pub fn serialize(member: &Member) -> Result<String> {
    toml::to_string_pretty(member).context("cannot serialize member.toml")
}

/// Read-modify-write of the file at `path` under its lock (a missing file is
/// the default); saved only when `f` changed it. A file that doesn't parse
/// fails the update rather than being overwritten.
pub fn update<T>(path: &Path, f: impl FnOnce(&mut Member) -> T) -> Result<T> {
    crate::serve::locked_file::update(path, |p| Ok(load(p)?.unwrap_or_default()), serialize, f)
}

/// The file at `path`; a missing file is `None` (the defaults).
pub fn load(path: &Path) -> Result<Option<Member>> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text).map(Some).with_context(|| format!("invalid {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// The name a member goes by without `name`: the hostname's first label
/// (`lap.local` → `lap`), if it's a valid member name.
pub fn default_name(hostname: &str) -> Result<String> {
    let label = hostname.split('.').next().unwrap_or_default();
    match check_member_name(label) {
        Ok(()) => Ok(label.to_string()),
        Err(e) => bail!("cannot name this host after hostname `{hostname}` ({e:#}); set `name` in member.toml (next to state.toml)"),
    }
}

/// `member.get`'s answer (and `member.set`'s, after the change).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberInfo {
    /// `member.toml`'s `name`, else the hostname's first label; `None`
    /// when neither is a valid name.
    pub name: Option<String>,
    /// `hub` as the file says: `self` on the hub, else the hub's member
    /// name; `None` in no fleet.
    pub hub: Option<String>,
    pub trust: Trust,
    /// `dev-builds` as the file says (`full` takes dev builds either way).
    pub dev_builds: bool,
    pub roots: Vec<PathBuf>,
    /// This member's public key (`ed25519:<hex>`), from `member.key`,
    /// created on first ask.
    pub key: PublicKey,
    /// [`PublicKey::fingerprint`] of `key`.
    pub fingerprint: String,
    /// A pending rotation's next key (`member.key.next`, `machine key
    /// rotate`), the key once a membership log listing it is taken; `None`
    /// when none is pending.
    pub next: Option<KeyInfo>,
    /// While this host is `worker` and exposed through another machine
    /// (`fleet::graph::exposed_workers`, over this host's link graph): the
    /// chain from the hub, its last but one dialing this host (a shell
    /// here). `None` otherwise.
    pub exposed_by: Option<Vec<String>>,
}

/// The [`MemberInfo::exposed_by`] chain of the member of this name at this
/// trust (the daemon's link graph).
pub type Exposure<'a> = &'a dyn Fn(&str, Trust) -> Option<Vec<String>>;

/// `member.set`'s params: each one present is changed, the rest kept.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetParams {
    /// A machine name; `""` clears it (the hostname's first label).
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub trust: Option<Trust>,
    /// Absolute paths added to `roots` (one already there is kept once).
    #[serde(default)]
    pub roots_add: Vec<PathBuf>,
    /// Paths taken out of `roots` (one not there is no error).
    #[serde(default)]
    pub roots_rm: Vec<PathBuf>,
}

/// `member` with `p` applied, checked (pure).
pub fn merge(mut member: Member, p: &SetParams) -> Result<Member, ApiError> {
    if let Some(name) = &p.name {
        if !name.is_empty() {
            check_member_name(name).map_err(|e| ApiError::invalid(format!("`name`: {e:#}")))?;
        }
        member.name = name.clone();
    }
    if let Some(trust) = p.trust {
        member.trust = trust;
    }
    for root in &p.roots_add {
        if !root.is_absolute() {
            return Err(ApiError::invalid(format!("`roots_add`: `{}` is not an absolute path", root.display())));
        }
        if !member.roots.contains(root) {
            member.roots.push(root.clone());
        }
    }
    member.roots.retain(|r| !p.roots_rm.contains(r));
    check(&member).map_err(|e| ApiError::invalid(format!("{e:#}")))?;
    Ok(member)
}

/// The answer for `member`, its key loaded (or created) from `member.key`
/// next to `path`.
fn info(member: &Member, path: &Path, hostname: &str, exposure: Exposure) -> Result<MemberInfo, ApiError> {
    let key = FileKeyStore::new(key_path(path)).public_key().map_err(ApiError::internal)?;
    let next = crate::serve::fleet::membership_api::pending_next(path)?;
    let name = member.name_or(hostname).ok();
    Ok(MemberInfo {
        next,
        exposed_by: name.as_deref().and_then(|n| exposure(n, member.trust)),
        name,
        hub: member.hub().field(),
        trust: member.trust,
        dev_builds: member.dev_builds,
        roots: member.roots.clone(),
        fingerprint: key.fingerprint(),
        key,
    })
}

/// `member.get` over the file at `path`; `hostname` names this host when
/// the file doesn't. The first one creates `member.key`.
pub fn get(path: &Path, hostname: &str, exposure: Exposure) -> Result<MemberInfo, ApiError> {
    let member = load(path).map_err(ApiError::internal)?.unwrap_or_default();
    info(&member, path, hostname, exposure)
}

/// `member.set`: `p` merged into the file, checked, written. Nothing else
/// follows it: a peer link reads the name at its next dial
/// (`fleet::dialer::member_name`), the rest is read at each use. The
/// answer's `exposed_by` is at the new trust.
pub fn set(path: &Path, hostname: &str, p: &SetParams, exposure: Exposure) -> Result<MemberInfo, ApiError> {
    let after = update(path, |cur| -> Result<Member, ApiError> {
        *cur = merge(cur.clone(), p)?;
        Ok(cur.clone())
    })
    .map_err(ApiError::internal)??;
    info(&after, path, hostname, exposure)
}

/// `hub` written as `hub` (from an accepted membership log, or
/// [`Hub::None`] on leaving), and with it `name` when given (the name the
/// log knows this host by, pinned). Not an API call: the daemon follows a
/// change itself (`daemon::Shared::set_hub`).
pub fn set_fleet(path: &Path, name: Option<&str>, hub: &Hub) -> Result<Member> {
    update(path, |cur| -> Result<Member> {
        let mut next = cur.clone();
        if let Some(name) = name {
            next.name = name.to_string();
        }
        next.hub = hub.field().unwrap_or_default();
        check(&next)?;
        *cur = next;
        Ok(cur.clone())
    })?
}

/// Run `method` if it's a `member.*` one; `None` otherwise.
pub fn call(method: &str, params: Value, path: &Path, hostname: &str, exposure: Exposure) -> Option<Answer> {
    let to_value = |info: MemberInfo| serde_json::to_value(info).map_err(|e| ApiError::internal(e.into()));
    let answer = match method {
        "member.get" => get(path, hostname, exposure).and_then(to_value),
        "member.set" => parse_params(params).and_then(|p: SetParams| set(path, hostname, &p, exposure)).and_then(to_value),
        _ => return None,
    };
    Some(answer)
}

/// Write `member` at `path` (tests: what an earlier `member.set` or a hand
/// edit left).
#[cfg(test)]
pub fn write_for_test(path: &Path, member: &Member) {
    update(path, |m| *m = member.clone()).unwrap();
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::serve::fleet::identity::Signer as _;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dsv-member-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn the_path_is_next_to_state_toml() {
        assert_eq!(path_from(Some("/xdg".into()), None).unwrap(), PathBuf::from("/xdg/devsandbox/member.toml"));
        assert_eq!(
            path_from(None, Some("/home/u".into())).unwrap(),
            PathBuf::from("/home/u/.local/share/devsandbox/member.toml")
        );
    }

    #[test]
    fn parses() {
        let m = parse("name = \"lap\"\ntrust = \"full\"\nroots = [\"/srv/cfg\"]\n").unwrap();
        assert_eq!(m, Member { name: "lap".into(), trust: Trust::Full, roots: vec!["/srv/cfg".into()], ..Default::default() });
        assert_eq!(parse("").unwrap(), Member::default(), "empty: the defaults");
        assert_eq!(Member::default().trust, Trust::Worker);
        assert!(parse("trust = \"worker\"\nlater = 1\n").is_ok(), "unknown keys are ignored");
        assert!(parse("trust = \"most\"\n").is_err());
        assert!(parse("name = \"a/b\"\n").is_err());
        assert!(parse("name = \"self\"\n").is_err(), "`self` is reserved");
        assert!(parse("roots = [\"rel\"]\n").is_err(), "roots are absolute");
        // `[allow]`.
        let a = parse("[allow]\nprivileged = true\nbinds = [\"/srv/x\"]\nlater = true\n").unwrap().allow;
        assert_eq!(a, Allow { privileged: true, binds: vec!["/srv/x".into()], ..Default::default() });
        assert!(parse("[allow]\nbinds = [\"x\"]\n").is_err(), "binds are absolute");
        assert_eq!(
            serialize(&Member { allow: a, ..Default::default() }).unwrap(),
            "trust = \"worker\"\n\n[allow]\nprivileged = true\nbinds = [\"/srv/x\"]\n"
        );
        // Soft caps: sizes checked, unset ones left out of the file.
        let caps = parse("[caps]\ncpus = 4\nmem = \"16G\"\ninstances = 3\n").unwrap().caps;
        assert_eq!(caps, SoftCaps { cpus: Some(4), mem: Some("16G".into()), disk: None, instances: Some(3) });
        assert_eq!(
            serialize(&Member { caps, ..Default::default() }).unwrap(),
            "trust = \"worker\"\n\n[allow]\n\n[caps]\ncpus = 4\nmem = \"16G\"\ninstances = 3\n"
        );
        assert!(!serialize(&Member::default()).unwrap().contains("caps"));
        let bad = parse("[caps]\ndisk = \"lots\"\n").unwrap_err();
        assert!(format!("{bad:#}").contains("caps.disk"), "{bad:#}");
        assert!(parse("[caps]\ncpus = -1\n").is_err());
        // `dev-builds`: off unless set, left out of the file while off.
        assert!(!parse("").unwrap().dev_builds);
        let dev = parse("dev-builds = true\n").unwrap();
        assert!(dev.dev_builds);
        assert_eq!(serialize(&dev).unwrap(), "trust = \"worker\"\ndev-builds = true\n\n[allow]\n");
        assert!(!serialize(&Member::default()).unwrap().contains("dev-builds"));
        // Roots go before the tables.
        let rooted = Member { roots: vec!["/a".into()], ..Default::default() };
        assert_eq!(parse(&serialize(&rooted).unwrap()).unwrap(), rooted);
    }

    #[test]
    fn hub_is_self_a_name_or_none() {
        assert_eq!(parse("").unwrap().hub(), Hub::None);
        assert_eq!(parse("hub = \"self\"\n").unwrap().hub(), Hub::This);
        assert_eq!(parse("hub = \"box\"\n").unwrap().hub(), Hub::Other("box".into()));
        assert!(parse("hub = \"a/b\"\n").is_err());
        assert!(parse("hub = \"-x\"\n").is_err());
        assert_eq!(serialize(&Member { hub: "self".into(), ..Default::default() }).unwrap(), "hub = \"self\"\ntrust = \"worker\"\n\n[allow]\n");
        assert!(!serialize(&Member::default()).unwrap().contains("hub"), "no fleet: left out");
        assert_eq!((Hub::None.field(), Hub::This.field(), Hub::Other("k".into()).field()), (None, Some("self".into()), Some("k".into())));
        assert!(Hub::This.is_this() && !Hub::Other("self2".into()).is_this());
    }

    #[test]
    fn dev_builds_or_full_take_dev_builds() {
        assert!(!Member::default().takes_dev_builds());
        assert!(Member { dev_builds: true, ..Default::default() }.takes_dev_builds());
        assert!(Member { trust: Trust::Full, ..Default::default() }.takes_dev_builds());
    }

    #[test]
    fn default_names_take_the_first_label() {
        assert_eq!(default_name("lap.local").unwrap(), "lap");
        assert_eq!(default_name("box").unwrap(), "box");
        assert!(default_name("").is_err());
        assert!(default_name("a b").is_err());
        assert!(default_name("self.local").is_err(), "`self` is reserved");
        let m = Member { name: "mine".into(), ..Default::default() };
        assert_eq!(m.name_or("a b").unwrap(), "mine");
        assert_eq!(Member::default().name_or("lap.local").unwrap(), "lap");
    }

    #[test]
    fn merge_checks_what_it_changes() {
        let p = |v: Value| -> SetParams { serde_json::from_value(v).unwrap() };
        let named = merge(Member::default(), &p(json!({"name": "lap", "trust": "full"}))).unwrap();
        assert_eq!((named.name.as_str(), named.trust), ("lap", Trust::Full));
        let kept = merge(named.clone(), &p(json!({"roots_add": ["/a", "/b", "/a/"]}))).unwrap();
        assert_eq!((kept.name.as_str(), kept.trust), ("lap", Trust::Full), "the rest kept");
        assert_eq!(kept.roots, [PathBuf::from("/a"), PathBuf::from("/b")], "deduped");
        let rm = merge(kept.clone(), &p(json!({"roots_rm": ["/a", "/gone"], "roots_add": ["/c"]}))).unwrap();
        assert_eq!(rm.roots, [PathBuf::from("/b"), PathBuf::from("/c")]);
        assert_eq!(merge(rm, &p(json!({"name": ""}))).unwrap().name, "", "cleared");
        let code = |v: Value| merge(named.clone(), &p(v)).unwrap_err().code;
        assert_eq!(code(json!({"name": "a/b"})), "invalid");
        assert_eq!(code(json!({"name": "self"})), "invalid");
        assert_eq!(code(json!({"roots_add": ["rel"]})), "invalid");
        // Hand-edited parts aren't settable, nor `hub` (only an accepted
        // membership log writes it).
        for bad in [json!({"trust": "most"}), json!({"dev_builds": true}), json!({"allow": {}}), json!({"caps": {}}), json!({"hub": "self"})] {
            assert!(serde_json::from_value::<SetParams>(bad.clone()).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_wire_matches_the_typings() {
        use crate::serve::dts::{assert_matches, prop_type, union_members};
        let key = crate::serve::fleet::identity::SigningKey::from_seed([1; 32]).public_key();
        let info = MemberInfo {
            name: Some("lap".into()),
            hub: Some("self".into()),
            trust: Trust::Full,
            dev_builds: true,
            roots: vec!["/srv/cfg".into()],
            fingerprint: key.fingerprint(),
            key,
            next: Some(KeyInfo::of(&key)),
            exposed_by: Some(vec!["box".into(), "lap".into()]),
        };
        assert_matches("MemberInfo", &serde_json::to_value(&info).unwrap());
        assert_eq!(serde_json::to_value(&info).unwrap()["exposed_by"], json!(["box", "lap"]));
        assert_matches("MembershipKey", &serde_json::to_value(&info).unwrap()["next"]);
        assert_matches("MemberInfo", &serde_json::to_value(MemberInfo { hub: None, ..info }).unwrap());
        assert_eq!(union_members("MemberTrust"), ["'worker'", "'full'"]);
        assert!(prop_type("ApiMethods", "member.get").is_some());
        assert!(prop_type("ApiMethods", "member.set").is_some());
    }

    #[test]
    fn get_and_set_go_through_the_file() {
        let dir = scratch("api");
        let path = dir.join("member.toml");
        let host = "lap.local";
        // Exposed while `worker`, as the daemon's graph would say.
        let exposure = |name: &str, trust: Trust| (trust == Trust::Worker).then(|| vec!["k".to_string(), name.to_string()]);
        let call = |method: &str, params: Value| super::call(method, params, &path, host, &exposure);
        let mut got = call("member.get", Value::Null).unwrap().unwrap();
        assert!(!path.exists(), "a get writes no member.toml");
        // The first get makes the key; every answer says the same one.
        let key = FileKeyStore::new(key_path(&path)).public_key().unwrap();
        assert_eq!(key_path(&path), dir.join("member.key"));
        let key_fields = json!({"key": key.to_string(), "fingerprint": key.fingerprint()});
        for f in ["key", "fingerprint"] {
            assert_eq!(got.as_object_mut().unwrap().remove(f).unwrap(), key_fields[f], "{f}");
        }
        assert_eq!(
            got,
            json!({"name": "lap", "hub": null, "trust": "worker", "dev_builds": false, "roots": [], "next": null, "exposed_by": ["k", "lap"]})
        );
        // A pending rotation shows.
        let next = FileKeyStore::new(key_path(&path)).next_key().unwrap().public_key();
        let got = call("member.get", Value::Null).unwrap().unwrap();
        assert_eq!((&got["key"], &got["next"]), (&key_fields["key"], &json!({"key": next, "fingerprint": next.fingerprint()})));
        std::fs::remove_file(FileKeyStore::new(key_path(&path)).next_path()).unwrap();

        let set = call("member.set", json!({"trust": "full", "roots_add": ["/srv/cfg"]})).unwrap().unwrap();
        assert_eq!((&set["key"], &set["fingerprint"]), (&key_fields["key"], &key_fields["fingerprint"]));
        assert_eq!((&set["trust"], &set["roots"]), (&json!("full"), &json!(["/srv/cfg"])));
        assert_eq!(set["exposed_by"], Value::Null, "at the new trust");
        assert_eq!(load(&path).unwrap().unwrap().trust, Trust::Full, "written");
        assert!(dir.join("member.lock").exists(), "written through locked_file");

        let set = call("member.set", json!({"name": "box"}));
        assert_eq!(set.unwrap().unwrap()["name"], "box");
        assert_eq!(call("member.set", json!({"hub": "self"})).unwrap().unwrap_err().code, "invalid");
        // The log's hub, written internally; the name pinned with it.
        set_fleet(&path, Some("box2"), &Hub::This).unwrap();
        let m = load(&path).unwrap().unwrap();
        assert_eq!((m.hub(), m.name.as_str()), (Hub::This, "box2"), "written");
        assert!(set_fleet(&path, Some("a/b"), &Hub::None).is_err());
        set_fleet(&path, None, &Hub::Other("k".into())).unwrap();
        assert_eq!(call("member.get", Value::Null).unwrap().unwrap()["hub"], "k");
        set_fleet(&path, Some("box"), &Hub::None).unwrap();
        assert_eq!(load(&path).unwrap().unwrap().hub(), Hub::None, "cleared");

        // Hand edits survive a set; a refused set changes nothing.
        update(&path, |m| m.dev_builds = true).unwrap();
        let bad = call("member.set", json!({"name": "a/b"}));
        assert_eq!(bad.unwrap().unwrap_err().code, "invalid");
        assert_eq!(call("member.set", json!({"nope": 1})).unwrap().unwrap_err().code, "invalid");
        let m = load(&path).unwrap().unwrap();
        assert_eq!((m.name.as_str(), m.dev_builds, m.trust), ("box", true, Trust::Full));
        assert!(call("member.nope", Value::Null).is_none());

        // A file that doesn't parse isn't overwritten.
        std::fs::write(&path, "trust = 1\n").unwrap();
        assert_eq!(call("member.get", Value::Null).unwrap().unwrap_err().code, "internal");
        assert!(call("member.set", json!({"trust": "worker"})).unwrap().is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "trust = 1\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
