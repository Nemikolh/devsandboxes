//! The daemon-owned files' one write discipline (`machines.toml`,
//! `forwards.toml`, `member.toml`, `membership.json`): two daemons can briefly overlap during a
//! handoff, so every write is a read-modify-write under an exclusive lock on
//! the sibling `<stem>.lock` (separate from the file, which the write
//! replaces), written to a temp file and renamed. `member.key` is created
//! once with the same pieces ([`lock_at`], [`write_atomic_mode`]; see
//! `fleet::identity::FileKeyStore`).

use std::fs::File;
use std::path::Path;

use anyhow::{Context, Result};

/// Read-modify-write of `path` under its lock: `load` reads it (missing is
/// the caller's empty value), `f` changes it, and `serialize` writes it back
/// only when `f` changed it. A file that doesn't load fails the update rather
/// than being overwritten.
pub fn update<V: Clone + PartialEq, T>(
    path: &Path,
    load: impl FnOnce(&Path) -> Result<V>,
    serialize: impl FnOnce(&V) -> Result<String>,
    f: impl FnOnce(&mut V) -> T,
) -> Result<T> {
    let dir = path.parent().with_context(|| format!("{} has no parent", path.display()))?;
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let _lock = lock(path)?;
    let mut value = load(path)?;
    let before = value.clone();
    let out = f(&mut value);
    if value != before {
        write_atomic(path, &serialize(&value)?)?;
    }
    Ok(out)
}

/// `<stem>.lock` next to `path`, locked exclusively until dropped.
fn lock(path: &Path) -> Result<File> {
    lock_at(&path.with_extension("lock"))
}

/// The lock file `lock_path` (created if missing), locked exclusively until
/// dropped. An `flock`: two opens in one process exclude each other too.
pub fn lock_at(lock_path: &Path) -> Result<File> {
    let file = File::options()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lock_path)
        .with_context(|| format!("cannot open {}", lock_path.display()))?;
    file.lock().with_context(|| format!("cannot lock {}", lock_path.display()))?;
    Ok(file)
}

/// Temp file + rename; the pid in the temp name keeps a killed writer's stale
/// temp from being reused by the other daemon.
fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let tmp = tmp_path(path)?;
    std::fs::write(&tmp, text).with_context(|| format!("cannot write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("cannot write {}", path.display()))
}

/// [`write_atomic`] with the temp file created fresh with `mode`, so the
/// file is never there with wider permissions (a stale temp is removed
/// first: its mode could be anything). The caller holds the lock.
pub fn write_atomic_mode(path: &Path, text: &str, mode: u32) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = tmp_path(path)?;
    match std::fs::remove_file(&tmp) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e).with_context(|| format!("cannot remove {}", tmp.display())),
        _ => {}
    }
    let mut file = File::options()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp)
        .with_context(|| format!("cannot create {}", tmp.display()))?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .with_context(|| format!("cannot write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("cannot write {}", path.display()))
}

fn tmp_path(path: &Path) -> Result<std::path::PathBuf> {
    let name = path.file_name().context("no file name")?.to_string_lossy();
    Ok(path.with_file_name(format!(".{name}.{}.tmp", std::process::id())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(path: &Path) -> Result<Vec<String>> {
        match std::fs::read_to_string(path) {
            Ok(text) if text.starts_with('!') => anyhow::bail!("bad"),
            Ok(text) => Ok(text.lines().map(String::from).collect()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }

    fn save(v: &Vec<String>) -> Result<String> {
        Ok(v.iter().map(|l| format!("{l}\n")).collect())
    }

    #[test]
    fn updates_under_the_sibling_lock_and_only_on_change() {
        let dir = std::env::temp_dir().join(format!("dsv-locked-file-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("things.toml");
        update(&path, load, save, |v| v.push("a".into())).unwrap();
        assert_eq!(update(&path, load, save, |v| v.len()).unwrap(), 1);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\n");
        assert!(dir.join("things.lock").exists());
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        // A no-op writes nothing.
        let other = dir.join("sub/things.toml");
        update(&other, load, save, |_| ()).unwrap();
        assert!(!other.exists());
        // A bad file stays.
        std::fs::write(&path, "!x").unwrap();
        assert!(update(&path, load, save, |v| v.clear()).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "!x");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
