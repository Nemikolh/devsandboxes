//! The devsandbox binary: the path to spawn, and the build stamp this
//! process reports.
//!
//! The two differ once the binary is replaced under a running process (an
//! upgrade, a `cargo build`, a `cp` over it): on Linux `current_exe()` then
//! reads `<path> (deleted)`, the old inode's name. A new spawn must run the
//! *new* binary at that path, while the stamp a process reports in `hello`
//! must stay its own binary's, so an old process never claims to be new.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::UNIX_EPOCH;

/// What the kernel appends to `/proc/self/exe`'s target once the file is
/// unlinked or replaced.
const DELETED: &str = " (deleted)";

/// The devsandbox binary to spawn: `current_exe()`, re-resolved on every
/// call (so a long-lived dashboard or daemon picks up an upgrade), without a
/// trailing ` (deleted)` when the path it names exists again.
pub fn devsandbox() -> io::Result<PathBuf> {
    Ok(strip_deleted(std::env::current_exe()?, |p| p.exists()))
}

/// `path` minus a trailing ` (deleted)` if `exists` says the stripped path
/// is there (the replacement); else `path` as is, so a spawn error names
/// what was actually tried.
fn strip_deleted(path: PathBuf, exists: impl Fn(&Path) -> bool) -> PathBuf {
    let stripped = path.to_str().and_then(|s| s.strip_suffix(DELETED)).map(PathBuf::from);
    match stripped {
        Some(stripped) if exists(&stripped) => stripped,
        _ => path,
    }
}

/// This process's binary's mtime (unix seconds), read once at first use and
/// kept: `main` calls it first thing, before an upgrade can swap the file.
/// On Linux `/proc/self/exe`'s metadata follows the link to the inode we
/// run, replaced or not; elsewhere `current_exe()` at first use is the
/// best there is.
pub fn own_build_stamp() -> Option<u64> {
    static STAMP: OnceLock<Option<u64>> = OnceLock::new();
    *STAMP.get_or_init(|| {
        #[cfg(target_os = "linux")]
        let path = PathBuf::from("/proc/self/exe");
        #[cfg(not(target_os = "linux"))]
        let path = std::env::current_exe().ok()?;
        let modified = std::fs::metadata(path).ok()?.modified().ok()?;
        Some(modified.duration_since(UNIX_EPOCH).ok()?.as_secs())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_deleted_only_when_the_replacement_exists() {
        let deleted = PathBuf::from("/usr/bin/devsandbox (deleted)");
        let replaced = PathBuf::from("/usr/bin/devsandbox");
        assert_eq!(strip_deleted(deleted.clone(), |p| p == replaced), replaced);
        // Nothing at the path yet: keep the name the error should show.
        assert_eq!(strip_deleted(deleted.clone(), |_| false), deleted);
    }

    #[test]
    fn leaves_other_paths_alone() {
        let plain = PathBuf::from("/usr/bin/devsandbox");
        assert_eq!(strip_deleted(plain.clone(), |_| true), plain);
        // Only a trailing suffix counts.
        let inside = PathBuf::from("/tmp/x (deleted)/devsandbox");
        assert_eq!(strip_deleted(inside.clone(), |_| true), inside);
    }

    #[test]
    fn devsandbox_resolves_to_an_existing_binary() {
        assert!(devsandbox().unwrap().exists());
    }

    #[test]
    fn build_stamp_is_stable() {
        let first = own_build_stamp();
        assert!(first.is_some());
        assert_eq!(own_build_stamp(), first);
    }
}
