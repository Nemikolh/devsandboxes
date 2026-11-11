//! Whether this host has a container runtime at all: the backend's binary
//! resolves on `PATH` (docs/architecture/daemon/host-poll.md, *A host with
//! no runtime*). A host without one is a driver: it drives other machines
//! and runs nothing itself, so the daemon's poll asks no runtime, its
//! snapshots carry no runtime error, its capacity says `runtime: false`
//! and it lists no signed copies as roots.
//!
//! A cheap check (no spawn), cached for [`INSTALL_TTL`], so a runtime
//! installed later is picked up without a daemon restart. The lookup and
//! the cache are pure over their inputs; [`installed`] is the process-wide
//! answer for [`super::backend`].

use std::ffi::OsStr;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long one answer stands: a runtime installed meanwhile shows within
/// this.
pub const INSTALL_TTL: Duration = Duration::from_secs(60);

/// Whether `bin` names an executable file: as a path when it has a `/`,
/// else in one of `path`'s dirs (`PATH`'s syntax; `None` or empty: nowhere).
pub fn on_path(bin: &str, path: Option<&OsStr>) -> bool {
    if bin.contains('/') {
        return executable(Path::new(bin));
    }
    let Some(path) = path else { return false };
    std::env::split_paths(path).filter(|d| !d.as_os_str().is_empty()).any(|dir| {
        let candidate = dir.join(bin);
        executable(&candidate) || (cfg!(windows) && executable(&candidate.with_extension("exe")))
    })
}

fn executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else { return false };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
    }
}

/// The last answer, asked again every [`INSTALL_TTL`].
#[derive(Debug, Default)]
pub struct InstallCache {
    last: Option<(Instant, bool)>,
}

impl InstallCache {
    /// The cached answer, `probe`d first when there's none or it's
    /// [`INSTALL_TTL`] old.
    pub fn get(&mut self, now: Instant, probe: impl FnOnce() -> bool) -> bool {
        match self.last {
            Some((at, answer)) if now < at + INSTALL_TTL => answer,
            _ => {
                let answer = probe();
                self.last = Some((now, answer));
                answer
            }
        }
    }
}

/// Whether this process's backend ([`super::backend`]) is installed: its
/// binary on this process's `PATH`, cached for [`INSTALL_TTL`].
pub fn installed() -> bool {
    static CACHE: Mutex<InstallCache> = Mutex::new(InstallCache { last: None });
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    cache.get(Instant::now(), || on_path(super::backend().bin(), std::env::var_os("PATH").as_deref()))
}

/// Why a runtime call isn't made, for messages: `docker isn't on PATH`.
pub fn missing_note() -> String {
    format!("`{}` isn't on PATH", super::backend().bin())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn the_binary_is_looked_up_on_path() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("devsandbox-installed-{}", std::process::id()));
        let (bin, other) = (dir.join("bin"), dir.join("other"));
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let docker = bin.join("docker");
        std::fs::write(&docker, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Not executable: not a runtime.
        std::fs::write(other.join("podman"), "").unwrap();
        let path = std::env::join_paths([&other, &bin]).unwrap();
        assert!(on_path("docker", Some(&path)));
        assert!(!on_path("podman", Some(&path)), "not executable");
        assert!(!on_path("container", Some(&path)));
        assert!(!on_path("docker", Some(OsStr::new("/nonexistent"))));
        assert!(!on_path("docker", Some(OsStr::new(""))));
        assert!(!on_path("docker", None));
        // A path is checked as is.
        assert!(on_path(docker.to_str().unwrap(), None));
        assert!(!on_path("/nonexistent/docker", Some(&path)));
        // A dir of that name isn't one.
        std::fs::create_dir_all(other.join("container")).unwrap();
        assert!(!on_path("container", Some(&path)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_answer_is_asked_again_every_ttl() {
        let mut cache = InstallCache::default();
        let t0 = Instant::now();
        assert!(!cache.get(t0, || false));
        let never = || -> bool { panic!("asked within the ttl") };
        assert!(!cache.get(t0 + INSTALL_TTL - Duration::from_millis(1), never));
        // Installed meanwhile: seen at the next check.
        assert!(cache.get(t0 + INSTALL_TTL, || true));
        assert!(cache.get(t0 + INSTALL_TTL + Duration::from_secs(1), never));
    }
}
