//! `forwards.toml`, next to `state.toml`: the daemon's ad-hoc forwards
//! (`forwards.add`), so a successor daemon (handoff, restart, boot) recreates
//! them on the host ports they had (`docs/architecture/daemon/forwards.md`,
//! *Ad-hoc forwards and `forwards.toml`*). Configured forwards aren't stored
//! here: their ports live in `state.toml` and they come back through the
//! registry's sync.
//!
//! Only the daemon writes it: `forwards.add` appends, `forwards.rm` removes,
//! the restore at registry start drops entries of removed instances (and of
//! machines no longer registered) and records fallback ports; the registry's
//! sync drops a removed machine's entries too. A daemon exiting doesn't touch it — dropping its
//! forwards is not a removal. Two daemons can briefly overlap during a
//! handoff, so every write is a read-modify-write under an exclusive lock on
//! the sibling `forwards.lock`, written to a temp file and renamed
//! (`serve::locked_file`).

use std::ffi::OsString;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 1;

/// One ad-hoc forward, as `forwards.add` asked for it, with the host port it
/// actually bound (also when the request let the port be chosen).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Canonical config root; with `machine`, the root on that machine as
    /// sent (never canonicalized here).
    pub dir: PathBuf,
    /// Instance state key; `None` for a service-only forward. With
    /// `machine`, the instance's name there (its `tunnel` resolves it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    /// Bind address; `None` is the default `127.0.0.1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<IpAddr>,
    pub container_port: u16,
    pub host_port: u16,
    /// The registered machine the forward reaches into (an ssh per bridge);
    /// `None` for this host's instances and services.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
}

impl Entry {
    pub fn bind(&self) -> IpAddr {
        self.address.unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST))
    }

    /// `bind` as stored: the default address is left out.
    pub fn address_of(bind: IpAddr) -> Option<IpAddr> {
        (bind != IpAddr::V4(Ipv4Addr::LOCALHOST)).then_some(bind)
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Doc {
    #[serde(default)]
    version: u32,
    #[serde(default, rename = "forward")]
    forwards: Vec<Entry>,
}

/// [`path_from`] over the process env.
pub fn path() -> Result<PathBuf> {
    path_from(std::env::var_os("XDG_DATA_HOME"), std::env::var_os("HOME"))
}

/// `<data>/devsandbox/forwards.toml`.
pub fn path_from(xdg_data_home: Option<OsString>, home: Option<OsString>) -> Result<PathBuf> {
    Ok(crate::daemon_config::data_dir_from(xdg_data_home, home)?.join("forwards.toml"))
}

pub fn parse(text: &str) -> Result<Vec<Entry>> {
    let file: Doc = toml::from_str(text)?;
    if file.version != VERSION {
        bail!("unsupported version {} (expected {VERSION})", file.version);
    }
    Ok(file.forwards)
}

pub fn serialize(entries: &[Entry]) -> Result<String> {
    let file = Doc { version: VERSION, forwards: entries.to_vec() };
    toml::to_string_pretty(&file).context("cannot serialize forwards")
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
/// the entries. A file that doesn't parse fails the update rather than being
/// overwritten.
pub fn update<T>(path: &Path, f: impl FnOnce(&mut Vec<Entry>) -> T) -> Result<T> {
    super::locked_file::update(path, load, |e| serialize(e), f)
}

/// Remove the first entry equal to `entry`. Returns whether one was there.
pub fn remove(entries: &mut Vec<Entry>, entry: &Entry) -> bool {
    match entries.iter().position(|e| e == entry) {
        Some(i) => {
            entries.remove(i);
            true
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(instance: Option<&str>, host_port: u16) -> Entry {
        Entry {
            dir: "/cfg".into(),
            instance: instance.map(String::from),
            service: None,
            address: None,
            container_port: 3000,
            host_port,
            machine: None,
        }
    }

    #[test]
    fn the_path_is_next_to_state_toml() {
        let p = path_from(Some("/xdg".into()), Some("/home/u".into())).unwrap();
        assert_eq!(p, PathBuf::from("/xdg/devsandbox/forwards.toml"));
        let p = path_from(None, Some("/home/u".into())).unwrap();
        assert_eq!(p, PathBuf::from("/home/u/.local/share/devsandbox/forwards.toml"));
        assert!(path_from(None, None).is_err());
    }

    #[test]
    fn entries_round_trip_and_the_version_is_checked() {
        let entries = vec![
            entry(Some("api"), 3000),
            Entry {
                service: Some("db".into()),
                address: Some("0.0.0.0".parse().unwrap()),
                container_port: 5432,
                ..entry(None, 15432)
            },
            Entry { machine: Some("box".into()), ..entry(Some("web"), 3001) },
        ];
        let text = serialize(&entries).unwrap();
        assert!(text.starts_with("version = 1\n"), "{text}");
        // Additive: only the remote entry has the key.
        assert_eq!(text.matches("machine = \"box\"").count(), 1, "{text}");
        assert_eq!(parse(&text).unwrap(), entries);
        assert_eq!(parse("version = 1\n").unwrap(), vec![]);
        assert!(parse("version = 2\n").is_err());
        assert!(parse("").is_err(), "no version");
        assert_eq!(Entry::address_of("127.0.0.1".parse().unwrap()), None);
        assert_eq!(entries[1].bind().to_string(), "0.0.0.0");
        assert_eq!(entries[0].bind().to_string(), "127.0.0.1");
    }

    #[test]
    fn update_writes_only_changes_and_keeps_a_bad_file() {
        let dir = std::env::temp_dir().join(format!("devsandbox-fwd-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("forwards.toml");
        assert_eq!(load(&path).unwrap(), vec![]);
        update(&path, |e| e.push(entry(Some("a"), 1))).unwrap();
        update(&path, |e| e.push(entry(Some("b"), 2))).unwrap();
        assert!(update(&path, |e| remove(e, &entry(Some("a"), 1))).unwrap());
        assert!(!update(&path, |e| remove(e, &entry(Some("a"), 1))).unwrap());
        assert_eq!(load(&path).unwrap(), vec![entry(Some("b"), 2)]);
        // A no-op doesn't write: no file appears where there was none.
        let other = dir.join("sub/forwards.toml");
        update(&other, |_| ()).unwrap();
        assert!(!other.exists());

        std::fs::write(&path, "version = 9\n").unwrap();
        assert!(update(&path, |e| e.clear()).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "version = 9\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
