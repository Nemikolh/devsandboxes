//! Machines: remote hosts whose daemons this one talks to over
//! `ssh <target> devsandbox api --stdio` (docs/machines.md).
//!
//! - this module: the registry, `machines.toml` next to `state.toml`
//! - `ssh`: the ssh argv and quoting, and the injectable [`ssh::Dial`] that
//!   opens a [`ssh::Duplex`] to a machine
//! - `link`: a connection to a machine's daemon (the hello)
//! - `service`: the held link per machine, its rows and forwarded calls
//! - `api`: the `machines.*` methods
//! - `forward`: requests with `machine` (quick calls, verbs) and the
//!   federated resolve and sandboxes listing
//!
//! Only the daemon writes the registry (`machines.add` / `machines.rm`);
//! the CLI reads it later to know whether any machine exists (F15). Two
//! daemons can briefly overlap during a handoff, so every write is a
//! read-modify-write under an exclusive lock on the sibling `machines.lock`,
//! written to a temp file and renamed, as `forward_store` does.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub mod api;
pub mod forward;
pub mod link;
pub mod service;
pub mod ssh;

/// One registered machine, as `machines.toml` holds it under
/// `[machine.<name>]` (docs/machines.md, F2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// The table key, so not a field of its own in the file.
    #[serde(skip)]
    pub name: String,
    /// What ssh takes (`me@host`, `host`, an ssh-config alias) or an
    /// `ssh://` URL ([`ssh::parse_target`]).
    pub target: String,
    /// Config roots on the machine, as the remote canonicalized them.
    #[serde(default)]
    pub roots: Vec<PathBuf>,
    /// The remote devsandbox as shell text (`~/bin/devsandbox`); `None` is
    /// `devsandbox` on the login shell's `PATH`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

impl Entry {
    pub fn command(&self) -> &str {
        self.command.as_deref().unwrap_or("devsandbox")
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
    let dir = path.parent().context("machines path has no parent")?;
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let _lock = lock(path)?;
    let mut entries = load(path)?;
    let before = entries.clone();
    let out = f(&mut entries);
    if entries != before {
        write_atomic(path, &serialize(&entries)?)?;
    }
    Ok(out)
}

/// Separate from `machines.toml`, which the atomic write replaces.
fn lock(path: &Path) -> Result<File> {
    let lock_path = path.with_file_name("machines.lock");
    let file = File::options()
        .create(true)
        .read(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("cannot open {}", lock_path.display()))?;
    file.lock().with_context(|| format!("cannot lock {}", lock_path.display()))?;
    Ok(file)
}

/// Temp file + rename; the pid in the temp name keeps a killed writer's stale
/// temp from being reused by the other daemon.
fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let tmp = path.with_file_name(format!(".machines.toml.{}.tmp", std::process::id()));
    std::fs::write(&tmp, text).with_context(|| format!("cannot write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("cannot write {}", path.display()))
}

/// A machine name is `[A-Za-z0-9._-]+`, not starting with `.` or `-`. Never
/// `/` or `@`: `my-server/web-2` splits at the first `/`, and `@/web-2` is
/// this host (F2). No leading `-`, so it can't read as a flag either.
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
        Entry { name: name.into(), target: target.into(), roots: vec!["/srv/a".into()], command: None }
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
            entry("a.box", "me@a"),
        ];
        let text = serialize(&entries).unwrap();
        assert!(text.contains("[machine.\"a.box\"]") && text.contains("[machine.b-box]"), "{text}");
        // `command` only when set; the name is the key, not a field.
        assert_eq!(text.matches("command").count(), 1, "{text}");
        assert!(!text.contains("name"), "{text}");
        let mut want = entries.clone();
        want.reverse();
        assert_eq!(parse(&text).unwrap(), want);
        assert_eq!(parse("").unwrap(), vec![]);
        let minimal = parse("[machine.m]\ntarget = \"m\"\n").unwrap();
        assert_eq!(minimal, vec![Entry { name: "m".into(), target: "m".into(), roots: vec![], command: None }]);
        assert!(parse("[machine.\"a/b\"]\ntarget = \"m\"\n").is_err(), "a bad name fails the file");
        assert!(parse("[machine.m]\n").is_err(), "no target");
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
