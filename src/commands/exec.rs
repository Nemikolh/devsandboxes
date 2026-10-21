use std::io::IsTerminal;

use anyhow::{bail, Result};

use super::resolve_instance;
use crate::devsbd::control::valid_run_id;
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

/// What one `devsandbox exec` does in its instance.
pub enum Exec<'a> {
    /// Run `command` (empty: the login shell), through a session when
    /// [`session_for`] says so; `session` is the caller's id (`--session`).
    Run { interactive: bool, tty: bool, command: &'a [String], session: Option<&'a str> },
    /// `--attach [ID]`: attach to session `ID`, or to the one detached
    /// session ([`choose_detached`]).
    Attach(Option<&'a str>),
    /// `--sessions`: the helper's `session ls`.
    Sessions,
}

/// CLI entry point: run the command, then exit the process with its status
/// ([`reported_code`] under `reserve_255`).
pub fn exec(name: &str, what: Exec, reserve_255: bool) -> Result<()> {
    std::process::exit(reported_code(exec_status(name, what)?, reserve_255));
}

/// The exit code `exec` reports for the command's `code`. `reserve_255`
/// (the hidden `--reserve-255`, passed by the reconnect loops over ssh,
/// `serve::machines::cli::Reconnect`): ssh exits 255 when the connection
/// fails, and a reconnect's attach to an ended session reports the program's
/// recorded code, so a program that exited 255 would look like a lost
/// connection forever. It reports 254 instead: lossy, but 255 then only ever
/// means ssh.
pub(crate) fn reported_code(code: i32, reserve_255: bool) -> i32 {
    if reserve_255 && code == 255 { 254 } else { code }
}

/// Run `what` inside the matching instance and return its exit code. An
/// empty command opens the login shell ([`SHELL_FALLBACK_CMD`]) with
/// [`shell_flags`] defaults.
pub fn exec_status(name: &str, what: Exec) -> Result<i32> {
    let state = State::load()?;
    let key = resolve_instance(&state, name)?;
    let instance = state.instances.get(&key).expect("key came from state");
    if let Some(why) = super::no_container_yet(&key, instance.phase_view().as_ref()) {
        bail!("{why}");
    }
    let stdin_tty = std::io::stdin().is_terminal();
    let helper = instance.devsbd_arch.is_some();

    match what {
        Exec::Sessions => {
            if !helper {
                bail!("`{key}` has no devsbd helper, so no sessions");
            }
            let listing = backend().output(&refs(&session_ls_argv(instance)))?;
            if listing.is_empty() {
                eprintln!("no sessions in `{key}`");
            } else {
                print!("{listing}");
            }
            Ok(0)
        }
        Exec::Attach(id) => {
            if !helper {
                bail!("`{key}` has no devsbd helper, so no sessions to attach to");
            }
            let id = match id {
                Some(id) => checked_session_id(id)?.to_string(),
                None => {
                    let listing = backend().output(&refs(&session_ls_argv(instance)))?;
                    choose_detached(&key, &listing, stdin_tty, &mut |prompt, options| super::pick(prompt, options))?
                }
            };
            let (interactive, tty) = shell_flags(false, false, stdin_tty);
            run_in(&key, instance, interactive, tty, &session_attach_command(&id))
        }
        Exec::Run { interactive, tty, command, session } => {
            if let Some(id) = session {
                checked_session_id(id)?;
            }
            let shell: Vec<String>;
            let (command, interactive, tty) = if command.is_empty() {
                shell = SHELL_FALLBACK_CMD.iter().map(|s| s.to_string()).collect();
                let (i, t) = shell_flags(interactive, tty, stdin_tty);
                (shell.as_slice(), i, t)
            } else {
                (command, interactive, tty)
            };
            let wrapped = session_for(helper, tty, session, || mint_session_id(Instance::now()))
                .map(|id| session_new_command(&id, command));
            run_in(&key, instance, interactive, tty, wrapped.as_deref().unwrap_or(command))
        }
    }
}

fn refs(args: &[String]) -> Vec<&str> {
    args.iter().map(String::as_str).collect()
}

/// Run `command` in `instance` (instance `key`) with stdio inherited, under
/// the ssh-agent relay when it applies; the runtime's exit code.
fn run_in(key: &str, instance: &Instance, interactive: bool, tty: bool, command: &[String]) -> Result<i32> {
    // Same gate as the `SSH_AUTH_SOCK` injection: relay mode with a host agent.
    let host_agent = crate::devsbd::relay_mode(instance) && has_host_agent();
    // The daemon's bridge for the command's lifetime, asked for alongside it:
    // the in-container daemon holds an agent client until the bridge attaches
    // (docs/architecture/helper/in-container.md).
    #[cfg(unix)]
    let relay = host_agent.then(|| AgentRelay::start(key));
    let args = exec_argv_with(instance, interactive, tty, command, host_agent);
    let code = backend().run_inherit(&refs(&args))?;
    #[cfg(unix)]
    if let Some(relay) = relay {
        relay.finish(&instance.container);
    }
    Ok(code)
}

/// The ssh-agent relay a CLI command holds while it runs in a container
/// (`exec`, `start`'s postStartCommand, `run`'s lifecycle chain): a
/// `bridges.ensure` to the daemon, which owns the one bridge per container
/// (docs/architecture/helper/ssh-agent.md). Two shapes: [`start`](Self::start)
/// (`exec`) asks on a background thread and never waits, since the poll
/// normally bridged a running instance already; [`ready`](Self::ready)
/// (lifecycle commands, which may `git clone` over ssh at once) waits for the
/// bridge before the command starts. Either way the connection stays open
/// while the command runs, which makes it a daemon holder, and nothing fails
/// the command: a relay that isn't there is one `note:`.
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
        let (conn, note) = Self::wait(key, agent.as_deref());
        if let Some(why) = note {
            eprintln!("note: ssh-agent relay unavailable in `{container}`: {why}");
        }
        conn
    }

    /// [`ready`](Self::ready)'s wait, reporting `agent`, without the
    /// printing: the connection, and why the relay isn't usable if it isn't.
    pub(crate) fn wait(
        key: &str,
        agent: Option<&std::path::Path>,
    ) -> (Option<crate::serve::client::Conn>, Option<String>) {
        match crate::serve::client::ensure_bridge(key, agent, Some(LIFECYCLE_RELAY_WAIT)) {
            Ok((conn, ready)) => (Some(conn), relay_note(ready)),
            Err(e) => (None, Some(format!("{e:#}"))),
        }
    }

    /// [`wait`](Self::wait) from inside the daemon (a dispatcher's child op):
    /// reports no agent (its own `$SSH_AUTH_SOCK` is already its last
    /// candidate, and reporting it would rank it above the clients' fresher
    /// ones) and never starts a daemon
    /// ([`ensure_bridge_running`](crate::serve::client::ensure_bridge_running)).
    /// Why the relay isn't usable, if it isn't.
    pub(crate) fn wait_in_daemon(key: &str) -> Option<String> {
        let dir = match crate::serve::endpoint::socket_dir() {
            Ok(dir) => dir,
            Err(e) => return Some(format!("{e:#}")),
        };
        match crate::serve::client::ensure_bridge_running(&dir, key, None, Some(LIFECYCLE_RELAY_WAIT)) {
            Ok(Some((_conn, ready))) => relay_note(ready),
            Ok(None) => Some("devsandbox serve is exiting".into()),
            Err(e) => Some(format!("{e:#}")),
        }
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
pub(crate) fn shell_flags(interactive: bool, tty: bool, stdin_tty: bool) -> (bool, bool) {
    if interactive || tty {
        (interactive, tty)
    } else {
        (true, stdin_tty)
    }
}

/// The session a `Run` exec goes through, if any: none without the helper
/// (`helper`: `Instance::devsbd_arch` is `Some`); else the caller's id
/// (`requested`, `--session`, whether or not there is a tty); else a fresh
/// id from `mint` when the exec gets a tty: every interactive exec is
/// resumable, every command-less one a new shell. Piped execs stay plain
/// (nothing to re-attach to).
fn session_for(helper: bool, tty: bool, requested: Option<&str>, mint: impl FnOnce() -> String) -> Option<String> {
    match requested {
        _ if !helper => None,
        Some(id) => Some(id.to_string()),
        None => tty.then(mint),
    }
}

/// The in-container command that creates session `id` running `command` and
/// attaches to it (an `id` already live attaches instead). No `--cwd`: the
/// supervisor starts in the client's cwd and env, which the runtime exec's
/// `-w`/`-u`/`-e` set as for a plain exec.
fn session_new_command(id: &str, command: &[String]) -> Vec<String> {
    let head = [crate::devsbd::BIN, "session", "new", "--id", id, "--"];
    head.iter().map(|s| s.to_string()).chain(command.iter().cloned()).collect()
}

/// The in-container command attaching to session `id`.
fn session_attach_command(id: &str) -> Vec<String> {
    [crate::devsbd::BIN, "session", "attach", id].iter().map(|s| s.to_string()).collect()
}

/// The runtime argv listing `instance`'s sessions (as its user: a session's
/// socket is its user's).
fn session_ls_argv(instance: &Instance) -> Vec<String> {
    let ls: Vec<String> = [crate::devsbd::BIN, "session", "ls"].iter().map(|s| s.to_string()).collect();
    exec_argv_with(instance, false, false, &ls, false)
}

/// A fresh session id at `now` (unix secs), in `control::valid_run_id`'s
/// shape so a name sort is start order. The hex comes from std's
/// per-process random hash keys (as `inbox`'s event ids): no dependency. A
/// collision would attach to that other session, so the odds matter: 1 in
/// 65536 among sessions of one instance started in the same second.
pub(crate) fn mint_session_id(now: u64) -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(now);
    h.write_u32(std::process::id());
    format!("{now:010}-{:04x}", h.finish() as u16)
}

/// `id` when it is a session id the helper accepts (checked here too, so a
/// typo fails before any runtime call).
pub(crate) fn checked_session_id(id: &str) -> Result<&str> {
    if !valid_run_id(id) {
        bail!("bad session id `{id}` (expected <unix secs:010>-<4 hex>, see --sessions)");
    }
    Ok(id)
}

/// The running, detached sessions in a `devsbd session ls` listing (lines
/// `<id> <state> <attached|detached|-> <started> <argv…>`): `(id, line)`.
fn detached_sessions(listing: &str) -> Vec<(&str, &str)> {
    listing
        .lines()
        .filter_map(|line| {
            let mut words = line.split(' ');
            let id = words.next()?;
            (words.next() == Some("running") && words.next() == Some("detached")).then_some((id, line))
        })
        .collect()
}

/// `--attach` without an id: the one detached session in `listing`; several
/// → `pick` on a TTY (`tty`), else an error naming them; none → an error.
fn choose_detached(
    key: &str,
    listing: &str,
    tty: bool,
    pick: &mut dyn FnMut(&str, &[&str]) -> Result<usize>,
) -> Result<String> {
    let found = detached_sessions(listing);
    match found.as_slice() {
        [] => bail!("no detached session in `{key}` (devsandbox exec {key} --sessions)"),
        [(id, _)] => Ok(id.to_string()),
        _ if tty => {
            let lines: Vec<&str> = found.iter().map(|(_, line)| *line).collect();
            let i = pick(&format!("`{key}` has several detached sessions"), &lines)?;
            Ok(found[i].0.to_string())
        }
        _ => {
            let ids: Vec<&str> = found.iter().map(|(id, _)| *id).collect();
            bail!("`{key}` has several detached sessions, pick one with --attach <id>: {}", ids.join(", "))
        }
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
    // remote_env, via the shared rule (docs/architecture/helper/ssh-agent.md).
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
            progress: None,
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

    const ID: &str = "1790000000-a1b2";

    #[test]
    fn session_only_with_the_helper_and_a_tty_or_an_explicit_id() {
        let mint = || ID.to_string();
        let never = || -> String { panic!("minted") };
        // No helper: plain exec, even with an explicit id (no session to make).
        assert_eq!(session_for(false, true, None, never), None);
        assert_eq!(session_for(false, true, Some("1790000000-0000"), never), None);
        // Helper, no tty: a piped exec stays plain.
        assert_eq!(session_for(true, false, None, never), None);
        // Helper and a tty (explicit `-t`, or the login-shell default on a
        // terminal, both decided before): a fresh session.
        assert_eq!(session_for(true, true, None, mint).as_deref(), Some(ID));
        // `--session`: the caller's id, tty or not.
        assert_eq!(session_for(true, false, Some("1790000000-0000"), never).as_deref(), Some("1790000000-0000"));
        assert_eq!(session_for(true, true, Some("1790000000-0000"), never).as_deref(), Some("1790000000-0000"));
    }

    /// The command-less shell on a terminal: `shell_flags` gives `-t`, so a
    /// session wraps the login shell, with the same `-w`/`-u`/`-e` flags.
    #[test]
    fn session_argv_wraps_the_command_after_the_plain_flags() {
        let mut inst = instance();
        inst.remote_user = Some("vscode".into());
        inst.remote_env.insert("FOO".into(), "bar".into());
        let (i, t) = shell_flags(false, false, true);
        let shell: Vec<String> = SHELL_FALLBACK_CMD.iter().map(|s| s.to_string()).collect();
        let id = session_for(true, t, None, || ID.to_string()).unwrap();
        let argv = exec_argv_with(&inst, i, t, &session_new_command(&id, &shell), false);
        let mut want: Vec<String> = [
            "exec", "-i", "-t", "-w", "/workspaces/repository-1", "-u", "vscode", "-e", "FOO=bar",
            "devsandbox-repo-abc1", crate::devsbd::BIN, "session", "new", "--id", ID, "--",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        want.extend(shell);
        assert_eq!(argv, want);
        assert_eq!(session_attach_command(ID), [crate::devsbd::BIN, "session", "attach", ID]);
        assert_eq!(
            session_ls_argv(&inst),
            ["exec", "-w", "/workspaces/repository-1", "-u", "vscode", "-e", "FOO=bar", "devsandbox-repo-abc1", crate::devsbd::BIN, "session", "ls"]
        );
    }

    #[test]
    fn minted_ids_are_valid_run_ids() {
        for now in [0, 1_790_000_000, 9_999_999_999] {
            let id = mint_session_id(now);
            assert!(valid_run_id(&id), "{id}");
            assert!(id.starts_with(&format!("{now:010}-")), "{id}");
        }
        assert!(checked_session_id(ID).is_ok());
        for bad in ["", "abc", "1790000000-A1B2", "../../etc", "1790000000-a1b2 "] {
            assert!(checked_session_id(bad).is_err(), "{bad:?}");
        }
    }

    const LISTING: &str = "\
1790000000-0001 exited 0 - 2026-09-21T10:00:00Z sh -lc x
1790000000-0002 running attached 2026-09-21T10:00:01Z zidane
1790000000-0003 running detached 2026-09-21T10:00:02Z claude
1790000000-0004 lost - 2026-09-21T10:00:03Z sh
1790000000-0005 killed 9 - 2026-09-21T10:00:04Z sh
";

    #[test]
    fn detached_sessions_are_the_running_unattached_ones() {
        assert_eq!(
            detached_sessions(LISTING),
            vec![("1790000000-0003", "1790000000-0003 running detached 2026-09-21T10:00:02Z claude")]
        );
        assert_eq!(detached_sessions(""), vec![]);
    }

    #[test]
    fn attach_without_an_id_picks_the_detached_one() {
        let mut never = |_: &str, _: &[&str]| -> Result<usize> { panic!("picked") };
        assert_eq!(choose_detached("web", LISTING, false, &mut never).unwrap(), "1790000000-0003");
        let e = choose_detached("web", "1790000000-0002 running attached 2026-09-21T10:00:01Z sh\n", true, &mut never);
        assert!(e.unwrap_err().to_string().starts_with("no detached session in `web`"));

        let two = "1790000000-0003 running detached 2026-09-21T10:00:02Z claude\n\
                   1790000000-0006 running detached 2026-09-21T10:00:05Z zsh -l\n";
        let e = choose_detached("web", two, false, &mut never).unwrap_err().to_string();
        assert_eq!(e, "`web` has several detached sessions, pick one with --attach <id>: 1790000000-0003, 1790000000-0006");
        let mut second = |prompt: &str, options: &[&str]| {
            assert_eq!(prompt, "`web` has several detached sessions");
            assert_eq!(options, ["1790000000-0003 running detached 2026-09-21T10:00:02Z claude", "1790000000-0006 running detached 2026-09-21T10:00:05Z zsh -l"]);
            Ok(1)
        };
        assert_eq!(choose_detached("web", two, true, &mut second).unwrap(), "1790000000-0006");
    }

    #[test]
    fn reserve_255_reports_a_programs_255_as_254() {
        assert_eq!(reported_code(255, true), 254);
        for code in [0, 1, 130, 250, 251, 254] {
            assert_eq!(reported_code(code, true), code);
        }
        assert_eq!(reported_code(255, false), 255, "off: as is");
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
