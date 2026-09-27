//! ssh-agent forwarding: the per-instance stable socket symlink the container
//! mounts, and its refresh on start (see docs/ssh-agent.md).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::runtime::backend;
use crate::state::{Instance, State};

/// Fixed container path the forwarded ssh-agent socket is mounted at, so Linux
/// and macOS converge on one target regardless of the host source (see
/// docs/ssh-agent.md).
pub(crate) const SSH_AGENT_TARGET: &str = "/run/devsandbox/ssh-agent.sock";

/// `<data-dir>/agent`, the dir holding per-instance agent symlinks. `None` when
/// the state path can't be resolved. One recipe so `run`, `rm`, and `gc` don't
/// each re-derive it (docs/ssh-agent.md).
pub(crate) fn ssh_agent_dir() -> Option<PathBuf> {
    Some(State::path().ok()?.parent()?.join("agent"))
}

/// The agent symlink path for one instance, `<data-dir>/agent/<id>.sock`. `None`
/// when the state path can't be resolved. Callers that clean up (`rm`, `gc`)
/// share this rather than duplicating the join.
pub(crate) fn ssh_agent_link_path(instance_id: &str) -> Option<PathBuf> {
    Some(ssh_agent_dir()?.join(format!("{instance_id}.sock")))
}

/// Create or re-point `agent_dir/<instance_id>.sock` at the host agent
/// socket. A symlink (not the raw path) because bind sources are
/// re-resolved at every container start, so `start` can re-point it after
/// the host agent rotates (docs/ssh-agent.md).
pub(crate) fn ssh_agent_link(agent_dir: &Path, instance_id: &str, sock: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(agent_dir)
        .with_context(|| format!("cannot create {}", agent_dir.display()))?;
    let link = agent_dir.join(format!("{instance_id}.sock"));
    match std::fs::remove_file(&link) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("cannot replace {}", link.display())),
    }
    symlink(sock, &link)
        .with_context(|| format!("cannot link {} -> {}", link.display(), sock.display()))?;
    Ok(link)
}

#[cfg(unix)]
fn symlink(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(src, dst)
}

/// Never reached: `ssh_agent_forward`/`ssh_agent_refresh` bail out first on
/// non-unix hosts, whose agent is a named pipe no runtime can bind
/// (docs/ssh-agent.md, _Windows_). Exists only so the crate compiles.
#[cfg(not(unix))]
fn symlink(_src: &Path, _dst: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "ssh-agent forwarding is unix-only",
    ))
}

/// Decide ssh-agent forwarding for a new instance. Returns the `-v
/// <link>:<target>` mount to push and the container target to persist, or
/// `None` when forwarding is off. Gate (all silent on miss — an absent agent
/// is the common case): the runtime binds files, `$SSH_AUTH_SOCK` is set, and
/// its socket exists on the host. On a link error, warn and skip: forwarding
/// is best-effort and must not fail container creation (docs/ssh-agent.md).
pub(super) fn ssh_agent_forward(instance_id: &str) -> Result<Option<(String, String)>> {
    if !cfg!(unix) || !backend().supports_file_binds() {
        return Ok(None);
    }
    let Some(sock) = std::env::var_os("SSH_AUTH_SOCK") else {
        return Ok(None);
    };
    let sock = PathBuf::from(sock);
    if std::fs::metadata(&sock).is_err() {
        return Ok(None);
    }
    let agent_dir = ssh_agent_dir().context("state path has no parent")?;
    match ssh_agent_link(&agent_dir, instance_id, &sock) {
        Ok(link) => Ok(Some((
            format!("{}:{SSH_AGENT_TARGET}", link.display()),
            SSH_AGENT_TARGET.to_string(),
        ))),
        Err(e) => {
            eprintln!("warning: ssh-agent forwarding disabled: {e:#}");
            Ok(None)
        }
    }
}

/// Re-point `info`'s agent symlink at the current host agent before its
/// container starts, so a restart after agent rotation (reboot, re-login)
/// re-captures the live socket — bind sources re-resolve at every container
/// start (docs/ssh-agent.md). Gate mirrors `ssh_agent_forward`: forwarding was
/// on for this instance, `$SSH_AUTH_SOCK` is set, and its socket exists.
/// Fully silent — every gate miss and error is a no-op (the link stays as-is
/// and forwarding is degraded for this run only): callers include the TUI
/// background thread that owns the alternate screen, where a stray warning
/// would corrupt the display, so screen safety wins over a lost warning.
pub(crate) fn ssh_agent_refresh(info: &Instance) {
    if !cfg!(unix) || info.ssh_auth_sock.is_none() {
        return;
    }
    let Some(sock) = std::env::var_os("SSH_AUTH_SOCK") else {
        return;
    };
    let sock = PathBuf::from(sock);
    if std::fs::metadata(&sock).is_err() {
        return;
    }
    let Some(agent_dir) = ssh_agent_dir() else {
        return;
    };
    let _ = ssh_agent_link(&agent_dir, &info.instance_id, &sock);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::run::tests::instance;

    #[test]
    #[cfg(unix)]
    fn ssh_agent_link_creates_and_repoints() {
        let dir = std::env::temp_dir().join(format!("devsandbox-agent-{}", std::process::id()));
        let first = dir.join("agent-a.sock");
        let second = dir.join("agent-b.sock");

        let link = ssh_agent_link(&dir, "repo-abc1", &first).unwrap();
        assert_eq!(link, dir.join("repo-abc1.sock"));
        assert_eq!(std::fs::read_link(&link).unwrap(), first);

        // A second call re-points the same link at the new target.
        let link2 = ssh_agent_link(&dir, "repo-abc1", &second).unwrap();
        assert_eq!(link2, link);
        assert_eq!(std::fs::read_link(&link).unwrap(), second);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn ssh_agent_refresh_noop_without_forwarding() {
        // `ssh_auth_sock: None` means forwarding was never on for this instance;
        // the gate returns before any env/fs access, so no link is created even
        // if the data dir and a live agent are present. Uses a unique id so the
        // agent dir (if it exists at all) can't already hold a matching link.
        let id = format!("refresh-gate-{}", std::process::id());
        let mut info = instance("repo");
        info.instance_id = id.clone();
        assert!(info.ssh_auth_sock.is_none());
        ssh_agent_refresh(&info);
        if let Ok(path) = State::path() {
            if let Some(parent) = path.parent() {
                let link = parent.join("agent").join(format!("{id}.sock"));
                assert!(!link.exists(), "gate should not create {}", link.display());
            }
        }
    }

    // --- features: pure generators ---
}
