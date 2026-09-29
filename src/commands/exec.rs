use std::io::IsTerminal;

use anyhow::{bail, Result};

use super::pick;
use crate::runtime::backend;
use crate::state::{Instance, State};

/// CLI entry point: run the command, then exit the process with its status.
pub fn exec(name: &str, interactive: bool, tty: bool, command: &[String]) -> Result<()> {
    std::process::exit(exec_status(name, interactive, tty, command)?);
}

/// Run the command inside the matching instance and return its exit code.
/// Split out of [`exec`] so callers that must not terminate the process (the
/// dashboard prompt) can reuse it.
pub fn exec_status(name: &str, interactive: bool, tty: bool, command: &[String]) -> Result<i32> {
    let state = State::load()?;

    // <name> may be an instance name, a sandbox config name, or a repository
    // (folder basename); collect every instance it could refer to.
    let mut matches: Vec<(&String, &Instance)> = state
        .instances
        .iter()
        .filter(|(instance, info)| {
            *instance == name
                || info.sandbox == name
                || info.folder.file_name().is_some_and(|f| f == name)
        })
        .collect();

    let (_, instance) = match matches.len() {
        0 => bail!("no sandbox instance matches `{name}` (see `devsandbox ps`)"),
        1 => matches.remove(0),
        _ => {
            let labels: Vec<String> = matches
                .iter()
                .map(|(instance, info)| format!("{instance} (sandbox {})", info.sandbox))
                .collect();
            let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
            if !std::io::stdin().is_terminal() {
                bail!("`{name}` is ambiguous: {}", labels.join(", "));
            }
            matches.remove(pick(&format!("`{name}` is ambiguous"), &label_refs)?)
        }
    };

    // ssh-agent relay for the command's lifetime, started optimistically
    // alongside it: no handshake wait, the daemon holds an agent client that
    // beats the bridge (docs/sandbox-helper.md). Only when a host agent exists,
    // same gate as the `SSH_AUTH_SOCK` injection below.
    #[cfg(unix)]
    let bridge = (crate::devsbd::relay_mode(instance)
        && crate::devsbd::bridge::has_host_agent())
    .then(|| crate::devsbd::bridge::spawn(instance))
    .flatten();
    let args = exec_argv(instance, interactive, tty, command);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let code = backend().run_inherit(&arg_refs)?;
    // Reported after the command so it can't interleave with its output.
    #[cfg(unix)]
    if let Some(Err(e)) = bridge.as_ref().and_then(|b| b.outcome(std::time::Duration::ZERO)) {
        eprintln!("note: ssh-agent relay unavailable in `{}`: {e}", instance.container);
    }
    Ok(code)
}

/// Build the runtime `exec` argv for `instance`: the flags, workspace,
/// remoteUser, remoteEnv, container, and command, in the order the CLI has
/// always emitted them. Factored out so the dashboard's integrated terminal
/// spawns the exact same command the CLI does and the two can't drift.
pub fn exec_argv(
    instance: &Instance,
    interactive: bool,
    tty: bool,
    command: &[String],
) -> Vec<String> {
    // Relay mode injects `SSH_AUTH_SOCK` only when the host has a live agent,
    // so an agent-less exec doesn't point ssh at a dead socket; mount mode
    // always injects (the bind decision was made at `run`). The probe lives
    // outside the pure builder so tests need no env mutation.
    let host_agent = crate::devsbd::relay_mode(instance) && has_host_agent();
    exec_argv_with(instance, interactive, tty, command, host_agent)
}

/// Whether the host currently has a usable ssh-agent, for relay-mode
/// `SSH_AUTH_SOCK` injection. Unix-only (the relay is); `false` elsewhere.
#[cfg(unix)]
pub(crate) fn has_host_agent() -> bool {
    crate::devsbd::bridge::has_host_agent()
}
#[cfg(not(unix))]
pub(crate) fn has_host_agent() -> bool {
    false
}

/// The container-side `SSH_AUTH_SOCK` value to inject on an exec into
/// `instance`, or `None` when no agent forwarding applies. The single source
/// of truth shared by the CLI/TUI builder ([`exec_argv_with`]) and lifecycle
/// execs (`run::exec_lifecycle`), so the two can't drift. Mount mode carries
/// the target on `ssh_auth_sock` and always injects (the bind decision was
/// made at `run`); relay mode uses the fixed target but only when a host agent
/// is actually present, so an agent-less exec doesn't point ssh at a dead
/// socket. `host_agent` is the probe result ([`has_host_agent`]), passed in so
/// the rule stays a pure function tests can drive without env mutation.
pub(crate) fn ssh_auth_sock_env(instance: &Instance, host_agent: bool) -> Option<&str> {
    match &instance.ssh_auth_sock {
        Some(sock) => Some(sock.as_str()),
        None if crate::devsbd::relay_mode(instance) && host_agent => {
            Some(crate::commands::run::SSH_AGENT_TARGET)
        }
        None => None,
    }
}

/// Pure argv builder. `host_agent` says the host has a live agent (relay mode
/// only); mount mode ignores it and injects unconditionally.
fn exec_argv_with(
    instance: &Instance,
    interactive: bool,
    tty: bool,
    command: &[String],
    host_agent: bool,
) -> Vec<String> {
    let mut args: Vec<String> = vec!["exec".into()];
    if interactive {
        args.push("-i".into());
    }
    if tty {
        args.push("-t".into());
    }
    args.extend(["-w".into(), instance.workspace.clone()]);
    if let Some(user) = &instance.remote_user {
        args.push("-u".into());
        args.push(user.clone());
    }
    for (key, value) in &instance.remote_env {
        args.push("-e".into());
        args.push(format!("{key}={value}"));
    }
    // agent forwarding: the activating env var rides every exec, same as
    // remote_env, via the shared rule (docs/sandbox-helper.md).
    if let Some(sock) = ssh_auth_sock_env(instance, host_agent) {
        args.push("-e".into());
        args.push(format!("SSH_AUTH_SOCK={sock}"));
    }
    args.push(instance.container.clone());
    args.extend(command.iter().cloned());
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn instance() -> Instance {
        Instance {
            sandbox: "repository-1".into(),
            instance_id: "repo-abc1".into(),
            project: "abc12345".into(),
            container: "devsandbox-repo-abc1".into(),
            folder: PathBuf::from("/home/u/repository-1"),
            base_folder: PathBuf::from("/home/u/repository-1"),
            worktree: None,
            branch: None,
            branch_created: true,
            shell_history: None,
            workspace: "/workspaces/repository-1".into(),
            workspace_file: None,
            remote_env: BTreeMap::new(),
            remote_user: None,
            ssh_auth_sock: None,
            devsbd_arch: None,
            volumes: Vec::new(),
            dispatcher: None,
            config_dir: None,
            extra_env: Default::default(),
            created_unix: 0,
        }
    }

    #[test]
    fn argv_minimal() {
        let inst = instance();
        assert_eq!(
            exec_argv_with(&inst, false, false, &["ls".into()], false),
            vec![
                "exec",
                "-w",
                "/workspaces/repository-1",
                "devsandbox-repo-abc1",
                "ls",
            ]
        );
    }

    #[test]
    fn argv_flag_ordering() {
        let mut inst = instance();
        inst.remote_user = Some("vscode".into());
        inst.remote_env.insert("FOO".into(), "bar".into());
        inst.remote_env.insert("BAZ".into(), "qux".into());
        assert_eq!(
            exec_argv_with(&inst, true, true, &["bash".into(), "-l".into()], false),
            vec![
                "exec",
                "-i",
                "-t",
                "-w",
                "/workspaces/repository-1",
                "-u",
                "vscode",
                // BTreeMap iterates sorted: BAZ before FOO
                "-e",
                "BAZ=qux",
                "-e",
                "FOO=bar",
                "devsandbox-repo-abc1",
                "bash",
                "-l",
            ]
        );
    }

    #[test]
    fn argv_ssh_auth_sock() {
        let mut inst = instance();
        inst.remote_env.insert("FOO".into(), "bar".into());
        inst.ssh_auth_sock = Some("/run/devsandbox/ssh-agent.sock".into());
        // Mount mode injects regardless of the host-agent flag.
        assert_eq!(
            exec_argv_with(&inst, false, false, &["ls".into()], false),
            vec![
                "exec",
                "-w",
                "/workspaces/repository-1",
                // remote_env -e pairs come first,
                "-e",
                "FOO=bar",
                // then SSH_AUTH_SOCK, before the container name.
                "-e",
                "SSH_AUTH_SOCK=/run/devsandbox/ssh-agent.sock",
                "devsandbox-repo-abc1",
                "ls",
            ]
        );
    }

    /// Relay mode (`devsbd_arch` set, no mount): inject the fixed target only
    /// when a host agent is present. Unix-only — `relay_mode` is `false` off
    /// unix, so no injection there.
    #[cfg(unix)]
    #[test]
    fn argv_relay_injects_only_with_host_agent() {
        let mut inst = instance();
        inst.devsbd_arch = Some(crate::devsbd::Arch::X86_64);
        assert_eq!(
            exec_argv_with(&inst, false, false, &["ls".into()], true),
            vec![
                "exec",
                "-w",
                "/workspaces/repository-1",
                "-e",
                "SSH_AUTH_SOCK=/run/devsandbox/ssh-agent.sock",
                "devsandbox-repo-abc1",
                "ls",
            ]
        );
        // No host agent → no SSH_AUTH_SOCK, so ssh doesn't hit a dead socket.
        assert_eq!(
            exec_argv_with(&inst, false, false, &["ls".into()], false),
            vec!["exec", "-w", "/workspaces/repository-1", "devsandbox-repo-abc1", "ls"]
        );
    }

    /// The shared rule the CLI/TUI builder and lifecycle execs both consult.
    #[test]
    fn ssh_auth_sock_env_rule() {
        // Mount mode: the recorded target, regardless of the host-agent flag.
        let mut mount = instance();
        mount.ssh_auth_sock = Some("/run/devsandbox/ssh-agent.sock".into());
        assert_eq!(ssh_auth_sock_env(&mount, false), Some("/run/devsandbox/ssh-agent.sock"));
        assert_eq!(ssh_auth_sock_env(&mount, true), Some("/run/devsandbox/ssh-agent.sock"));

        // Relay mode (`devsbd_arch` set, no mount): the fixed target only when a
        // host agent is present. Unix-only — `relay_mode` is `false` off unix.
        let mut relay = instance();
        relay.devsbd_arch = Some(crate::devsbd::Arch::X86_64);
        #[cfg(unix)]
        {
            assert_eq!(
                ssh_auth_sock_env(&relay, true),
                Some(crate::commands::run::SSH_AGENT_TARGET)
            );
            assert_eq!(ssh_auth_sock_env(&relay, false), None);
        }
        #[cfg(not(unix))]
        {
            assert_eq!(ssh_auth_sock_env(&relay, true), None);
        }

        // Neither mount nor relay: never injected.
        let bare = instance();
        assert_eq!(ssh_auth_sock_env(&bare, true), None);
        assert_eq!(ssh_auth_sock_env(&bare, false), None);
    }
}
