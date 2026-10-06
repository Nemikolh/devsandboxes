use std::io::IsTerminal;

use anyhow::Result;

use super::resolve_instance;
use crate::runtime::backend;
use crate::state::{Instance, State};

/// In-container login shell, as argv pieces: what `exec <name>` with no
/// command runs, and what the dashboard's integrated terminal opens. Images
/// are often minimal (no bash, no login profile wired up), so probe zsh first
/// (most devsandboxes ship it), then bash, and fall back to POSIX `sh`, all as
/// login shells. Shared so a CLI shell, a dashboard tab, and a programmatic
/// pty consumer spawning `devsandbox exec <name>` all get the same shell.
pub const SHELL_FALLBACK_CMD: [&str; 3] = [
    "sh",
    "-lc",
    "command -v zsh >/dev/null 2>&1 && exec zsh -l; command -v bash >/dev/null 2>&1 && exec bash -l; exec sh -l",
];

/// CLI entry point: run the command, then exit the process with its status.
pub fn exec(name: &str, interactive: bool, tty: bool, command: &[String]) -> Result<()> {
    std::process::exit(exec_status(name, interactive, tty, command)?);
}

/// Run the command inside the matching instance and return its exit code.
/// Split out of [`exec`] so callers that must not terminate the process (the
/// dashboard prompt) can reuse it. An empty `command` opens the login shell
/// ([`SHELL_FALLBACK_CMD`]) with [`shell_flags`] defaults.
pub fn exec_status(name: &str, interactive: bool, tty: bool, command: &[String]) -> Result<i32> {
    let state = State::load()?;
    let key = resolve_instance(&state, name)?;
    let instance = state.instances.get(&key).expect("key came from state");

    let shell: Vec<String>;
    let (command, interactive, tty) = if command.is_empty() {
        shell = SHELL_FALLBACK_CMD.iter().map(|s| s.to_string()).collect();
        let (i, t) = shell_flags(interactive, tty, std::io::stdin().is_terminal());
        (shell.as_slice(), i, t)
    } else {
        (command, interactive, tty)
    };

    // Same gate as the `SSH_AUTH_SOCK` injection: relay mode with a host agent.
    let host_agent = crate::devsbd::relay_mode(instance) && has_host_agent();
    // The daemon's bridge for the command's lifetime, asked for alongside it:
    // the in-container daemon holds an agent client until the bridge attaches
    // (docs/sandbox-helper.md).
    #[cfg(unix)]
    let relay = host_agent.then(|| AgentRelay::start(&key));
    let args = exec_argv_with(instance, interactive, tty, command, host_agent);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let code = backend().run_inherit(&arg_refs)?;
    #[cfg(unix)]
    if let Some(relay) = relay {
        relay.finish(&instance.container);
    }
    Ok(code)
}

/// The ssh-agent relay a CLI command holds while it runs in a container
/// (`exec`, `start`'s postStartCommand, `run`'s lifecycle chain): a
/// `bridges.ensure` to the daemon, which owns the one bridge per container
/// (docs/ssh-agent.md). Two shapes: [`start`](Self::start) (`exec`) asks on a
/// background thread and never waits, since the poll normally bridged a
/// running instance already; [`ready`](Self::ready) (lifecycle commands,
/// which may `git clone` over ssh at once) waits for the bridge before the
/// command starts. Either way the connection stays open while the command
/// runs, which makes it a daemon holder, and nothing fails the command: a
/// relay that isn't there is one `note:`.
#[cfg(unix)]
pub(crate) struct AgentRelay(std::thread::JoinHandle<Result<crate::serve::client::Conn>>);

/// How long a lifecycle command waits for its bridge: past the bridge's own
/// 10 s handshake timeout, so a timed-out handshake still says so.
#[cfg(unix)]
const LIFECYCLE_RELAY_WAIT: std::time::Duration = std::time::Duration::from_secs(12);

/// Why a waited relay isn't usable, if it isn't: `None` when ready, or when
/// an older daemon couldn't wait (no answer either way).
#[cfg(unix)]
fn relay_note(ready: Option<crate::devsbd::bridge::Readiness>) -> Option<String> {
    match ready {
        None => None,
        Some(r) if r.ready => None,
        Some(r) => Some(r.error.unwrap_or_else(|| "the bridge isn't ready".into())),
    }
}

#[cfg(unix)]
impl AgentRelay {
    /// Ask for instance `key`'s bridge, reporting this process's agent.
    pub(crate) fn start(key: &str) -> AgentRelay {
        let key = key.to_string();
        AgentRelay(std::thread::spawn(move || {
            let agent = crate::devsbd::bridge::own_agent();
            crate::serve::client::ensure_bridge(&key, agent.as_deref(), None).map(|(conn, _)| conn)
        }))
    }

    /// Ask for instance `key`'s bridge and wait (up to
    /// [`LIFECYCLE_RELAY_WAIT`]) for its handshake, before a lifecycle command
    /// starts. Not ready → the `note:` now, on stderr (stdout stays clean for
    /// `run --json`); the command runs anyway. Hold the returned connection
    /// until the command is over.
    pub(crate) fn ready(key: &str, container: &str) -> Option<crate::serve::client::Conn> {
        let agent = crate::devsbd::bridge::own_agent();
        let (conn, note) = match crate::serve::client::ensure_bridge(key, agent.as_deref(), Some(LIFECYCLE_RELAY_WAIT)) {
            Ok((conn, ready)) => (Some(conn), relay_note(ready)),
            Err(e) => (None, Some(format!("{e:#}"))),
        };
        if let Some(why) = note {
            eprintln!("note: ssh-agent relay unavailable in `{container}`: {why}");
        }
        conn
    }

    /// The command is over: wait for the request if it's still out, report
    /// a failure (on stderr, after the command so it can't interleave with
    /// its output; stdout stays clean for `run --json`) and hang up.
    pub(crate) fn finish(self, container: &str) {
        let result = self.0.join().unwrap_or_else(|_| Err(anyhow::anyhow!("the request panicked")));
        if let Err(e) = result {
            eprintln!("note: ssh-agent relay unavailable in `{container}`: {e:#}");
        }
    }
}

/// `-i`/`-t` for a command-less (login shell) exec. Explicit flags are taken
/// as given. With neither, the shell keeps stdin open (`-i`: without it the
/// shell reads EOF and exits at once, so piping a script in still works) and
/// gets a pseudo-TTY when stdin is a terminal (`-t`), which is what an
/// interactive user or a pty-hosting consumer (node-pty, a GUI terminal) wants
/// without having to know the flags.
fn shell_flags(interactive: bool, tty: bool, stdin_tty: bool) -> (bool, bool) {
    if interactive || tty {
        (interactive, tty)
    } else {
        (true, stdin_tty)
    }
}

/// Build the runtime `exec` argv for `instance`: the flags, workspace,
/// remoteUser, env (`Instance::exec_env`: remoteEnv, then the saved
/// `--env`), container, and command, in the order the CLI has
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
/// only); mount mode ignores it and injects unconditionally. For callers that
/// probed it already (to ask the daemon for a bridge on the same answer).
pub(crate) fn exec_argv_with(
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
    for (key, value) in &instance.exec_env() {
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
            folders: Vec::new(),
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
            forwarded_ports: Default::default(),
            done: None,
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

    /// The saved `--env` rides every exec, overriding remoteEnv on a clash.
    #[test]
    fn argv_includes_extra_env_over_remote_env() {
        let mut inst = instance();
        inst.remote_env.insert("FOO".into(), "remote".into());
        inst.remote_env.insert("BAZ".into(), "qux".into());
        inst.extra_env.insert("FOO".into(), "extra".into());
        inst.extra_env.insert("PR".into(), "42".into());
        assert_eq!(
            exec_argv_with(&inst, false, false, &["ls".into()], false),
            vec![
                "exec",
                "-w",
                "/workspaces/repository-1",
                "-e",
                "BAZ=qux",
                "-e",
                "FOO=extra",
                "-e",
                "PR=42",
                "devsandbox-repo-abc1",
                "ls",
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

    #[cfg(unix)]
    #[test]
    fn relay_note_only_for_a_relay_known_not_ready() {
        use crate::devsbd::bridge::Readiness;
        assert_eq!(relay_note(None), None, "older daemon: no verdict");
        assert_eq!(relay_note(Some(Readiness { ready: true, error: None })), None);
        let outdated = Readiness { ready: false, error: Some("helper in c is outdated".into()) };
        assert_eq!(relay_note(Some(outdated)).as_deref(), Some("helper in c is outdated"));
        assert_eq!(relay_note(Some(Readiness { ready: false, error: None })).as_deref(), Some("the bridge isn't ready"));
    }

    #[test]
    fn shell_flags_default_only_without_explicit_flags() {
        // Neither flag: stdin stays open, a TTY only when stdin is one.
        assert_eq!(shell_flags(false, false, true), (true, true));
        assert_eq!(shell_flags(false, false, false), (true, false));
        // Any explicit flag is honored exactly, TTY or not.
        assert_eq!(shell_flags(true, false, true), (true, false));
        assert_eq!(shell_flags(false, true, false), (false, true));
        assert_eq!(shell_flags(true, true, false), (true, true));
    }

    #[test]
    fn shell_fallback_cmd_probes_zsh_bash_sh() {
        assert_eq!(SHELL_FALLBACK_CMD[0], "sh");
        assert_eq!(SHELL_FALLBACK_CMD[1], "-lc");
        // zsh probed before bash before sh, all as login shells.
        let at = |s: &str| SHELL_FALLBACK_CMD[2].find(s).unwrap();
        assert!(at("exec zsh -l") < at("exec bash -l"));
        assert!(at("exec bash -l") < at("exec sh -l"));
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
