//! The identity probe (docs/architecture/runtimes.md, *Mode detection*): the
//! user-namespace mode of the runtime context this host's devsandbox talks
//! to, cached in `state.toml` per context. The cache is consulted first, then
//! the runtime's metadata (`Backend::identity_fast_path`), then the canary
//! (`Backend::identity_canary`) when the metadata can't tell or on
//! `--reprobe`. Probed where the container is created: a machine probes for
//! itself.

use std::path::Path;

use anyhow::{Context, Result};

use crate::runtime::backend;
use crate::runtime::identity::{self, Engine, FastPath, Ids, Mode, Probe};
use crate::state::State;

/// The probe for the current runtime context. `image` is the sandbox's
/// (already built or pulled) image, which the canary runs from; `reprobe`
/// skips the cache and runs the canary even when the metadata is clear. An
/// image without `/bin/sh` fails the canary: the error is the caller's to
/// handle.
pub(crate) fn probe(image: &str, reprobe: bool) -> Result<Probe> {
    let b = backend();
    // Apple `container`: every container is its own VM; nothing to run.
    if b.name() == "container" {
        return Ok(Probe { mode: Mode::VmBacked, selinux: false });
    }
    let env = |key: &str| std::env::var(key).ok().filter(|v| !v.is_empty());
    let key = identity::context_key(b.name(), env, docker_current_context);
    let cached = State::load()?.identity_probes.get(&key).copied();
    let engine = if b.name() == "podman" { Engine::Podman } else { Engine::Docker };
    let (probe, fresh) = decide(cached, reprobe, || b.identity_fast_path(), |relabel| {
        let dir = State::path()?.with_file_name(format!("identity-probe-{}", std::process::id()));
        let canary = with_canary_dir(&dir, |dir| b.identity_canary(image, dir, relabel))?;
        let host = host_ids()?;
        identity::classify(host, canary.as_root, canary.as_1000, engine).with_context(|| {
            format!(
                "identity canary: unrecognized ownership (host {}:{}, uid 0 wrote as {}:{}, uid {} as {}:{})",
                host.uid,
                host.gid,
                canary.as_root.uid,
                canary.as_root.gid,
                identity::ASSUMED.uid,
                canary.as_1000.uid,
                canary.as_1000.gid
            )
        })
    })?;
    if fresh {
        State::update(|state| {
            state.identity_probes.insert(key, probe);
            Ok(())
        })?;
    }
    Ok(probe)
}

/// The probe's decision, I/O injected: the cached probe unless `reprobe`,
/// else the metadata, else (ambiguous metadata, or `reprobe`) the canary,
/// told whether to relabel its bind. SELinux always comes from the metadata.
/// The `bool` is whether the result is new (to be cached).
fn decide(
    cached: Option<Probe>,
    reprobe: bool,
    fast_path: impl FnOnce() -> Result<FastPath>,
    canary: impl FnOnce(bool) -> Result<Mode>,
) -> Result<(Probe, bool)> {
    if let (Some(probe), false) = (cached, reprobe) {
        return Ok((probe, false));
    }
    let fast = fast_path().context("cannot read the runtime's info")?;
    let mode = match fast.mode {
        Ok(mode) if !reprobe => mode,
        Ok(_) => canary(fast.selinux)?,
        Err(why) => canary(fast.selinux).with_context(|| format!("{why}, so the identity canary decides"))?,
    };
    Ok((Probe { mode, selinux: fast.selinux }, true))
}

/// Run `f` over a fresh, empty `dir` any uid can write (the canary's
/// writers are uid 0 and 1000, mapped who knows where), removed after. It
/// sits next to `state.toml`, under `$HOME`: the dir VM file shares (colima,
/// lima) cover by default, which the system temp dir may not be.
fn with_canary_dir<T>(dir: &Path, f: impl FnOnce(&Path) -> Result<T>) -> Result<T> {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o777))
            .with_context(|| format!("cannot open up {}", dir.display()))?;
    }
    let out = f(dir);
    // The dir is ours, so its foreign-owned files unlink fine.
    let _ = std::fs::remove_dir_all(dir);
    out
}

#[cfg(unix)]
fn host_ids() -> Result<Ids> {
    // SAFETY: getuid/getgid have no preconditions and can't fail.
    Ok(Ids { uid: unsafe { libc::getuid() }, gid: unsafe { libc::getgid() } })
}

#[cfg(not(unix))]
fn host_ids() -> Result<Ids> {
    anyhow::bail!("the identity canary needs a unix host")
}

/// The docker CLI's `currentContext` (`$DOCKER_CONFIG/config.json`, else
/// `~/.docker/config.json`), as the CLI itself resolves it.
fn docker_current_context() -> Option<String> {
    let dir = std::env::var_os("DOCKER_CONFIG")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".docker")))?;
    identity::docker_current_context(&std::fs::read_to_string(dir.join("config.json")).ok()?)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use anyhow::bail;

    use super::*;

    const ROOTFUL: Probe = Probe { mode: Mode::Rootful, selinux: false };

    fn fast(mode: Result<Mode, &'static str>, selinux: bool) -> impl FnOnce() -> Result<FastPath> {
        move || Ok(FastPath { mode, selinux })
    }

    #[test]
    fn cache_hit_runs_nothing() {
        let out = decide(Some(ROOTFUL), false, || panic!("fast path ran"), |_| panic!("canary ran")).unwrap();
        assert_eq!(out, (ROOTFUL, false));
    }

    #[test]
    fn clear_metadata_skips_the_canary() {
        let out = decide(None, false, fast(Ok(Mode::RootlessDocker), true), |_| panic!("canary ran")).unwrap();
        assert_eq!(out, (Probe { mode: Mode::RootlessDocker, selinux: true }, true));
    }

    #[test]
    fn ambiguous_metadata_runs_the_canary_with_its_selinux() {
        let relabeled = Cell::new(None);
        let canary = |relabel| {
            relabeled.set(Some(relabel));
            Ok(Mode::VmBacked)
        };
        let out = decide(None, false, fast(Err("remote"), true), canary).unwrap();
        assert_eq!(out, (Probe { mode: Mode::VmBacked, selinux: true }, true));
        assert_eq!(relabeled.get(), Some(true));
    }

    #[test]
    fn reprobe_skips_the_cache_and_runs_the_canary() {
        let ran = Cell::new(false);
        let canary = |_| {
            ran.set(true);
            Ok(Mode::RootlessPodman)
        };
        let out = decide(Some(ROOTFUL), true, fast(Ok(Mode::Rootful), false), canary).unwrap();
        assert_eq!(out, (Probe { mode: Mode::RootlessPodman, selinux: false }, true));
        assert!(ran.get());
    }

    #[test]
    fn failures_surface_with_why() {
        let err = decide(None, false, fast(Err("the docker daemon remaps users"), false), |_| bail!("no sh"))
            .unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("remaps users, so the identity canary decides") && text.contains("no sh"), "{text}");
        let err = decide(None, false, || bail!("daemon down"), |_| panic!("canary ran")).unwrap_err();
        assert!(format!("{err:#}").contains("daemon down"));
    }

    #[test]
    fn canary_dir_is_world_writable_and_removed() {
        let dir = std::env::temp_dir().join(format!("devsandbox-canary-dir-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("stale")).unwrap();
        let seen = with_canary_dir(&dir, |d| {
            let entries = std::fs::read_dir(d)?.count();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(std::fs::metadata(d)?.permissions().mode() & 0o777, 0o777);
            }
            std::fs::write(d.join("0"), "")?;
            Ok(entries)
        })
        .unwrap();
        assert_eq!(seen, 0, "a leftover dir is emptied first");
        assert!(!dir.exists());
        assert!(with_canary_dir(&dir, |_| -> Result<()> { bail!("x") }).is_err());
        assert!(!dir.exists());
    }
}
