//! A hub's saved remote rows, `remotes.json` next to `state.toml`
//! (docs/architecture/daemon/fleet.md, *Hub mode*): what the machine links
//! last stored in `snapshots::Remotes`, so a restarted hub shows the fleet
//! before its machines reconnect. Written by the link service's
//! `serve-remotes` thread (debounced, [`save_wait`]), read once at start.
//!
//! Only the rows' pieces `snapshots::merge` can't get from the registry are
//! saved (the rows, their time, the machine's version); kind, target,
//! roots and the rest come from `machines.toml` as for any machine. A file
//! of another [`VERSION`], or one that doesn't parse, is ignored and
//! overwritten by the next save.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::serve::snapshots::{LinkState, Machine, restored_error};

/// The file's format; another one is ignored.
pub const VERSION: u32 = 1;

/// At most one write per this long while rows keep changing.
pub const SAVE_EVERY: Duration = Duration::from_secs(5);

/// The file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Saved {
    pub version: u32,
    /// By machine name.
    pub machines: BTreeMap<String, SavedMachine>,
}

/// One machine's last known rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedMachine {
    /// Unix seconds of the rows (their last push).
    pub at: u64,
    /// Its devsandbox semver, from the last hello.
    #[serde(default)]
    pub version: Option<String>,
    /// Remote root → its last `instances.snapshot` params.
    #[serde(default)]
    pub rows: BTreeMap<PathBuf, Value>,
}

/// [`path_from`] over the process env.
pub fn path() -> Result<PathBuf> {
    path_from(std::env::var_os("XDG_DATA_HOME"), std::env::var_os("HOME"))
}

/// `<data>/devsandbox/remotes.json`, next to `state.toml`.
pub fn path_from(xdg_data_home: Option<OsString>, home: Option<OsString>) -> Result<PathBuf> {
    Ok(crate::daemon_config::data_dir_from(xdg_data_home, home)?.join("remotes.json"))
}

/// What to save of `machines`: those with rows or a version; `now` (unix
/// seconds) for rows of no known time.
pub fn of(machines: &[Machine], now: u64) -> Saved {
    let machines = machines
        .iter()
        .filter(|m| !m.rows.is_empty() || m.version.is_some())
        .map(|m| {
            let rows = m.rows.iter().map(|(root, params)| (root.clone(), (**params).clone())).collect();
            (m.name.clone(), SavedMachine { at: m.seen.unwrap_or(now), version: m.version.clone(), rows })
        })
        .collect();
    Saved { version: VERSION, machines }
}

/// The saved machines as [`Remotes::restore`] takes them: `unreachable`,
/// restored, their rows and time.
///
/// [`Remotes::restore`]: crate::serve::snapshots::Remotes::restore
pub fn machines(saved: Saved) -> Vec<Machine> {
    saved
        .machines
        .into_iter()
        .map(|(name, s)| Machine {
            name,
            version: s.version,
            rows: s.rows.into_iter().map(|(root, params)| (root, Arc::new(params))).collect(),
            seen: Some(s.at),
            restored: true,
            state: LinkState::Unreachable,
            error: Some(restored_error(None, Some(s.at))),
            ..Machine::default()
        })
        .collect()
}

/// The file's content; missing, unparsable or of another [`VERSION`] is
/// empty (`Err` only for a read error other than missing).
pub fn load(path: &Path) -> Result<Saved> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(parse(&text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Saved::default()),
        Err(e) => Err(anyhow::Error::new(e).context(format!("cannot read {}", path.display()))),
    }
}

fn parse(text: &str) -> Saved {
    serde_json::from_str::<Saved>(text).ok().filter(|s| s.version == VERSION).unwrap_or_default()
}

/// Replace the file with `saved` (`locked_file::update`: under
/// `remotes.lock`, temp file + rename; unchanged content isn't written).
pub fn save(path: &Path, saved: &Saved) -> Result<()> {
    crate::serve::locked_file::update(
        path,
        load,
        |s| Ok(serde_json::to_string_pretty(s)? + "\n"),
        |s| *s = saved.clone(),
    )
}

/// How long a save due now waits: none at first, else until
/// [`SAVE_EVERY`] after the `last` one.
pub fn save_wait(last: Option<Instant>, now: Instant) -> Duration {
    last.map_or(Duration::ZERO, |last| (last + SAVE_EVERY).saturating_duration_since(now))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve::machines::Entry;
    use crate::serve::snapshots::Remotes;
    use serde_json::json;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dsv-saved-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn machine(name: &str, rows: &[(&str, Value)], seen: Option<u64>) -> Machine {
        Machine {
            name: name.into(),
            version: Some("0.6.0".into()),
            rows: rows.iter().map(|(r, v)| (PathBuf::from(r), Arc::new(v.clone()))).collect(),
            seen,
            state: LinkState::Connected,
            ..Machine::default()
        }
    }

    #[test]
    fn the_path_is_next_to_state_toml() {
        let p = path_from(None, Some("/home/u".into())).unwrap();
        assert_eq!(p, PathBuf::from("/home/u/.local/share/devsandbox/remotes.json"));
    }

    #[test]
    fn round_trips_through_the_file() {
        let dir = scratch("trip");
        let path = dir.join("remotes.json");
        assert_eq!(load(&path).unwrap(), Saved::default(), "missing: empty");
        let rows = json!({"dir": "/r", "instances": [{"name": "web-1"}], "runtime_name": "docker"});
        let never = Machine { name: "never".into(), ..Machine::default() };
        let saved = of(&[machine("lap", &[("/r", rows.clone())], Some(1_761_550_000)), machine("box", &[], None), never], 42);
        assert_eq!(saved.machines.keys().collect::<Vec<_>>(), ["box", "lap"], "nothing to save of `never`");
        assert_eq!(saved.machines["box"].at, 42, "no push yet: now");
        save(&path, &saved).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let file: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(file["version"], VERSION);
        assert_eq!(file["machines"]["lap"], json!({"at": 1_761_550_000u64, "version": "0.6.0", "rows": {"/r": rows}}));
        assert!(dir.join("remotes.lock").exists());
        assert_eq!(load(&path).unwrap(), saved);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn another_version_or_garbage_is_ignored_and_overwritten() {
        let dir = scratch("version");
        let path = dir.join("remotes.json");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, r#"{"version": 99, "machines": {"lap": {"at": 1, "rows": {}}}}"#).unwrap();
        assert_eq!(load(&path).unwrap(), Saved::default());
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(load(&path).unwrap(), Saved::default());
        let saved = of(&[machine("lap", &[], Some(7))], 0);
        save(&path, &saved).unwrap();
        assert_eq!(load(&path).unwrap(), saved);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Restored rows show `unreachable` with their time until the link
    /// connects; the registry still decides which machines and roots stay.
    #[test]
    fn restored_rows_are_unreachable_until_the_link_connects() {
        let rows = json!({"dir": "/r", "instances": [{"name": "web-1", "root": "/r"}], "runtime_name": "podman"});
        let at = 1_761_566_400; // Oct 27 12:00 UTC
        let saved = of(&[machine("lap", &[("/r", rows.clone()), ("/old", json!({}))], Some(at)), machine("gone", &[], None)], 0);
        let remotes = Remotes::default();
        remotes.restore(machines(saved));
        let lap = Entry { roots: vec!["/r".into()], ..Entry::offered("lap") };
        remotes.sync(&[lap]);
        assert!(remotes.get("gone").is_none(), "no longer registered: dropped");
        let m = remotes.get("lap").unwrap();
        assert_eq!((m.state, m.restored, m.rows.keys().collect::<Vec<_>>()), (LinkState::Unreachable, true, vec![&PathBuf::from("/r")]));
        assert_eq!(m.error.as_deref(), Some("not reconnected since this daemon started (rows from Oct 27 12:00 UTC)"));
        let merged = crate::serve::snapshots::merge(&json!({"instances": []}), &remotes.all());
        assert_eq!((&merged["instances"][0]["name"], &merged["instances"][0]["machine"]), (&json!("web-1"), &json!("lap")));
        assert_eq!((&merged["machines"][0]["state"], &merged["machines"][0]["runtime"]), (&json!("unreachable"), &json!("podman")));
        // Saving again keeps the rows' time and drops what the registry dropped.
        let again = of(&remotes.all(), 0);
        assert_eq!((again.machines.len(), again.machines["lap"].at), (1, at));

        let set = |state, error: Option<&str>| {
            let mut moved = false;
            remotes.update("lap", |m| moved = m.set_link(state, error.map(String::from)));
            (moved, remotes.get("lap").unwrap())
        };
        // Idle or connecting: still unreachable, no flapping.
        let (moved, m) = set(LinkState::Idle, None);
        assert_eq!((moved, m.state), (false, LinkState::Unreachable));
        let (moved, _) = set(LinkState::Connecting, None);
        assert!(!moved);
        let (moved, m) = set(LinkState::Unreachable, Some("lap: its fleet link isn't up"));
        assert!(moved);
        assert_eq!(m.error.as_deref(), Some("lap: its fleet link isn't up (rows from Oct 27 12:00 UTC)"));
        // Connected: a live machine again.
        let (moved, m) = set(LinkState::Connected, None);
        assert_eq!((moved, m.state, m.restored, m.error), (true, LinkState::Connected, false, None));
        let (_, m) = set(LinkState::Connecting, None);
        assert_eq!(m.state, LinkState::Connecting);
    }

    #[test]
    fn saves_are_spaced_by_save_every() {
        let t0 = Instant::now();
        assert_eq!(save_wait(None, t0), Duration::ZERO, "the first at once");
        assert_eq!(save_wait(Some(t0), t0 + Duration::from_secs(1)), SAVE_EVERY - Duration::from_secs(1));
        assert_eq!(save_wait(Some(t0), t0 + SAVE_EVERY), Duration::ZERO);
        assert_eq!(save_wait(Some(t0), t0 + SAVE_EVERY * 3), Duration::ZERO);
    }
}
