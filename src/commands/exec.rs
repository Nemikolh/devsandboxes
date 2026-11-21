use std::io::IsTerminal;

use anyhow::{bail, Context, Result};

use super::resolve_instance;
use crate::devsbd::control::valid_run_id;
use crate::runtime::backend;
use crate::runtime::procs::{parse_session, Session};
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
    /// `--attach [ID]`: attach to session `ID`, or to the one
    /// [`choose_session`] picks.
    Attach(Option<&'a str>),
    /// `--sessions`: the helper's `session ls`.
    Sessions,
    /// `--end ID`: end session `ID` (the helper's `session rm`).
    End(&'a str),
    /// `--service NAME`: `command` (empty: the login shell) in the container
    /// of service `service` the instance uses, `-i -t`, never a session.
    Service { service: &'a str, command: &'a [String] },
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
    let terminal = stdin_tty && std::io::stdout().is_terminal();
    let helper = instance.devsbd_arch.is_some();

    match what {
        Exec::Service { service, command } => {
            if !stdin_tty {
                bail!("`exec --service` opens a terminal in the service's container: it needs a TTY");
            }
            let container = instance_service_container(&key, instance, &state, service)?;
            if !super::services::container_running(&container) {
                bail!("service `{service}` of `{key}` isn't running ({container})");
            }
            // Through its instance: where the instance's opens go.
            #[cfg(unix)]
            let _attach = crate::serve::client::attach_hold(&instance.instance_id, None);
            backend().run_inherit(&refs(&service_exec_argv(&container, command)))
        }
        Exec::Sessions => {
            if !helper {
                bail!("`{key}` has no devsbd helper, so no sessions");
            }
            let listing = backend().output(&refs(&session_argv(instance, &["ls"])))?;
            if listing.is_empty() {
                eprintln!("no sessions in `{key}`");
            } else {
                print!("{listing}");
            }
            Ok(0)
        }
        Exec::End(id) => {
            let id = checked_session_id(id)?;
            if !helper {
                bail!("`{key}` has no devsbd helper, so no sessions");
            }
            let argv = session_argv(instance, &["rm", id]);
            let out = std::process::Command::new(backend().bin())
                .args(&argv)
                .stdin(std::process::Stdio::null())
                .output()
                .with_context(|| format!("failed to run {} (is it installed?)", backend().bin()))?;
            ended(&key, id, out.status.code().unwrap_or(1), &String::from_utf8_lossy(&out.stderr))?;
            println!("ended session {id} in {key}");
            Ok(0)
        }
        Exec::Attach(id) => {
            if !helper {
                bail!("`{key}` has no devsbd helper, so no sessions to attach to");
            }
            let id = match id {
                Some(id) => checked_session_id(id)?.to_string(),
                None => {
                    let listing = backend().output(&refs(&session_argv(instance, &["ls"])))?;
                    choose_session(&key, &listing, stdin_tty, &mut |prompt, options| super::pick(prompt, options))?
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
                let (i, t) = command_flags(interactive, tty, terminal);
                (command, i, t)
            };
            let wrapped = session_for(helper, tty, session, || mint_session_id(Instance::now()))
                .map(|id| session_new_command(&id, command));
            run_in(&key, instance, interactive, tty, wrapped.as_deref().unwrap_or(command))
        }
    }
}

/// The container of service `service` instance `key` uses, from the
/// instance's own config root (`port::service_container_for`, as the
/// forwards and a fleet worker derive it).
fn instance_service_container(key: &str, instance: &Instance, state: &State, service: &str) -> Result<String> {
    let Some(dir) = instance.config_dir.as_deref() else {
        bail!("`{key}` has no config root recorded, so its services can't be found");
    };
    let config = crate::config::Config::load(dir)?;
    let project = super::services::project_id(dir)?;
    super::port::service_container_for(&config, state, &project, key, service).map_err(anyhow::Error::msg)
}

/// The runtime argv of a terminal in service container `container`:
/// `exec -i -t <container>` and `command`, else the login shell, as the
/// dashboard's service tabs run it.
fn service_exec_argv(container: &str, command: &[String]) -> Vec<String> {
    let mut args: Vec<String> = ["exec", "-i", "-t", container].map(String::from).to_vec();
    if command.is_empty() {
        args.extend(SHELL_FALLBACK_CMD.iter().map(|s| s.to_string()));
    } else {
        args.extend(command.iter().cloned());
    }
    args
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
    // On a terminal, this host's user is at the instance while the command
    // runs: its opens come here (`instances.attach`, best effort). A
    // non-tty exec (a script, cron, an agent) records nothing, so it can't
    // take a user's opens away from where they are.
    #[cfg(unix)]
    let _attach = tty.then(|| crate::serve::client::attach_hold(&instance.instance_id, None)).flatten();
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

/// `-i`/`-t` for an exec with a command. Explicit flags are taken as given.
/// With neither, both when this process is on a terminal (`terminal`: stdin
/// and stdout), so `exec web claude` gets a TTY (its size, raw input, mouse)
/// and a session like the shell does; any redirect (`exec web cat f > out`,
/// a script, an agent's tool call) keeps plain piped stdio.
pub(crate) fn command_flags(interactive: bool, tty: bool, terminal: bool) -> (bool, bool) {
    if interactive || tty { (interactive, tty) } else { (terminal, terminal) }
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
/// `-w`/`-u`/`-e` set as for a plain exec. Also a machine's piped
/// `exec --session` (`machines::cli::pipe_mode`).
pub(crate) fn session_new_command(id: &str, command: &[String]) -> Vec<String> {
    let head = [crate::devsbd::BIN, "session", "new", "--id", id, "--"];
    head.iter().map(|s| s.to_string()).chain(command.iter().cloned()).collect()
}

/// The in-container command attaching to session `id` (also a machine's
/// piped `exec --attach`: `machines::cli::attach_mode`).
pub(crate) fn session_attach_command(id: &str) -> Vec<String> {
    [crate::devsbd::BIN, "session", "attach", id].iter().map(|s| s.to_string()).collect()
}

/// The runtime argv of the helper's `session <args>` in `instance` (`ls`,
/// `rm <id>`), as its user: a session's socket and dir are its user's.
fn session_argv(instance: &Instance, args: &[&str]) -> Vec<String> {
    let command: Vec<String> =
        [crate::devsbd::BIN, "session"].iter().chain(args).map(|s| s.to_string()).collect();
    exec_argv_with(instance, false, false, &command, false)
}

/// The helper's `NO_SESSION_EXIT` (`devsbd/src/sessions.rs`): no such
/// session, for an attach (unknown or `lost`) and for `session rm`.
pub(crate) const NO_SESSION_EXIT: i32 = 250;

/// How `--end`'s `session rm <id>` in instance `key` went, from its exit
/// `code` and `stderr`: the exit code, not the text, says the session isn't
/// there ([`NO_SESSION_EXIT`]); any other failure carries stderr's last
/// line (the helper's or the runtime's own reason).
fn ended(key: &str, id: &str, code: i32, stderr: &str) -> Result<()> {
    match code {
        0 => Ok(()),
        NO_SESSION_EXIT => bail!("no session {id} in {key} (devsandbox exec {key} --sessions)"),
        _ => {
            let why = stderr.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("non-zero exit");
            bail!("ending session {id} in {key} failed (exit {code}): {why}")
        }
    }
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

/// What an attach without an id chooses from, in a `devsbd session ls`
/// listing (lines `<id> <state> <attached|detached|-> <started> <argv…>`),
/// as `(session, line)`: the one detached running session when there is
/// exactly one, else every running one, detached first. Attached ones count
/// because `attached` only means a client is connected: the one docker
/// leaves behind after its terminal went stays connected, and nothing in
/// the container can tell (docs/architecture/helper/in-container.md,
/// *Sessions*); attaching takes it over.
pub(crate) fn attach_candidates(listing: &str) -> Vec<(Session, &str)> {
    let mut running: Vec<(Session, &str)> = listing
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| (parse_session(line), line))
        .filter(|(s, _)| s.running())
        .collect();
    // Stable: listing order kept within each group.
    running.sort_by_key(|(s, _)| s.attached);
    if running.iter().filter(|(s, _)| !s.attached).count() == 1 {
        running.truncate(1);
    }
    running
}

/// `--attach` without an id: the one of [`attach_candidates`]; several →
/// `pick` on a TTY (`tty`), else an error naming them; none → an error.
pub(crate) fn choose_session(
    key: &str,
    listing: &str,
    tty: bool,
    pick: &mut dyn FnMut(&str, &[&str]) -> Result<usize>,
) -> Result<String> {
    let found = attach_candidates(listing);
    match found.as_slice() {
        [] => bail!("no running session in `{key}` (devsandbox exec {key} --sessions)"),
        [(s, _)] => Ok(s.id.clone()),
        _ if tty => {
            let lines: Vec<&str> = found.iter().map(|(_, line)| *line).collect();
            let i = pick(&format!("`{key}`: attach to a session (an attached one is taken over)"), &lines)?;
            Ok(found[i].0.id.clone())
        }
        _ => {
            let ids: Vec<String> = found
                .iter()
                .map(|(s, _)| format!("{} ({})", s.id, if s.attached { "attached" } else { "detached" }))
                .collect();
            bail!("`{key}` has several running sessions, pick one with --attach <id>: {}", ids.join(", "))
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

/// Where and as whom a command runs in `instance`, and with what env: the
/// workspace, the remote user (`None`: the container's own), and
/// `Instance::exec_env` then `SSH_AUTH_SOCK` ([`ssh_auth_sock_env`]). What
/// the runtime exec argv sets ([`exec_argv_with`]) and what a fleet `PTY`
/// session is created with (`serve::fleet::pty`), so the two can't drift.
pub(crate) struct ExecContext<'a> {
    pub cwd: &'a str,
    pub user: Option<&'a str>,
    pub env: Vec<(String, String)>,
}

/// [`ExecContext`] of `instance`; `host_agent` as for [`ssh_auth_sock_env`].
pub(crate) fn exec_context(instance: &Instance, host_agent: bool) -> ExecContext<'_> {
    let mut env: Vec<(String, String)> = instance.exec_env().into_iter().collect();
    // agent forwarding: the activating env var rides every exec, same as
    // remote_env, via the shared rule (docs/architecture/helper/ssh-agent.md).
    if let Some(sock) = ssh_auth_sock_env(instance, host_agent) {
        env.push(("SSH_AUTH_SOCK".into(), sock.to_string()));
    }
    ExecContext { cwd: &instance.workspace, user: instance.remote_user.as_deref(), env }
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
    let ctx = exec_context(instance, host_agent);
    args.extend(["-w".into(), ctx.cwd.to_string()]);
    if let Some(user) = ctx.user {
        args.push("-u".into());
        args.push(user.to_string());
    }
    for (key, value) in &ctx.env {
        args.push("-e".into());
        args.push(format!("{key}={value}"));
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
            home: None,
            browser_sandbox: None,
            tool_env: Default::default(),
            ssh_auth_sock: None,
            devsbd_arch: None,
            devsbd_error: None,
            volumes: Vec::new(),
            bind_dirs: Vec::new(),
            dispatcher: None,
            profile: None,
            config_dir: None,
            extra_env: Default::default(),
            forwarded_ports: Default::default(),
            done: None,
            incarnation: None,
            claims: Vec::new(),
            progress: None,
            created_unix: 0,
        }
    }

    #[test]
    fn a_service_terminal_is_the_dashboards_service_tab() {
        let shell: Vec<String> = SHELL_FALLBACK_CMD.iter().map(|s| s.to_string()).collect();
        let argv = service_exec_argv("devsandbox-svc-p-db", &[]);
        assert_eq!(argv[..4], ["exec", "-i", "-t", "devsandbox-svc-p-db"]);
        assert_eq!(argv[4..], shell);
        assert_eq!(service_exec_argv("c", &["psql".into()]), ["exec", "-i", "-t", "c", "psql"]);
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
        let plain = ["exec", "-w", "/workspaces/repository-1", "-u", "vscode", "-e", "FOO=bar", "devsandbox-repo-abc1", crate::devsbd::BIN, "session"];
        assert_eq!(session_argv(&inst, &["ls"]), [&plain[..], &["ls"][..]].concat());
        // `--end`'s: the same user, no -i/-t.
        assert_eq!(session_argv(&inst, &["rm", ID]), [&plain[..], &["rm", ID][..]].concat());
    }

    /// `--end`'s outcome comes from `session rm`'s code: 250 is no such
    /// session (pointing at `--sessions`), any other failure says why.
    #[test]
    fn ending_a_session_reads_the_code() {
        assert!(ended("web", ID, 0, "").is_ok());
        let gone = ended("web", ID, NO_SESSION_EXIT, "devsbd session: no session `x`\n").unwrap_err();
        assert_eq!(gone.to_string(), format!("no session {ID} in web (devsandbox exec web --sessions)"));
        let failed = ended("web", ID, 1, "devsbd session: permission denied\n\n").unwrap_err();
        assert_eq!(failed.to_string(), format!("ending session {ID} in web failed (exit 1): devsbd session: permission denied"));
        assert!(ended("web", ID, 126, "").unwrap_err().to_string().ends_with("(exit 126): non-zero exit"));
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

    /// [`attach_candidates`]' ids for `listing`.
    fn candidate_ids(listing: &str) -> Vec<String> {
        attach_candidates(listing).into_iter().map(|(s, _)| s.id).collect()
    }

    const ENDED: &str = "\
1790000000-0001 exited 0 - 2026-09-21T10:00:00Z sh -lc x
1790000000-0004 lost - 2026-09-21T10:00:03Z sh
1790000000-0005 killed 9 - 2026-09-21T10:00:04Z sh
";
    const D3: &str = "1790000000-0003 running detached 2026-09-21T10:00:02Z claude\n";
    const D6: &str = "1790000000-0006 running detached 2026-09-21T10:00:05Z zsh -l\n";
    const A2: &str = "1790000000-0002 running attached 2026-09-21T10:00:01Z zidane\n";
    const A7: &str = "1790000000-0007 running attached 2026-09-21T10:00:06Z sh\n";

    /// The rule's table: one detached wins over attached ones; else one
    /// running is it; else all running, detached first; ended/lost never.
    #[test]
    fn attach_candidates_follow_the_rule() {
        let cases: [(String, &[&str]); 7] = [
            (String::new(), &[]),
            (ENDED.to_string(), &[]),
            (LISTING.to_string(), &["1790000000-0003"]),
            (format!("{A2}{ENDED}{D3}{A7}"), &["1790000000-0003"]),
            (format!("{ENDED}{A2}"), &["1790000000-0002"]),
            (format!("{D3}{A2}{D6}"), &["1790000000-0003", "1790000000-0006", "1790000000-0002"]),
            (format!("{A7}{ENDED}{A2}"), &["1790000000-0007", "1790000000-0002"]),
        ];
        for (listing, want) in cases {
            assert_eq!(candidate_ids(&listing), want, "{listing}");
        }
        assert_eq!(attach_candidates(D3)[0].1, D3.trim_end(), "the ls line kept for the pick list");
    }

    #[test]
    fn attach_without_an_id_picks_the_detached_one() {
        let mut never = |_: &str, _: &[&str]| -> Result<usize> { panic!("picked") };
        assert_eq!(choose_session("web", LISTING, false, &mut never).unwrap(), "1790000000-0003");
        assert_eq!(choose_session("web", A2, false, &mut never).unwrap(), "1790000000-0002", "the one attached: taken over");
        let e = choose_session("web", ENDED, true, &mut never).unwrap_err().to_string();
        assert_eq!(e, "no running session in `web` (devsandbox exec web --sessions)");

        let several = format!("{A2}{D3}{D6}");
        let e = choose_session("web", &several, false, &mut never).unwrap_err().to_string();
        assert_eq!(
            e,
            "`web` has several running sessions, pick one with --attach <id>: \
             1790000000-0003 (detached), 1790000000-0006 (detached), 1790000000-0002 (attached)"
        );
        let mut third = |prompt: &str, options: &[&str]| {
            assert_eq!(prompt, "`web`: attach to a session (an attached one is taken over)");
            assert_eq!(options, [D3.trim_end(), D6.trim_end(), A2.trim_end()]);
            Ok(2)
        };
        assert_eq!(choose_session("web", &several, true, &mut third).unwrap(), "1790000000-0002");
        let two_attached = format!("{A2}{A7}");
        let e = choose_session("web", &two_attached, false, &mut never).unwrap_err().to_string();
        assert!(e.ends_with("1790000000-0002 (attached), 1790000000-0007 (attached)"), "{e}");
    }

    #[test]
    fn reserve_255_reports_a_programs_255_as_254() {
        assert_eq!(reported_code(255, true), 254);
        for code in [0, 1, 130, 250, 251, 252, 254] {
            assert_eq!(reported_code(code, true), code);
        }
        assert_eq!(reported_code(255, false), 255, "off: as is");
    }

    #[test]
    fn command_flags_default_to_a_tty_only_on_a_terminal() {
        assert_eq!(command_flags(false, false, true), (true, true));
        assert_eq!(command_flags(false, false, false), (false, false));
        assert_eq!(command_flags(true, false, true), (true, false));
        assert_eq!(command_flags(false, true, true), (false, true));
        assert_eq!(command_flags(true, true, false), (true, true));
    }

    #[test]
    fn shell_flags_default_only_without_explicit_flags() {
        // Neither flag: stdin stays open, a TTY only when stdin is one.
        assert_eq!(shell_flags(false, false, true), (true, true));
        assert_eq!(shell_flags(false, false, false), (true, false));
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

        let bare = instance();
        assert_eq!(ssh_auth_sock_env(&bare, true), None);
        assert_eq!(ssh_auth_sock_env(&bare, false), None);
    }
}
