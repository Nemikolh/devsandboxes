//! `member.toml`, next to `state.toml`: this host as a fleet member
//! (docs/architecture/daemon/fleet.md, *Member file*): the name it goes by,
//! its trust, the config roots it shows the fleet, and the hand-edited
//! `[allow]`, `[caps]` and `dev-builds`.
//!
//! Written by the daemon only (decision 3 of the fleet plan), through
//! `member.set` ([`call`]; `devsandbox machine trust` and `machine roots`
//! are its clients), with [`update`]: under the sibling
//! `member.lock`, a temp file and a rename (`serve::locked_file`).
//! `[allow]`, `[caps]` and `dev-builds` have no call: they're edited by
//! hand and re-read at each use (`worker_policy` reads `trust` and
//! `[allow]` itself, on every platform).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::serve::api::{ApiError, parse as parse_params};
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

impl Member {
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

/// `text` as a [`Member`], [`check`]ed.
pub fn parse(text: &str) -> Result<Member> {
    let member: Member = toml::from_str(text)?;
    check(&member)?;
    Ok(member)
}

/// A `name` that's set is a valid machine name, `roots` are absolute, and
/// `[caps]` and `[allow]` are well-formed.
pub fn check(member: &Member) -> Result<()> {
    if !member.name.is_empty() {
        check_name(&member.name).context("`name`")?;
    }
    if let Some(bad) = member.roots.iter().find(|r| !r.is_absolute()) {
        bail!("`roots`: `{}` is not an absolute path", bad.display());
    }
    member.caps.check()?;
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
/// (`lap.local` → `lap`), if it's a valid machine name.
pub fn default_name(hostname: &str) -> Result<String> {
    let label = hostname.split('.').next().unwrap_or_default();
    match check_name(label) {
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
    pub trust: Trust,
    /// `dev-builds` as the file says (`full` takes dev builds either way).
    pub dev_builds: bool,
    pub roots: Vec<PathBuf>,
}

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
            check_name(name).map_err(|e| ApiError::invalid(format!("`name`: {e:#}")))?;
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

fn info(member: &Member, hostname: &str) -> MemberInfo {
    MemberInfo {
        name: member.name_or(hostname).ok(),
        trust: member.trust,
        dev_builds: member.dev_builds,
        roots: member.roots.clone(),
    }
}

/// `member.get` over the file at `path`; `hostname` names this host when
/// the file doesn't.
pub fn get(path: &Path, hostname: &str) -> Result<MemberInfo, ApiError> {
    let member = load(path).map_err(ApiError::internal)?.unwrap_or_default();
    Ok(info(&member, hostname))
}

/// `member.set`: `p` merged into the file, checked, written. Nothing else
/// follows it: a peer link reads the name at its next dial
/// (`fleet::dialer::member_name`), the rest is read at each use.
pub fn set(path: &Path, hostname: &str, p: &SetParams) -> Result<MemberInfo, ApiError> {
    let after = update(path, |cur| -> Result<Member, ApiError> {
        *cur = merge(cur.clone(), p)?;
        Ok(cur.clone())
    })
    .map_err(ApiError::internal)??;
    Ok(info(&after, hostname))
}

/// Run `method` if it's a `member.*` one; `None` otherwise.
pub fn call(method: &str, params: Value, path: &Path, hostname: &str) -> Option<Answer> {
    let to_value = |info: MemberInfo| serde_json::to_value(info).map_err(|e| ApiError::internal(e.into()));
    let answer = match method {
        "member.get" => get(path, hostname).and_then(to_value),
        "member.set" => parse_params(params).and_then(|p: SetParams| set(path, hostname, &p)).and_then(to_value),
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
        assert_eq!(code(json!({"roots_add": ["rel"]})), "invalid");
        // Hand-edited parts aren't settable.
        for bad in [json!({"trust": "most"}), json!({"dev_builds": true}), json!({"allow": {}}), json!({"caps": {}})] {
            assert!(serde_json::from_value::<SetParams>(bad.clone()).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_wire_matches_the_typings() {
        use crate::serve::dts::{assert_matches, prop_type, union_members};
        let info = MemberInfo { name: Some("lap".into()), trust: Trust::Full, dev_builds: true, roots: vec!["/srv/cfg".into()] };
        assert_matches("MemberInfo", &serde_json::to_value(info).unwrap());
        assert_eq!(union_members("MemberTrust"), ["'worker'", "'full'"]);
        assert!(prop_type("ApiMethods", "member.get").is_some());
        assert!(prop_type("ApiMethods", "member.set").is_some());
    }

    #[test]
    fn get_and_set_go_through_the_file() {
        let dir = scratch("api");
        let path = dir.join("member.toml");
        let host = "lap.local";
        let call = |method: &str, params: Value| super::call(method, params, &path, host);
        let got = call("member.get", Value::Null);
        assert_eq!(got.unwrap().unwrap(), json!({"name": "lap", "trust": "worker", "dev_builds": false, "roots": []}));
        assert!(!path.exists(), "a get writes nothing");

        let set = call("member.set", json!({"trust": "full", "roots_add": ["/srv/cfg"]}));
        assert_eq!(set.unwrap().unwrap(), json!({"name": "lap", "trust": "full", "dev_builds": false, "roots": ["/srv/cfg"]}));
        assert_eq!(load(&path).unwrap().unwrap().trust, Trust::Full, "written");
        assert!(dir.join("member.lock").exists(), "written through locked_file");

        let set = call("member.set", json!({"name": "box"}));
        assert_eq!(set.unwrap().unwrap()["name"], "box");

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
