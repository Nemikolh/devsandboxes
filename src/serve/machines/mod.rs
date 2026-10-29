//! Machines: remote hosts whose daemons this one talks to over the `API`
//! streams of one link per machine (docs/architecture/daemon/machines.md):
//! for an ssh machine the link this host holds to it
//! (`ssh <target> devsandbox fleet-link --peer <this host>`,
//! `fleet::dialer`), for an offered fleet worker the one it holds to this
//! host (docs/architecture/daemon/fleet.md); either way reached at
//! `fleet-<name>.sock` (`fleet::dialer::dial`).
//!
//! - this module: the registry, `machines.toml` next to `state.toml`
//! - `ssh`: the add-time ssh argvs and quoting, and the injectable
//!   [`ssh::Dial`] that opens a [`ssh::Duplex`] to a machine
//! - `link`: a connection to a machine's daemon (the hello)
//! - `service`: the held link per machine, its rows and forwarded calls
//! - `api`: the `machines.*` methods
//! - `forward`: requests with `machine` (quick calls, verbs) and the
//!   federated resolve and sandboxes listing
//! - `cli`: the CLI's side: where a `<name>` points, `--machine`, and a
//!   command's calls and `PTY` dials over the machine's link
//! - `install`: the CLI's check of a machine's devsandbox and the copy of
//!   this binary there
//! - `saved`: a hub's `remotes.json`, the machines' last known rows
//!
//! Only the daemon writes the registry (`machines.add` / `set` / `rm`);
//! the CLI reads it later to know whether any machine exists. Two
//! daemons can briefly overlap during a handoff, so every write is a
//! read-modify-write under an exclusive lock on the sibling `machines.lock`,
//! written to a temp file and renamed (`serve::locked_file`).

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub mod api;
pub mod cli;
pub mod forward;
pub mod inbound;
pub mod install;
pub mod link;
pub mod saved;
pub mod service;
pub mod ssh;

/// One registered machine, as `machines.toml` holds it under
/// `[machine.<name>]` (docs/architecture/daemon/machines.md, *Registry:
/// `machines.toml`*). In the file the kind is `target = "…"` (ssh) or
/// `offered = true`, exactly one of them ([`RawEntry`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawEntry", into = "RawEntry")]
pub struct Entry {
    /// The table key, so not a field of its own in the file.
    pub name: String,
    /// How this host reaches the machine.
    pub kind: Kind,
    /// Config roots on the machine, as the remote canonicalized them.
    pub roots: Vec<PathBuf>,
    /// The remote devsandbox as shell text (`~/bin/devsandbox`); `None` is
    /// `devsandbox` on the login shell's `PATH`.
    pub command: Option<String>,
    /// This host's ssh-agent answers the machine's `AGENT` streams
    /// (docs/architecture/daemon/machines.md, *Agent*); off by default, and
    /// only written when on.
    pub agent: bool,
}

/// How a machine is reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// This host sshes to it: `target` is what ssh takes (`me@host`, `host`,
    /// an ssh-config alias) or an `ssh://` URL ([`ssh::parse_target`]).
    Ssh { target: String },
    /// A fleet worker that dialed this host (docs/architecture/daemon/fleet.md,
    /// *Hub side*): reached only over the `API` channel of its held fleet
    /// link, through `fleet-<name>.sock`; nothing here can ssh to it.
    Offered,
}

/// [`Kind`] on the wire (`kind` of `machines.list` / `get` and the
/// federated snapshot's `machines`, next to a `target` that is `null` for an
/// offered machine and an inbound neighbor; `inbound` only in
/// `machines.list`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MachineKind {
    #[default]
    Ssh,
    Offered,
    /// An inbound neighbor (`inbound`): a machine whose link to this host
    /// is dialed from its end, listed by `machines.list` while it's up;
    /// never a registry entry.
    Inbound,
}

impl Kind {
    /// The wire's `kind` and `target`.
    pub fn wire(&self) -> (MachineKind, Option<String>) {
        match self {
            Kind::Ssh { target } => (MachineKind::Ssh, Some(target.clone())),
            Kind::Offered => (MachineKind::Offered, None),
        }
    }

    /// Back from the wire's `kind` and `target`: an ssh machine needs its
    /// target.
    pub fn from_wire(kind: MachineKind, target: Option<String>) -> Result<Kind> {
        match (kind, target) {
            (MachineKind::Ssh, Some(target)) => Ok(Kind::Ssh { target }),
            (MachineKind::Ssh, None) => bail!("an ssh machine without a target"),
            (MachineKind::Offered, _) => Ok(Kind::Offered),
            (MachineKind::Inbound, _) => bail!("an inbound neighbor isn't a registry entry"),
        }
    }
}

impl Entry {
    /// An ssh machine with no roots yet, `command` and `agent` unset.
    pub fn ssh(name: impl Into<String>, target: impl Into<String>) -> Entry {
        Entry { name: name.into(), kind: Kind::Ssh { target: target.into() }, roots: Vec::new(), command: None, agent: false }
    }

    /// An offered fleet worker with no roots (`fleet.offer` adds its fleet
    /// root: `fleet::offer::accept`).
    pub fn offered(name: impl Into<String>) -> Entry {
        Entry { name: name.into(), kind: Kind::Offered, roots: Vec::new(), command: None, agent: false }
    }

    pub fn command(&self) -> &str {
        self.command.as_deref().unwrap_or("devsandbox")
    }

    /// The ssh target; `None` for an offered machine.
    pub fn target(&self) -> Option<&str> {
        match &self.kind {
            Kind::Ssh { target } => Some(target),
            Kind::Offered => None,
        }
    }

    pub fn is_offered(&self) -> bool {
        self.kind == Kind::Offered
    }

    /// What messages and listings call where it is: the target, or
    /// `offered`.
    pub fn label(&self) -> &str {
        self.target().unwrap_or("offered")
    }

    /// The ssh target, for the add-time argv builders ([`ssh`]): an offered
    /// machine has none.
    pub fn ssh_target(&self) -> Result<&str> {
        let name = &self.name;
        self.target().with_context(|| format!("machine `{name}` is an offered fleet worker, reached over its fleet link, not ssh"))
    }
}

/// [`Entry`] as the file holds it.
#[derive(Serialize, Deserialize)]
struct RawEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    offered: bool,
    #[serde(default)]
    roots: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    command: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    agent: bool,
}

impl TryFrom<RawEntry> for Entry {
    type Error = String;

    /// The name is the table key: [`parse`] fills it in.
    fn try_from(raw: RawEntry) -> Result<Entry, String> {
        let kind = match (raw.target, raw.offered) {
            (Some(target), false) => Kind::Ssh { target },
            (None, true) => Kind::Offered,
            (Some(_), true) => return Err("`target` and `offered = true` both set".into()),
            (None, false) => return Err("no `target` (or `offered = true`)".into()),
        };
        Ok(Entry { name: String::new(), kind, roots: raw.roots, command: raw.command, agent: raw.agent })
    }
}

impl From<Entry> for RawEntry {
    fn from(e: Entry) -> RawEntry {
        let (target, offered) = match e.kind {
            Kind::Ssh { target } => (Some(target), false),
            Kind::Offered => (None, true),
        };
        RawEntry { target, offered, roots: e.roots, command: e.command, agent: e.agent }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Doc {
    #[serde(default, rename = "machine")]
    machines: BTreeMap<String, Entry>,
}

/// [`path_from`] over the process env.
pub fn path() -> Result<PathBuf> {
    path_from(std::env::var_os("XDG_DATA_HOME"), std::env::var_os("HOME"))
}

/// `<data>/devsandbox/machines.toml`.
pub fn path_from(xdg_data_home: Option<OsString>, home: Option<OsString>) -> Result<PathBuf> {
    Ok(crate::daemon_config::data_dir_from(xdg_data_home, home)?.join("machines.toml"))
}

/// The entries, sorted by name. A hand-edited name that breaks
/// [`check_name`] fails the whole file: a qualified `<machine>/<name>`
/// could not address it.
pub fn parse(text: &str) -> Result<Vec<Entry>> {
    let doc: Doc = toml::from_str(text)?;
    doc.machines
        .into_iter()
        .map(|(name, entry)| {
            check_name(&name)?;
            Ok(Entry { name, ..entry })
        })
        .collect()
}

pub fn serialize(entries: &[Entry]) -> Result<String> {
    let doc = Doc { machines: entries.iter().map(|e| (e.name.clone(), e.clone())).collect() };
    toml::to_string_pretty(&doc).context("cannot serialize machines")
}

/// The registered machines; a missing file is none.
pub fn load(path: &Path) -> Result<Vec<Entry>> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text).with_context(|| format!("invalid {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// Read-modify-write under the exclusive lock; saves only when `f` changed
/// the entries. A file that doesn't parse fails the update rather than being
/// overwritten.
pub fn update<T>(path: &Path, f: impl FnOnce(&mut Vec<Entry>) -> T) -> Result<T> {
    super::locked_file::update(path, load, |e| serialize(e), f)
}

/// A machine name is `[A-Za-z0-9._-]+`, not starting with `.` or `-`. Never
/// `/` or `@`: `my-server/web-2` splits at the first `/`, and `@/web-2` is
/// this host. No leading `-`, so it can't read as a flag either.
pub fn check_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("empty machine name");
    }
    if name.starts_with(['.', '-']) {
        bail!("machine name `{name}` starts with `{}`", &name[..1]);
    }
    if let Some(c) = name.chars().find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))) {
        bail!("machine name `{}` has `{}` (letters, digits, `.`, `_` and `-` only)", name.escape_debug(), c.escape_debug());
    }
    Ok(())
}

/// Where a CLI `<name>` points (docs/architecture/daemon/machines.md,
/// *Registry: `machines.toml`*).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Where {
    /// `@/<name>`: this host only.
    Local,
    /// `<machine>/<name>`: that machine only.
    Machine(String),
    /// A bare name: this host and every machine.
    Any,
}

/// Split a CLI `<name>` into where it points and the name there. Docker
/// container names can't hold `/` or `@`, so neither can a working instance
/// or sandbox name: any `/` qualifies, at the first one, and a machine
/// part that isn't a valid machine name is an error rather than a bare name
/// that could never match.
pub fn parse_name(arg: &str) -> Result<(Where, String)> {
    let Some((machine, name)) = arg.split_once('/') else { return Ok((Where::Any, arg.to_string())) };
    if name.is_empty() {
        bail!("`{arg}`: no name after `/`");
    }
    if machine == "@" {
        return Ok((Where::Local, name.to_string()));
    }
    check_name(machine).with_context(|| format!("`{arg}` isn't `<machine>/<name>` or `@/<name>`"))?;
    Ok((Where::Machine(machine.to_string()), name.to_string()))
}

/// The registered machines' names, from the file: the one place the
/// CLI reads the daemon's registry itself, so that without machines no
/// command contacts a daemon it didn't before. Missing, empty or unreadable
/// is none.
pub fn registered_names() -> Vec<String> {
    path().and_then(|p| load(&p)).map(|entries| entries.into_iter().map(|e| e.name).collect()).unwrap_or_default()
}

/// The error for a machine nobody registered.
pub fn no_machine(name: &str) -> anyhow::Error {
    anyhow::anyhow!("no machine named `{name}` (devsandbox machine ls)")
}

/// The name `machine add` gives a target without `--name`: its host
/// (`me@my-server.com` → `my-server.com`, `ssh://me@box:2200` → `box`, an
/// ssh-config alias as is). A host that isn't a valid name (an IPv6
/// address, ssh's own `host:port`) is an error that asks for `--name`.
pub fn default_name(target: &str) -> Result<String> {
    let host = ssh::parse_target(target)?.host;
    let host = host.rsplit_once('@').map_or(host.as_str(), |(_, h)| h);
    match check_name(host) {
        Ok(()) => Ok(host.to_string()),
        Err(e) => bail!("cannot name a machine after `{host}` ({e:#}); pass --name"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, target: &str) -> Entry {
        Entry { roots: vec!["/srv/a".into()], ..Entry::ssh(name, target) }
    }

    #[test]
    fn the_path_is_next_to_state_toml() {
        let p = path_from(Some("/xdg".into()), Some("/home/u".into())).unwrap();
        assert_eq!(p, PathBuf::from("/xdg/devsandbox/machines.toml"));
        let p = path_from(None, Some("/home/u".into())).unwrap();
        assert_eq!(p, PathBuf::from("/home/u/.local/share/devsandbox/machines.toml"));
        assert!(path_from(None, None).is_err());
    }

    #[test]
    fn entries_round_trip_sorted_by_name() {
        let entries = vec![
            Entry { command: Some("~/bin/devsandbox".into()), roots: vec!["/a".into(), "/b".into()], ..entry("b-box", "ssh://me@b:2200") },
            Entry { agent: true, ..entry("a.box", "me@a") },
        ];
        let text = serialize(&entries).unwrap();
        assert!(text.contains("[machine.\"a.box\"]") && text.contains("[machine.b-box]"), "{text}");
        // `command` and `agent` only when set; the name is the key, not a field.
        assert_eq!(text.matches("command").count(), 1, "{text}");
        assert_eq!(text.matches("agent = true").count(), 1, "{text}");
        assert!(!text.contains("agent = false"), "{text}");
        assert!(!text.contains("name"), "{text}");
        let mut want = entries.clone();
        want.reverse();
        assert_eq!(parse(&text).unwrap(), want);
        assert_eq!(parse("").unwrap(), vec![]);
        let minimal = parse("[machine.m]\ntarget = \"m\"\n").unwrap();
        assert_eq!(minimal, vec![Entry::ssh("m", "m")]);
        assert!(parse("[machine.\"a/b\"]\ntarget = \"m\"\n").is_err(), "a bad name fails the file");
        assert!(parse("[machine.m]\n").is_err(), "no target");
    }

    #[test]
    fn offered_entries_round_trip_as_offered_true() {
        let entries = vec![Entry::offered("lap"), entry("srv", "me@srv")];
        let text = serialize(&entries).unwrap();
        assert_eq!(text.matches("offered = true").count(), 1, "{text}");
        assert_eq!(text.matches("target").count(), 1, "{text}");
        assert!(!text.contains("offered = false"), "{text}");
        assert_eq!(parse(&text).unwrap(), entries);
        assert_eq!(parse("[machine.lap]\noffered = true\n").unwrap(), vec![Entry::offered("lap")]);
        let both = parse("[machine.m]\ntarget = \"m\"\noffered = true\n").unwrap_err();
        assert!(format!("{both:#}").contains("both set"), "{both:#}");
        assert!(parse("[machine.m]\noffered = false\n").is_err(), "neither");

        let lap = Entry::offered("lap");
        assert_eq!((lap.target(), lap.label(), lap.is_offered()), (None, "offered", true));
        assert!(format!("{:#}", lap.ssh_target().unwrap_err()).contains("reached over its fleet link, not ssh"));
        assert_eq!(entry("srv", "me@srv").ssh_target().unwrap(), "me@srv");
        assert_eq!(Kind::Offered.wire(), (MachineKind::Offered, None));
        assert_eq!(Kind::from_wire(MachineKind::Ssh, Some("t".into())).unwrap(), Kind::Ssh { target: "t".into() });
        assert_eq!(Kind::from_wire(MachineKind::Offered, None).unwrap(), Kind::Offered);
        assert!(Kind::from_wire(MachineKind::Ssh, None).is_err());
        assert!(Kind::from_wire(MachineKind::Inbound, None).is_err());
    }

    #[test]
    fn update_writes_only_changes_and_keeps_a_bad_file() {
        let dir = std::env::temp_dir().join(format!("devsandbox-machines-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("machines.toml");
        assert_eq!(load(&path).unwrap(), vec![]);
        update(&path, |e| e.push(entry("a", "a"))).unwrap();
        update(&path, |e| e.push(entry("b", "b"))).unwrap();
        assert!(dir.join("machines.lock").exists());
        update(&path, |e| e.retain(|e| e.name != "a")).unwrap();
        assert_eq!(load(&path).unwrap(), vec![entry("b", "b")]);
        // A no-op doesn't write: no file appears where there was none.
        let other = dir.join("sub/machines.toml");
        update(&other, |_| ()).unwrap();
        assert!(!other.exists());
        // No temp file is left behind.
        let leftovers: Vec<_> = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).filter(|e| e.file_name().to_string_lossy().ends_with(".tmp")).collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");

        std::fs::write(&path, "[machine.m\n").unwrap();
        assert!(update(&path, |e| e.clear()).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[machine.m\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_updates_lose_nothing() {
        let dir = std::env::temp_dir().join(format!("devsandbox-machines-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("machines.toml");
        let writers: Vec<_> = (0..8)
            .map(|i| {
                let path = path.clone();
                std::thread::spawn(move || update(&path, |e| e.push(entry(&format!("m{i}"), "t"))).unwrap())
            })
            .collect();
        for w in writers {
            w.join().unwrap();
        }
        assert_eq!(load(&path).unwrap().len(), 8);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn names_are_plain_words() {
        for good in ["my-server", "my-server.com", "a", "A_1", "10.0.0.1", "box-"] {
            assert!(check_name(good).is_ok(), "{good}");
        }
        for bad in ["", ".hidden", "-x", "a/b", "@", "me@box", "a b", "a:22", "é"] {
            assert!(check_name(bad).is_err(), "{bad:?} passed");
        }
    }

    #[test]
    fn names_parse_qualified_local_or_bare() {
        let p = |s: &str| parse_name(s).unwrap();
        assert_eq!(p("web-2"), (Where::Any, "web-2".into()));
        assert_eq!(p("@/web-2"), (Where::Local, "web-2".into()));
        assert_eq!(p("my-server/web-2"), (Where::Machine("my-server".into()), "web-2".into()));
        // Split at the first `/`: the rest is the name, whatever it holds.
        assert_eq!(p("a/b/c"), (Where::Machine("a".into()), "b/c".into()));
        assert_eq!(p("a/devsandbox-web"), (Where::Machine("a".into()), "devsandbox-web".into()));
        for bad in ["a/", "@/", "/web", "me@box/web", "a b/web", "-x/web", "@@/web"] {
            assert!(parse_name(bad).is_err(), "{bad:?} parsed");
        }
        assert_eq!(no_machine("m").to_string(), "no machine named `m` (devsandbox machine ls)");
    }

    #[test]
    fn default_names_are_the_host() {
        assert_eq!(default_name("me@my-server.com").unwrap(), "my-server.com");
        assert_eq!(default_name("ssh://me@box:2200").unwrap(), "box");
        assert_eq!(default_name("ssh://box").unwrap(), "box");
        assert_eq!(default_name("my-alias").unwrap(), "my-alias");
        assert_eq!(default_name("10.0.0.5").unwrap(), "10.0.0.5");
        for (target, why) in [("ssh://[::1]:22", "pass --name"), ("me@host:22", "pass --name"), ("-x", "starts with `-`")] {
            let err = format!("{:#}", default_name(target).unwrap_err());
            assert!(err.contains(why), "{target}: {err}");
        }
    }
}
