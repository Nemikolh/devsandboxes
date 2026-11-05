//! `placements.json`, next to `state.toml`: the runs this host placed on
//! other machines (docs/architecture/daemon/fleet.md, *Fetch-back*; fleet
//! plan decision 76). Fetch-back lands a placed instance's work in the local
//! checkout its placement came from, and that checkout is known only from
//! here, never from what the worker says.
//!
//! Only the daemon writes it: `placed_run` adds an entry once a forwarded
//! run's answer names its instance (a repeat placement of the same name on
//! the same machine replaces it). Nothing prunes it yet. Every write is a
//! read-modify-write under an exclusive lock on the sibling
//! `placements.lock`, written to a temp file and renamed
//! (`serve::locked_file`), as two daemons can overlap during a handoff.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 1;

/// One placed run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// The worker, by the name this host knows it by (`Placed::Worker`'s).
    pub machine: String,
    /// The instance's name there (the run's answer).
    pub instance: String,
    pub sandbox: String,
    /// The local config root the run was typed against.
    pub root: PathBuf,
    /// The local checkout of the sandbox's `folder` the code came from.
    pub folder: PathBuf,
    /// Where that folder is on the worker.
    pub remote: PathBuf,
    /// The repo's identity (`code::repo::identity`) when it was sent.
    pub repo: String,
    /// The branch sent.
    pub branch: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Doc {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    placements: Vec<Entry>,
}

/// `placements.json` next to the `state.toml` at `state`.
pub fn path_next_to(state: &Path) -> PathBuf {
    state.with_file_name("placements.json")
}

pub fn parse(text: &str) -> Result<Vec<Entry>> {
    let doc: Doc = serde_json::from_str(text)?;
    if doc.version != VERSION {
        bail!("unsupported version {} (expected {VERSION})", doc.version);
    }
    Ok(doc.placements)
}

pub fn serialize(entries: &[Entry]) -> Result<String> {
    let mut text = serde_json::to_string_pretty(&Doc { version: VERSION, placements: entries.to_vec() })
        .context("cannot serialize placements")?;
    text.push('\n');
    Ok(text)
}

/// The saved entries; a missing file is none.
pub fn load(path: &Path) -> Result<Vec<Entry>> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text).with_context(|| format!("invalid {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// Read-modify-write under the exclusive lock; saves only when `f` changed
/// the entries. A file that doesn't parse fails the update rather than
/// being overwritten.
pub fn update<T>(path: &Path, f: impl FnOnce(&mut Vec<Entry>) -> T) -> Result<T> {
    crate::serve::locked_file::update(path, load, |e| serialize(e), f)
}

/// Add `entry`, replacing one of the same machine and instance.
pub fn record(entries: &mut Vec<Entry>, entry: Entry) {
    entries.retain(|e| !(e.machine == entry.machine && e.instance == entry.instance));
    entries.push(entry);
}

/// The entry `machine/instance` names; with no `machine`, the one placed
/// instance of that name, if exactly one. Why none, for the caller.
pub fn find<'a>(entries: &'a [Entry], machine: Option<&str>, instance: &str) -> Result<&'a Entry, String> {
    let matches: Vec<&Entry> = entries.iter().filter(|e| e.instance == instance && machine.is_none_or(|m| e.machine == m)).collect();
    match (matches.as_slice(), machine) {
        ([one], _) => Ok(one),
        ([], Some(m)) => Err(format!("this host didn't place `{m}/{instance}`")),
        ([], None) => Err(format!("this host placed no instance `{instance}`")),
        (many, _) => {
            let names: Vec<String> = many.iter().map(|e| format!("{}/{}", e.machine, e.instance)).collect();
            Err(format!("`{instance}` is ambiguous: {}", names.join(", ")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(machine: &str, instance: &str) -> Entry {
        Entry {
            machine: machine.into(),
            instance: instance.into(),
            sandbox: "web".into(),
            root: "/w/cfg".into(),
            folder: "/w/web".into(),
            remote: "/d/fleet/hub/roots/0123456789abcdef/web".into(),
            repo: "fedcba9876543210".into(),
            branch: "main".into(),
        }
    }

    #[test]
    fn the_path_is_next_to_state_toml() {
        assert_eq!(path_next_to(Path::new("/d/devsandbox/state.toml")), PathBuf::from("/d/devsandbox/placements.json"));
    }

    #[test]
    fn entries_round_trip_and_the_version_is_checked() {
        let entries = vec![entry("lap", "web-2"), entry("box", "web")];
        let text = serialize(&entries).unwrap();
        assert!(text.contains("\"version\": 1"), "{text}");
        assert_eq!(parse(&text).unwrap(), entries);
        assert_eq!(parse("{\"version\": 1}").unwrap(), vec![]);
        assert!(parse("{\"version\": 2, \"placements\": []}").is_err());
        assert!(parse("{}").is_err(), "no version");
        assert!(parse("not json").is_err());
    }

    #[test]
    fn update_records_replaces_and_keeps_a_bad_file() {
        let dir = std::env::temp_dir().join(format!("devsandbox-placements-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("placements.json");
        assert_eq!(load(&path).unwrap(), vec![]);
        update(&path, |e| record(e, entry("lap", "web-2"))).unwrap();
        update(&path, |e| record(e, entry("box", "web-2"))).unwrap();
        update(&path, |e| record(e, Entry { branch: "feature".into(), ..entry("lap", "web-2") })).unwrap();
        let got = load(&path).unwrap();
        assert_eq!(got, vec![entry("box", "web-2"), Entry { branch: "feature".into(), ..entry("lap", "web-2") }]);
        // A no-op doesn't write.
        let other = dir.join("sub/placements.json");
        update(&other, |_| ()).unwrap();
        assert!(!other.exists());
        std::fs::write(&path, "{\"version\": 9}").unwrap();
        assert!(update(&path, |e| e.clear()).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"version\": 9}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_takes_a_qualified_or_an_unambiguous_bare_name() {
        let entries = vec![entry("lap", "web-2"), entry("box", "web-2"), entry("box", "api")];
        assert_eq!(find(&entries, Some("box"), "web-2").unwrap(), &entries[1]);
        assert_eq!(find(&entries, None, "api").unwrap(), &entries[2]);
        assert_eq!(find(&entries, None, "web-2").unwrap_err(), "`web-2` is ambiguous: lap/web-2, box/web-2");
        assert_eq!(find(&entries, Some("lap"), "api").unwrap_err(), "this host didn't place `lap/api`");
        assert_eq!(find(&entries, None, "x").unwrap_err(), "this host placed no instance `x`");
    }
}
