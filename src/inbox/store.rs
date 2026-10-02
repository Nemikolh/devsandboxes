//! `inbox.toml`, next to `state.toml`: the one host-wide Inbox, shared by
//! every dashboard (and, later, the CLI). It holds the history across
//! dashboard sessions — the container outbox forgets a record once a bridge
//! took it — so an in-memory copy per dashboard would mean last writer wins,
//! bringing back records another one dismissed.
//!
//! Every change is a read-modify-write under an exclusive lock on the sibling
//! `inbox.lock` ([`update`]), written to a temp file and renamed, so a crash
//! mid-write can't leave a truncated file and a reader never sees a partial
//! one. Readers ([`load`]) take a shared lock and are best-effort about it:
//! the atomic rename already makes an unlocked read safe. [`stamp`] is the
//! cheap "did it change" check dashboards run each tick.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};

use super::{Inbox, VERSION};
use crate::state::State;

/// The store path: `inbox.toml` beside the state file.
pub fn path() -> Result<PathBuf> {
    Ok(State::path()?.with_file_name("inbox.toml"))
}

/// The lock beside a store file. Separate from `inbox.toml` itself, which the
/// atomic write replaces (locking a file that gets renamed away locks nothing).
fn lock_path(path: &Path) -> PathBuf {
    path.with_file_name("inbox.lock")
}

/// The saved Inbox; a missing file is an empty one.
pub fn load() -> Result<Inbox> {
    load_at(&path()?)
}

/// [`load`] from an explicit path (tests, and the one call site that already
/// resolved it).
pub fn load_at(path: &Path) -> Result<Inbox> {
    let _shared = lock(path, false);
    read(path).map(|(inbox, _)| inbox)
}

/// Read and parse without locking (the caller holds one, or doesn't need it).
/// The flag is set for a pre-v2 file, which [`update_at`] then rewrites so
/// the migration (and its fresh thread ids) happens once, not on every read.
fn read(path: &Path) -> Result<(Inbox, bool)> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Inbox::default(), false)),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
    // Only a v1 file needs the name -> instance_id map, so the state file is
    // read on migration, not on every load.
    let old = version(&text) < VERSION;
    let names = if old { names_from_state() } else { BTreeMap::new() };
    let inbox = Inbox::from_toml(&text, &names)
        .map_err(|e| anyhow::anyhow!("invalid {}: {e}", path.display()))?;
    Ok((inbox, old))
}

/// The file's `version`, 0 (i.e. v1) when absent or unparsable; a real parse
/// error is reported by [`Inbox::from_toml`] right after.
fn version(text: &str) -> u32 {
    #[derive(serde::Deserialize)]
    struct Probe {
        #[serde(default)]
        version: u32,
    }
    toml::from_str::<Probe>(text).map_or(0, |p| p.version)
}

/// Instance name (state key) -> `instance_id`, for migrating a v1 file. A
/// state that won't load leaves every thread unresolved rather than failing
/// the Inbox.
fn names_from_state() -> BTreeMap<String, String> {
    State::load().map_or_else(
        |_| BTreeMap::new(),
        |s| s.instances.iter().map(|(k, i)| (k.clone(), i.instance_id.clone())).collect(),
    )
}

/// Read-modify-write under the exclusive lock: load, run `f`, and save only if
/// the content changed, so an update that decides to do nothing doesn't bump
/// the mtime other dashboards poll.
pub fn update<T>(f: impl FnOnce(&mut Inbox) -> T) -> Result<T> {
    update_at(&path()?, f)
}

/// [`update`] on an explicit path (tests).
pub fn update_at<T>(path: &Path, f: impl FnOnce(&mut Inbox) -> T) -> Result<T> {
    let dir = path.parent().context("inbox path has no parent")?;
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let _exclusive = lock(path, true).with_context(|| format!("cannot lock {}", lock_path(path).display()))?;
    let (mut inbox, migrated) = read(path)?;
    let before = serialize(&inbox)?;
    let out = f(&mut inbox);
    let after = serialize(&inbox)?;
    if after != before || migrated {
        write_atomic(path, &after)?;
    }
    Ok(out)
}

fn serialize(inbox: &Inbox) -> Result<String> {
    inbox.to_toml().map_err(|e| anyhow::anyhow!("cannot serialize inbox: {e}"))
}

/// Hold the store lock until the returned file is dropped. A shared lock that
/// can't be taken (read-only dir, exotic filesystem) is not fatal: the atomic
/// write means an unlocked read is still consistent.
fn lock(path: &Path, exclusive: bool) -> Result<File> {
    let file = File::options()
        .create(true)
        .read(true)
        .write(true)
        .open(lock_path(path))
        .with_context(|| format!("cannot open {}", lock_path(path).display()))?;
    if exclusive { file.lock() } else { file.lock_shared() }.context("cannot lock the inbox")?;
    Ok(file)
}

/// Write `text` through a temp file + rename, so readers only ever see a
/// complete file. The temp name carries the pid: the lock already serializes
/// writers, but a stale temp from a killed process must not be reused.
fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let tmp = path.with_file_name(format!(".inbox.toml.{}.tmp", std::process::id()));
    std::fs::write(&tmp, text).with_context(|| format!("cannot write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("cannot write {}", path.display()))
}

/// Cheap change check: the store's mtime and length, `None` when it doesn't
/// exist yet. Dashboards compare it each tick and reload when it moves.
pub fn stamp() -> Option<(SystemTime, u64)> {
    stamp_at(&path().ok()?)
}

/// [`stamp`] of an explicit path (tests).
pub fn stamp_at(path: &Path) -> Option<(SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devsbd::notify::{Level, Record};
    use crate::inbox::Op;

    fn rec(msg: &str) -> Record {
        Record { level: Level::Info, key: None, link: None, msg: msg.into(), at: 0 }
    }

    /// A unique empty directory under the test tmp dir.
    fn tmpdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("devsandbox-inbox-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_file_loads_empty_and_update_creates_it() {
        let path = tmpdir("create").join("inbox.toml");
        assert!(load_at(&path).unwrap().threads.is_empty());
        assert_eq!(stamp_at(&path), None);
        update_at(&path, |i| i.push("a-id".into(), "a".into(), rec("one"), true)).unwrap();
        let loaded = load_at(&path).unwrap();
        assert_eq!(loaded.threads.len(), 1);
        assert_eq!(loaded.threads[0].head().msg, "one");
        assert!(stamp_at(&path).is_some());
    }

    #[test]
    fn save_only_when_the_content_changed() {
        let path = tmpdir("unchanged").join("inbox.toml");
        update_at(&path, |i| i.push("a-id".into(), "a".into(), rec("one"), true)).unwrap();
        let before = stamp_at(&path).unwrap();
        // Far enough apart that a coarse mtime would still move.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        // A no-op closure, and an op that matches nothing.
        update_at(&path, |_| {}).unwrap();
        update_at(&path, |i| i.apply(&Op::RemoveThread(9999))).unwrap();
        assert_eq!(stamp_at(&path).unwrap(), before, "no write, no mtime bump");
        let out = update_at(&path, |i| {
            i.apply(&Op::MarkAllRead);
            42
        })
        .unwrap();
        assert_eq!(out, 42, "update returns the closure's value");
        assert_ne!(stamp_at(&path).unwrap(), before, "a real change writes");
    }

    /// Two writers interleaving read-modify-writes: the lock makes every push
    /// land, instead of one process's copy overwriting the other's.
    #[test]
    fn concurrent_updates_lose_nothing() {
        let path = tmpdir("concurrent").join("inbox.toml");
        std::thread::scope(|s| {
            for who in ["a", "b"] {
                let path = path.clone();
                s.spawn(move || {
                    for i in 0..50 {
                        update_at(&path, |inbox| {
                            inbox.push(format!("{who}-id"), who.into(), rec(&format!("{who}{i}")), true)
                        })
                        .unwrap();
                    }
                });
            }
        });
        let inbox = load_at(&path).unwrap();
        assert_eq!(inbox.threads.len(), 100);
        assert_eq!(inbox.count_for("a-id"), 50);
        assert_eq!(inbox.count_for("b-id"), 50);
        let mut msgs: Vec<&str> = inbox.threads.iter().map(|t| t.head().msg.as_str()).collect();
        msgs.sort_unstable();
        msgs.dedup();
        assert_eq!(msgs.len(), 100, "every push is distinct and present");
        // Ids stay unique across writers, so selection can't alias.
        let mut ids: Vec<u64> = inbox.threads.iter().map(|t| t.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 100);
    }

    /// A v1 file on disk is migrated on load and rewritten as v2 by the next
    /// update, even one that changes nothing, so it migrates once.
    #[test]
    fn loads_and_upgrades_a_v1_file() {
        let path = tmpdir("v1").join("inbox.toml");
        std::fs::write(
            &path,
            "[[thread]]\ninstance = \"web\"\nunread = true\n[[thread.note]]\nseq = 3\nlevel = \"info\"\nat = 0\nmsg = \"hi\"\n",
        )
        .unwrap();
        let inbox = load_at(&path).unwrap();
        assert_eq!(inbox.threads[0].owner_name, "web");
        assert_eq!(inbox.threads[0].head().msg, "hi");
        update_at(&path, |_| {}).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("version = 2"), "{text}");
        assert!(!text.contains("instance ="), "{text}");
    }
}
