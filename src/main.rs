mod build_log;
mod commands;
mod config;
mod config_identity;
mod daemon_config;
mod devsbd;
mod exe;
mod features;
mod inbox;
mod json_stdout;
mod render;
mod runtime;
#[cfg(unix)]
mod serve;
mod snapshot;
mod state;
mod worker_policy;
#[cfg(test)]
mod test_support;
// The dashboard is a client of devsandbox serve, which is unix-only.
#[cfg(unix)]
mod tui;

use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "devsandbox", version, about = "Manage devcontainer-based sandboxes")]
struct Cli {
    /// Directory containing devsandboxes.toml (defaults to the current directory).
    #[arg(short = 'C', long, global = true, default_value = ".")]
    dir: PathBuf,

    /// Run the verb in this process and never start the daemon: what the
    /// daemon's verb subprocesses (`serve::verbs`) and the dispatcher's run
    /// with. A flag, not an env var, so a `devsandbox` a hook calls doesn't
    /// inherit it.
    #[arg(long, global = true, hide = true)]
    in_process: bool,

    /// Run a command without a name (`ps`, `stats`, `ls`, `status`, `gc`,
    /// `service ls|rebuild`, `stop|start|rebuild --all`, `port --service
    /// <global-svc>`) on this machine
    /// (`devsandbox machine ls`); a named one takes `<machine>/<name>` instead
    #[arg(long, global = true, value_name = "MACHINE")]
    machine: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// List the sandbox configs defined in devsandboxes.toml
    Ls {
        /// Output JSON instead of a table
        #[arg(long)]
        json: bool,
    },
    /// List running sandbox instances
    Ps {
        /// Also show stopped instances
        #[arg(short, long)]
        all: bool,
        /// Output JSON instead of a table
        #[arg(long)]
        json: bool,
    },
    /// Start a new sandbox instance (in the background)
    Run {
        /// Sandbox config to start (`<machine>/<sandbox>` for a machine's,
        /// `@/<sandbox>` for this host's); interactive menu on a TTY when
        /// omitted
        sandbox: Option<String>,
        /// Instance name; generated (and printed) when omitted
        #[arg(long)]
        name: Option<String>,
        /// Branch for a worktree instance; overrides the sandbox's
        /// `worktree-branch` (supports `${instance}`). An existing branch is
        /// checked out (one only on `origin` gets a tracking branch)
        #[arg(long)]
        branch: Option<String>,
        /// Start point for a worktree instance's branch (e.g. `origin/develop`);
        /// overrides the sandbox's `worktree-base` and default-branch detection
        #[arg(long)]
        base: Option<String>,
        /// Extra container env var for this instance only (dispatcher use)
        #[arg(long = "env", hide = true, value_name = "K=V", value_parser = devsbd::control::parse_env)]
        env: Vec<(String, String)>,
        /// Owning dispatcher's instance id (dispatcher use)
        #[arg(long, hide = true, value_name = "INSTANCE_ID")]
        dispatcher: Option<String>,
        /// Print the new instance as one JSON document; child process output
        /// (builds, hooks) goes to stderr so stdout stays parseable
        #[arg(long)]
        json: bool,
        #[arg(long, help = REPROBE_HELP)]
        reprobe: bool,
        #[arg(short, long, help = DETACH_HELP)]
        detach: bool,
    },
    /// Recreate an instance's container from the current config (keeps the
    /// worktree, branch, and per-instance state); no-op when there is no drift
    #[command(visible_alias = "recreate")]
    Rebuild {
        #[arg(required_unless_present = "all", help = NAME_HELP)]
        name: Option<String>,
        /// Rebuild every instance whose config has drifted
        #[arg(long, conflicts_with = "name")]
        all: bool,
        /// Recreate even without config drift (picks up devsandbox-side
        /// behavior changes the drift hashes cannot see)
        #[arg(long)]
        force: bool,
        #[arg(long, help = REPROBE_HELP)]
        reprobe: bool,
        #[arg(short, long, help = DETACH_HELP)]
        detach: bool,
    },
    /// Rename an instance (state only; the container keeps its old name)
    Rename {
        /// Exact instance name (no sandbox/folder resolution);
        /// `<machine>/<name>` for a machine's, `@/<name>` for this host's
        name: String,
        /// New instance name
        new_name: String,
    },
    /// Remove a sandbox instance (container, worktree, state entry)
    Rm {
        #[arg(help = NAME_HELP)]
        name: String,
        /// Delete the worktree's branch without asking (`git branch -D`:
        /// unmerged commits go too)
        #[arg(long, conflicts_with = "keep_branch")]
        delete_branch: bool,
        /// Keep the worktree's branch, even one already merged or on its remote
        #[arg(long)]
        keep_branch: bool,
        /// Remove even with uncommitted changes in the worktree (discarded)
        /// or when a teardown step fails (warned, then skipped)
        #[arg(short, long)]
        force: bool,
        /// Where the branch question would come up, fail (exit 3, the
        /// question as the error) before removing anything: the daemon's
        /// `rm` with `ask`, so the client asks on its own TTY
        #[arg(long, hide = true)]
        fail_on_ask: bool,
    },
    /// Stop a sandbox instance (docker stop; `start` restarts it)
    Stop {
        #[arg(required_unless_present = "all", help = NAME_HELP)]
        name: Option<String>,
        /// Stop every running instance
        #[arg(long, conflicts_with = "name")]
        all: bool,
    },
    /// Start a stopped sandbox instance (services included)
    Start {
        #[arg(required_unless_present = "all", help = NAME_HELP)]
        name: Option<String>,
        /// Start every stopped instance
        #[arg(long, conflicts_with = "name")]
        all: bool,
        #[arg(short, long, help = DETACH_HELP)]
        detach: bool,
    },
    /// Remove shared services no live instance references and orphaned
    /// shell-history files
    Gc {
        /// Delete orphaned shell-history files without asking
        #[arg(long)]
        force: bool,
    },
    /// Manage services
    Service {
        #[command(subcommand)]
        cmd: ServiceCommand,
    },
    /// Read and answer Inbox threads (docs/reference/inbox-cli.md)
    Inbox {
        #[command(subcommand)]
        cmd: InboxCommand,
    },
    /// Show the last lines of a container's logs, or an instance's build log
    Logs {
        #[arg(help = CONTAINER_NAME_HELP)]
        name: String,
        /// Number of log lines to show (container logs only)
        #[arg(short = 'n', long, default_value_t = 50)]
        lines: usize,
        /// Show the instance's build log instead: the whole output of its
        /// last `run`/`rebuild`/`start` (the one before is `build.1.log`
        /// next to it)
        #[arg(long)]
        build: bool,
        /// With --build: keep printing while the instance is still being
        /// built, until it's done or failed
        #[arg(short, long, requires = "build")]
        follow: bool,
    },
    /// Pretty-print the runtime's inspect JSON for a container
    Inspect {
        #[arg(help = CONTAINER_NAME_HELP)]
        name: String,
        /// Output JSON instead of a table
        #[arg(long)]
        json: bool,
    },
    /// Open VS Code attached to a sandbox instance (same as `o` in the TUI)
    Vscode {
        /// Instance name, sandbox config name, or repository folder name
        /// (`@/<name>`: this host's; machines aren't supported yet)
        name: String,
        /// Also open this file (relative to the instance's workspace folder)
        /// at a line and column in the attached window; waits up to 30 s for
        /// the window to attach
        #[arg(long, value_name = "PATH[:LINE[:COL]]", value_parser = commands::vscode::Goto::parse)]
        goto: Option<commands::vscode::Goto>,
    },
    /// Mark a sandbox instance done: kept as is (container, worktree), shown
    /// dimmed until removed
    Done {
        #[arg(help = NAME_HELP)]
        name: String,
    },
    /// Clear an instance's done mark
    Undone {
        #[arg(help = NAME_HELP)]
        name: String,
    },
    /// Show CPU/memory usage of running devsandbox containers
    Stats {
        /// Output JSON instead of a table
        #[arg(long)]
        json: bool,
    },
    /// Machine-readable snapshot of sandboxes, instances, and services
    Status {
        /// Emit JSON; mandatory for now (plain `status` is reserved for a
        /// future human summary)
        #[arg(long, required = true)]
        json: bool,
    },
    /// Run a command in a sandbox instance (a login shell when none is given)
    ///
    /// With a TTY, on an instance with the devsbd helper, the command runs in
    /// a session that outlives this client: if the connection or terminal
    /// goes, it keeps running and `--attach` gets back to it.
    ///
    ///     devsandbox exec web
    ///     devsandbox exec web --sessions
    ///     devsandbox exec web --attach
    ///     devsandbox exec web --attach=1790000000-a1b2
    Exec {
        /// Keep stdin open
        #[arg(short)]
        interactive: bool,
        /// Allocate a pseudo-TTY
        #[arg(short)]
        tty: bool,
        /// Attach to session ID; without ID, to the instance's one detached
        /// session (several: a pick list on a TTY). Give ID as `--attach=ID`
        /// before the name, or after it
        #[arg(long, value_name = "ID", num_args = 0..=1, conflicts_with_all = ["command", "sessions", "session", "interactive", "tty"])]
        attach: Option<Option<String>>,
        /// List the instance's sessions: id, state, attached or detached,
        /// started (UTC), command
        #[arg(long, conflicts_with_all = ["command", "session", "interactive", "tty"])]
        sessions: bool,
        /// Run in session ID, attaching if it is live (even without a TTY);
        /// without the helper, a plain exec
        #[arg(long, value_name = "ID")]
        session: Option<String>,
        /// Report a command's exit code 255 as 254, so 255 only ever means
        /// ssh's own failure: what the reconnect loops over ssh pass
        /// (`commands::exec::reported_code`)
        #[arg(long, hide = true)]
        reserve_255: bool,
        #[arg(help = NAME_HELP)]
        name: String,
        /// Command and arguments to run; omitted: a login shell (zsh, bash,
        /// or sh), with `-i`, plus `-t` when stdin is a TTY, unless either
        /// flag is given
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Forward a container port to the host until Ctrl-C
    ///
    /// Each `<port>` binds the same port on the host (falling back to a free
    /// port if it's taken); `<host>:<port>` binds an explicit host port and
    /// fails if it's in use. Several specs forward in parallel.
    ///
    ///     devsandbox port api 3000
    ///     devsandbox port api --service postgres 5432
    ///     devsandbox port --service redis 6379
    ///     devsandbox port box/api 3000
    ///     devsandbox port --machine box --service redis 6379
    Port {
        /// Instance name, sandbox config name, or repository folder name
        /// (`<machine>/<name>`: a machine's, forwarded here over ssh;
        /// `@/<name>`: this host's); omit only with `--service <global-svc>`
        /// (`--machine <m>`: that machine's)
        name: Option<String>,
        /// Forward a service's port instead of the instance's own; a global
        /// service needs no instance name, an isolated one does
        #[arg(long)]
        service: Option<String>,
        /// Host address to bind (use `0.0.0.0` to expose beyond loopback)
        #[arg(long, default_value = "127.0.0.1")]
        address: std::net::IpAddr,
        /// Ports to forward, each `<port>` or `<host>:<port>` (at least one)
        ports: Vec<String>,
    },
    /// Run the per-user host daemon in the foreground (docs/architecture/daemon/role.md)
    ///
    /// Plumbing: commands start it on demand, detached, and it exits after
    /// 10 minutes without clients. Exits at once (status 0) when one is
    /// already running. `serve install` starts it at login instead.
    #[cfg(unix)]
    #[command(args_conflicts_with_subcommands = true)]
    Serve {
        #[command(subcommand)]
        cmd: Option<ServeCommand>,
        /// Never exit for idleness
        #[arg(long)]
        keep_alive: bool,
        /// Socket dir (the lazy start passes the one its client resolved)
        #[arg(long, hide = true, value_name = "DIR")]
        socket_dir: Option<PathBuf>,
    },
    /// Relay the daemon's API (docs/reference/api.md) over stdin/stdout, for clients
    /// that can only spawn a process
    ///
    /// Starts the daemon if needed. The client sends the `hello` itself.
    /// Exits 0 when stdin closes, 75 when the daemon hangs up first.
    #[cfg(unix)]
    Api {
        /// Relay over stdin/stdout (the only transport)
        #[arg(long, required = true)]
        stdio: bool,
        /// Who's relaying (`api:<name>`, `npm:<name>`), for serve.log
        #[arg(long, default_value = "api")]
        client: String,
        /// Give relayed verb requests this process's env (a machine link's
        /// relay over ssh: docs/architecture/daemon/machines.md)
        #[arg(long, hide = true)]
        session_env: bool,
    },
    /// The far end of a remote forward's ssh (docs/architecture/daemon/forwards.md,
    /// *Remote bridges*): resolve the route here, write its header line to
    /// stdout, then become the instance's `devsbd bridge`
    #[cfg(unix)]
    #[command(hide = true)]
    Tunnel {
        /// Instance name, sandbox config name, or repository folder name
        #[arg(long, required_unless_present = "service")]
        instance: Option<String>,
        /// The service whose port to dial
        #[arg(long)]
        service: Option<String>,
        /// The container-side port
        #[arg(long)]
        port: u16,
    },
    /// The far end of a machine's agent relay (docs/architecture/daemon/machines.md,
    /// *Agent*): serve the linked laptop's ssh-agent on a socket here, carried
    /// over stdin/stdout
    #[cfg(unix)]
    #[command(hide = true)]
    AgentRelay {
        /// Who's linked (`machine:<hostname>`): names the socket
        #[arg(long)]
        client: String,
    },
    /// The hub's end of a fleet worker's link (docs/architecture/daemon/fleet.md,
    /// *Hub side*): offer the worker to this host's daemon and carry its API
    /// over stdin/stdout
    #[cfg(unix)]
    #[command(hide = true)]
    FleetLink {
        /// The worker's machine name: names the socket and its offered entry
        #[arg(long)]
        worker: String,
        /// The worker's canonical fleet root: its offered entry's one root
        #[arg(long)]
        root: PathBuf,
        /// The worker's lending is `drain`: it takes no new work
        #[arg(long)]
        drain: bool,
    },
    /// Register remote hosts (over ssh) whose sandboxes this one manages
    /// (docs/architecture/daemon/machines.md)
    #[cfg(unix)]
    Machine {
        #[command(subcommand)]
        cmd: MachineCommand,
    },
    /// Show or change whether this host lends itself to a fleet hub
    /// (docs/architecture/daemon/fleet.md); `fleet join` picks the hub
    #[cfg(unix)]
    Lend {
        #[command(subcommand)]
        cmd: Option<LendCommand>,
    },
    /// Join or leave a fleet: lend this host to a hub as a worker
    /// (docs/architecture/daemon/fleet.md)
    #[cfg(unix)]
    Fleet {
        #[command(subcommand)]
        cmd: FleetCommand,
    },
}

/// An instance `<name>`: one text for every command taking one.
const NAME_HELP: &str = "Instance name, sandbox config name, or repository folder name; \
     `<machine>/<name>` for a machine's, `@/<name>` for this host's (a bare name is looked up on every machine)";

/// `logs`/`inspect`'s `<name>`, which may also be a container.
const CONTAINER_NAME_HELP: &str = "Instance name, sandbox config name, repository folder name, or devsandbox \
     container name (service containers included); `<machine>/<name>` for a machine's, `@/<name>` for this \
     host's (a bare container name is this host's)";

/// `-d` on `run`, `rebuild`, `start`: one text, one behaviour.
const DETACH_HELP: &str = "Return once the instance is registered (its name is printed); the build \
     goes on in devsandbox serve. `devsandbox logs --build <name> --follow` follows it";

const REPROBE_HELP: &str = "Probe the runtime's user-namespace mode again (rootful, rootless, VM-backed) \
     with a throwaway container, instead of the cached result";

#[cfg(unix)]
#[derive(Subcommand)]
enum ServeCommand {
    /// Start the daemon at login, keep-alive: a systemd user unit (Linux) or
    /// a LaunchAgent (macOS). Run again to refresh its PATH and runtime env
    Install,
    /// Stop the daemon `serve install` manages and remove its unit
    Uninstall,
}

#[cfg(unix)]
#[derive(Subcommand)]
enum MachineCommand {
    /// Register a machine: an ssh target running devsandbox
    ///
    /// Adding a machine means trusting it: whoever controls it can act as
    /// you in its containers (your ssh agent reaches it only with --agent),
    /// and its devsandbox socket is as powerful as docker.sock.
    ///
    /// Without --config, its known config roots are offered to pick from
    /// (on a TTY; otherwise the only one is taken). --config is a path on
    /// the machine: quote a `~` (`--config '~/sandboxes'`), or your local
    /// shell expands it to your local home.
    ///
    /// When the machine has no devsandbox or another version, add offers to
    /// copy this one there (to ~/.local/bin/devsandbox when missing, in
    /// place when updating); off a TTY it fails unless --auto-install.
    ///
    ///     devsandbox machine add me@my-server
    ///
    ///     devsandbox machine add ssh://me@box:2200 --name box --config '~/sandboxes'
    Add {
        /// What ssh takes (`me@host`, an ssh-config alias) or `ssh://[user@]host[:port]`
        target: String,
        /// The machine's name (default: the target's host)
        #[arg(long)]
        name: Option<String>,
        /// A config root on the machine (repeatable)
        #[arg(long = "config", value_name = "PATH")]
        configs: Vec<String>,
        /// Relay this host's ssh-agent to the machine (off by default; see `machine set`)
        #[arg(long)]
        agent: bool,
        /// Install or update devsandbox on the machine without asking (copies this binary)
        #[arg(long)]
        auto_install: bool,
    },
    /// List the registered machines
    Ls {
        /// Output JSON instead of a table
        #[arg(long)]
        json: bool,
    },
    /// Change a registered machine's settings
    ///
    ///     devsandbox machine set my-server --agent on
    Set {
        name: String,
        /// Relay this host's ssh-agent to the machine
        #[arg(long, required = true, action = clap::ArgAction::Set, value_name = "on|off", value_parser = commands::machine::parse_on_off)]
        agent: bool,
    },
    /// Unregister a machine (nothing on it changes)
    Rm { name: String },
}

#[cfg(unix)]
#[derive(Subcommand, Debug, PartialEq, Eq)]
enum LendCommand {
    /// The lending state, the hub, the name offered and the link (the default)
    Show,
    /// Lend to the hub `fleet join` set: offer this host and take new work
    On,
    /// Stop lending: the link to the hub goes down
    Off,
    /// Keep the link (what's placed here stays reachable), take no new work
    Drain,
}

#[cfg(unix)]
#[derive(Subcommand, Debug, PartialEq, Eq)]
enum FleetCommand {
    /// Lend this host to a hub: its daemon dials `ssh <hub> devsandbox fleet-link`
    ///
    ///     devsandbox fleet join me@hub
    ///
    ///     devsandbox fleet join ssh://me@hub:2200 --name lap
    Join {
        /// What ssh takes (`me@host`, an ssh-config alias) or `ssh://[user@]host[:port]`
        hub: String,
        /// The name this host offers itself under (default: the hostname's first label)
        #[arg(long)]
        name: Option<String>,
    },
    /// Drain, wait until no instance the hub placed here runs, then stop lending
    /// (Ctrl-C stops the wait and leaves it draining)
    Leave,
}

/// `<thread>`: `<owner>/<key>` (owner: instance name or id) or the id `inbox ls` shows.
type ThreadArg = commands::inbox::ThreadArg;

#[derive(Subcommand)]
enum InboxCommand {
    /// List threads, last change first
    Ls {
        /// Which threads
        #[arg(long, default_value = "all", value_parser = ["needs-you", "active", "done", "all"])]
        view: String,
        /// Output JSON instead of a table
        #[arg(long)]
        json: bool,
    },
    /// Show a thread: header, then its feed newest first
    Show {
        /// `<owner>/<key>` or a thread id
        #[arg(value_parser = ThreadArg::parse)]
        thread: ThreadArg,
        /// Output JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Reply to a thread's owner
    Reply {
        /// `<owner>/<key>` or a thread id
        #[arg(value_parser = ThreadArg::parse)]
        thread: ThreadArg,
        /// The reply; `-` reads it from stdin
        text: String,
    },
    /// Press one of a thread's actions (by its id)
    Act {
        /// `<owner>/<key>` or a thread id
        #[arg(value_parser = ThreadArg::parse)]
        thread: ThreadArg,
        action: String,
    },
    /// Submit the form of one of a thread's messages
    Submit {
        /// `<owner>/<key>` or a thread id
        #[arg(value_parser = ThreadArg::parse)]
        thread: ThreadArg,
        /// The message holding the form
        message: String,
        /// Answers as a JSON object of question id -> answer (any subset:
        /// the draft and the defaults fill in the rest); read from stdin
        /// when absent
        #[arg(long, value_name = "ANSWERS")]
        json: Option<String>,
    },
    /// Mark a thread done
    Done {
        /// `<owner>/<key>` or a thread id
        #[arg(value_parser = ThreadArg::parse)]
        thread: ThreadArg,
    },
    /// Reopen a done thread
    Reopen {
        /// `<owner>/<key>` or a thread id
        #[arg(value_parser = ThreadArg::parse)]
        thread: ThreadArg,
    },
}

#[derive(Subcommand)]
enum ServiceCommand {
    /// Recreate a service's container(s) from the current config and rewire
    /// every running sandbox that references it (no sandbox restart)
    Rebuild {
        /// Service name from devsandboxes.toml
        name: String,
    },
    /// List the services defined in devsandboxes.toml
    Ls {
        /// Output JSON instead of a table
        #[arg(long)]
        json: bool,
    },
}

fn main() -> Result<()> {
    // First thing: the stamp must be this binary's, read before an upgrade
    // can replace the file.
    exe::own_build_stamp();
    let cli = Cli::parse();
    #[cfg(unix)]
    if cli.in_process {
        serve::client::set_in_process();
    }

    // Here rather than in `Config::load`: that also runs on daemon threads,
    // where a notice would land in `serve.log` with no one to read it. A failure leaves the legacy file read
    // in place, so it only warns.
    match config::migrate_legacy(&cli.dir) {
        Ok(Some(new)) => eprintln!("devsandbox: renamed config.toml to {}", new.display()),
        Ok(None) => {}
        Err(e) => eprintln!("devsandbox: warning: {e:#}"),
    }

    let Some(command) = cli.command else {
        if cli.machine.is_some() {
            anyhow::bail!("--machine isn't for the dashboard");
        }
        if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
            #[cfg(unix)]
            return tui::dashboard(&cli.dir);
            #[cfg(not(unix))]
            anyhow::bail!("the dashboard needs devsandbox serve (unix only for now)");
        }
        Cli::command().print_help()?;
        std::process::exit(2);
    };

    // The five verbs and `service rebuild` go to the daemon
    // (docs/architecture/daemon/verbs.md, docs/reference/api.md *Verbs*); it runs
    // them as `--in-process` subprocesses, which take the arms below. With
    // no daemon to reach, `fallback` says why and they run here instead.
    //
    // Before that, machines (docs/architecture/daemon/machines.md): a name may be a machine's,
    // and `--machine` runs a nameless command there. A command that ran on a
    // machine over ssh ends here; the rest come back with their name as this
    // host knows it. The registry is read from the file (no daemon round
    // trip), never under `--in-process`.
    #[cfg(unix)]
    let machines = if cli.in_process { Vec::new() } else { serve::machines::registered_names() };
    #[cfg(unix)]
    let command = match serve::machines::cli::route(cli.machine.as_deref(), cli.in_process, &machines, command)? {
        serve::machines::cli::Routed::Here(command) => command,
        serve::machines::cli::Routed::Exit(code) => std::process::exit(code),
        serve::machines::cli::Routed::Port { entry, root, name, service, address, ports } => {
            return commands::port::port_remote(entry, root, name, service, address, ports);
        }
    };
    #[cfg(not(unix))]
    if cli.machine.is_some() {
        anyhow::bail!("--machine needs devsandbox serve (unix only for now)");
    }
    #[cfg(unix)]
    let fed = serve::verbs_client::Fed { machines: !machines.is_empty(), machine: cli.machine.clone() };
    #[cfg(unix)]
    let (command, fallback) = match serve::verbs_client::from_command(&command).filter(|_| !cli.in_process) {
        Some(verb) => match serve::verbs_client::run(&cli.dir, verb, &fed)? {
            serve::verbs_client::Done::Exit(code) => std::process::exit(code),
            serve::verbs_client::Done::InProcess(why) => {
                // As the daemon's subprocesses do: a fallback never lazy
                // starts a daemon (nothing answered just now).
                serve::client::set_in_process();
                // `@/x` is `x` here; a machine's name refused.
                (serve::machines::cli::localize(command)?, Some(why))
            }
        },
        None => (command, None),
    };
    #[cfg(not(unix))]
    let fallback: Option<String> = None;
    let in_process = cli.in_process || fallback.is_some();
    let detach_refused = || detach_needs_daemon(cli.in_process, fallback.as_deref());

    match command {
        Command::Ls { json } => commands::ls::ls(&cli.dir, json),
        Command::Ps { all, json } => commands::ps::ps(all, json),
        Command::Run { sandbox, name, branch, base, env, dispatcher, json, reprobe, detach } => {
            if detach {
                return Err(detach_refused());
            }
            // Taken before any work so every child process (and autostart
            // below) writes to stderr; only the final document hits stdout.
            let json_out = json.then(json_stdout::JsonStdout::capture).transpose()?;
            // A CLI process: the build log may tee fds 1/2 (on top of
            // `--json`'s redirect). Autostart's runs below get their own.
            build_log::enable();
            let extras = commands::run::RunExtras { reprobe, env, dispatcher };
            let result = commands::run::run(&cli.dir, sandbox, name, branch, base, extras);
            if let (None, Ok(Some(key))) = (&json_out, &result) {
                println!("{key}");
            }
            // After, not before: `run web` for an autostart `web` must not
            // create two instances. Whatever `run` returned is kept.
            commands::autostart::autostart(&cli.dir);
            // After autostart, so the daemon's own pass finds this boot done.
            #[cfg(unix)]
            if matches!(result, Ok(Some(_))) && !in_process {
                serve::client::ensure_running("cli");
            }
            match (result?, json_out) {
                (Some(key), Some(out)) => out.emit(&commands::run::run_record_json(&key)?),
                // Only a written example config gets here: no instance to report.
                (None, Some(_)) => anyhow::bail!("no instance created"),
                (_, None) => Ok(()),
            }
        }
        Command::Rebuild { name, all, force, reprobe, detach } => {
            if detach {
                return Err(detach_refused());
            }
            build_log::enable();
            let result = commands::rebuild::rebuild(&cli.dir, name, all, force, reprobe);
            // Like `start`: the rebuilt container is up, and a dispatcher /
            // inbox instance needs a host even with no dashboard open.
            commands::autostart::autostart(&cli.dir);
            #[cfg(unix)]
            if result.is_ok() && !in_process {
                serve::client::ensure_running("cli");
            }
            result
        }
        Command::Rename { name, new_name } => commands::rename::rename(&name, &new_name),
        Command::Rm { name, delete_branch, keep_branch, force, fail_on_ask } => {
            let result = commands::rm::rm(&name, rm_branch_flag(delete_branch, keep_branch), force, fail_on_ask);
            match result {
                // Printed as `main`'s own `Err` would be (`serve::verbs`
                // parses that line), with its own exit code.
                Err(e) if e.is::<commands::rm::NeedsAnswer>() => {
                    eprintln!("Error: {e:?}");
                    std::process::exit(commands::rm::NEEDS_ANSWER_EXIT);
                }
                result => result,
            }
        }
        Command::Stop { name, all } => commands::stop::stop(name, all),
        Command::Start { name, all, detach } => {
            if detach {
                return Err(detach_refused());
            }
            build_log::enable();
            let result = commands::start::start(&cli.dir, name, all);
            commands::autostart::autostart(&cli.dir);
            #[cfg(unix)]
            if result.is_ok() && !in_process {
                serve::client::ensure_running("cli");
            }
            result
        }
        Command::Gc { force } => commands::services::gc(&cli.dir, force),
        Command::Service { cmd } => match cmd {
            ServiceCommand::Rebuild { name } => commands::services::rebuild(&cli.dir, &name),
            ServiceCommand::Ls { json } => commands::services::ls(&cli.dir, json),
        },
        Command::Inbox { cmd } => {
            use commands::inbox::{self, Mutation};
            match cmd {
                InboxCommand::Ls { view, json } => inbox::ls(&view, json),
                InboxCommand::Show { thread, json } => inbox::show(&thread, json),
                InboxCommand::Reply { thread, text } => inbox::mutate(&thread, Mutation::Reply(inbox::reply_text(&text)?)),
                InboxCommand::Act { thread, action } => inbox::mutate(&thread, Mutation::Act(action)),
                InboxCommand::Submit { thread, message, json } => {
                    let answers = inbox::submit_answers(json.as_deref())?;
                    inbox::mutate(&thread, Mutation::Submit { message, answers })
                }
                InboxCommand::Done { thread } => inbox::mutate(&thread, Mutation::Done),
                InboxCommand::Reopen { thread } => inbox::mutate(&thread, Mutation::Reopen),
            }
        }
        Command::Logs { name, lines, build, follow } => {
            if build {
                commands::logs::build_log(&name, follow)
            } else {
                commands::logs::logs(&name, lines)
            }
        }
        Command::Inspect { name, json } => commands::inspect::inspect(&name, json),
        Command::Vscode { name, goto } => commands::vscode::vscode(&cli.dir, &name, goto.as_ref()),
        Command::Done { name } => commands::done::done(&name, true),
        Command::Undone { name } => commands::done::done(&name, false),
        Command::Stats { json } => commands::stats::stats(json),
        Command::Status { json: _ } => commands::status::status(&cli.dir),
        Command::Exec { interactive, tty, attach, sessions, session, reserve_255, name, command } => {
            use commands::exec::Exec;
            let what = match (&attach, sessions) {
                (Some(id), _) => Exec::Attach(id.as_deref()),
                (None, true) => Exec::Sessions,
                (None, false) => Exec::Run { interactive, tty, command: &command, session: session.as_deref() },
            };
            commands::exec::exec(&name, what, reserve_255)
        }
        Command::Port { name, service, address, ports } => {
            #[cfg(unix)]
            {
                commands::port::port(cli.dir, name, service, address, ports)
            }
            #[cfg(not(unix))]
            {
                let _ = (name, service, address, ports);
                anyhow::bail!("port forwarding is unix-only for now")
            }
        }
        #[cfg(unix)]
        Command::Serve { cmd, keep_alive, socket_dir } => match cmd {
            None => serve::daemon::serve(socket_dir, keep_alive),
            Some(ServeCommand::Install) => serve::install::install_cli(),
            Some(ServeCommand::Uninstall) => serve::install::uninstall_cli(),
        },
        #[cfg(unix)]
        Command::Api { stdio: _, client, session_env } => {
            std::process::exit(serve::relay::stdio(&client, session_env)?)
        }
        #[cfg(unix)]
        Command::Tunnel { instance, service, port } => commands::tunnel::tunnel(cli.dir, instance, service, port),
        #[cfg(unix)]
        Command::AgentRelay { client } => commands::agent_relay::agent_relay(&client),
        #[cfg(unix)]
        Command::FleetLink { worker, root, drain } => commands::fleet_link::fleet_link(&worker, &root, drain),
        #[cfg(unix)]
        Command::Machine { cmd } => match cmd {
            MachineCommand::Add { target, name, configs, agent, auto_install } => {
                commands::machine::add(&target, name.as_deref(), &configs, agent, auto_install)
            }
            MachineCommand::Ls { json } => commands::machine::ls(json),
            MachineCommand::Set { name, agent } => commands::machine::set(&name, agent),
            MachineCommand::Rm { name } => commands::machine::rm(&name),
        },
        #[cfg(unix)]
        Command::Lend { cmd } => {
            use serve::fleet::lend::State;
            match cmd.unwrap_or(LendCommand::Show) {
                LendCommand::Show => commands::lend::show(),
                LendCommand::On => commands::lend::set_state(State::On),
                LendCommand::Off => commands::lend::set_state(State::Off),
                LendCommand::Drain => commands::lend::set_state(State::Drain),
            }
        }
        #[cfg(unix)]
        Command::Fleet { cmd } => match cmd {
            FleetCommand::Join { hub, name } => commands::lend::join(&hub, name.as_deref()),
            FleetCommand::Leave => commands::lend::leave(),
        },
    }
}

/// Why an in-process verb can't take `-d`: nothing would carry the build on
/// once this process returns. `fallback` is why the daemon wasn't reached.
fn detach_needs_daemon(in_process_flag: bool, fallback: Option<&str>) -> anyhow::Error {
    match fallback {
        Some(why) => anyhow::anyhow!("`-d` needs devsandbox serve: {why}"),
        None if cfg!(unix) && in_process_flag => anyhow::anyhow!("`-d` needs devsandbox serve, not --in-process"),
        None if cfg!(unix) => anyhow::anyhow!("`-d` needs devsandbox serve"),
        None => anyhow::anyhow!("`-d` needs devsandbox serve, which is unix-only for now"),
    }
}

/// `rm`'s branch flags as the answer `commands::rm::rm` takes: `None` (no
/// flag) keeps the prompt. clap makes the two mutually exclusive.
fn rm_branch_flag(delete_branch: bool, keep_branch: bool) -> Option<bool> {
    match (delete_branch, keep_branch) {
        (true, _) => Some(true),
        (_, true) => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{rm_branch_flag, Cli, Command};
    #[cfg(unix)]
    use super::{FleetCommand, LendCommand};
    #[cfg(unix)]
    use std::path::Path;

    fn rm_flag(args: &[&str]) -> Result<Option<bool>, clap::Error> {
        let cli = Cli::try_parse_from(["devsandbox", "rm", "web"].iter().chain(args))?;
        match cli.command {
            Some(Command::Rm { delete_branch, keep_branch, .. }) => {
                Ok(rm_branch_flag(delete_branch, keep_branch))
            }
            _ => panic!("not rm"),
        }
    }

    #[test]
    fn rm_branch_flags_parse_and_exclude_each_other() {
        assert_eq!(rm_flag(&[]).unwrap(), None);
        assert_eq!(rm_flag(&["--delete-branch"]).unwrap(), Some(true));
        assert_eq!(rm_flag(&["--keep-branch"]).unwrap(), Some(false));
        let err = rm_flag(&["--delete-branch", "--keep-branch"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[cfg(unix)]
    #[test]
    fn lend_and_fleet_parse() {
        let parse = |args: &[&str]| Cli::try_parse_from(std::iter::once("devsandbox").chain(args.iter().copied()));
        let lend = |args: &[&str]| match parse(args).unwrap().command {
            Some(Command::Lend { cmd }) => cmd,
            _ => panic!("not lend"),
        };
        assert_eq!(lend(&["lend"]), None, "show by default");
        assert_eq!(lend(&["lend", "show"]), Some(LendCommand::Show));
        assert_eq!(lend(&["lend", "on"]), Some(LendCommand::On));
        assert_eq!(lend(&["lend", "off"]), Some(LendCommand::Off));
        assert_eq!(lend(&["lend", "drain"]), Some(LendCommand::Drain));
        assert!(parse(&["lend", "maybe"]).is_err());
        let fleet = |args: &[&str]| match parse(args).unwrap().command {
            Some(Command::Fleet { cmd }) => cmd,
            _ => panic!("not fleet"),
        };
        assert_eq!(fleet(&["fleet", "join", "me@hub"]), FleetCommand::Join { hub: "me@hub".into(), name: None });
        assert_eq!(
            fleet(&["fleet", "join", "ssh://me@hub:2200", "--name", "lap"]),
            FleetCommand::Join { hub: "ssh://me@hub:2200".into(), name: Some("lap".into()) }
        );
        assert_eq!(fleet(&["fleet", "leave"]), FleetCommand::Leave);
        assert!(parse(&["fleet", "join"]).is_err(), "a hub is required");
        assert!(parse(&["fleet"]).is_err());
        match parse(&["fleet-link", "--worker", "lap", "--root", "/d/fleet/hub", "--drain"]).unwrap().command {
            Some(Command::FleetLink { worker, root, drain }) => assert!(worker == "lap" && root == Path::new("/d/fleet/hub") && drain),
            _ => panic!("not fleet-link"),
        }
        assert!(parse(&["fleet-link", "--worker", "lap"]).is_err(), "a root is required");
    }

    /// `exec`'s (attach, sessions, session, name, command).
    type ExecArgs = (Option<Option<String>>, bool, Option<String>, String, Vec<String>);

    fn exec_args(args: &[&str]) -> Result<ExecArgs, clap::Error> {
        let cli = Cli::try_parse_from(["devsandbox", "exec"].iter().chain(args))?;
        match cli.command {
            Some(Command::Exec { attach, sessions, session, name, command, .. }) => {
                Ok((attach, sessions, session, name, command))
            }
            _ => panic!("not exec"),
        }
    }

    #[test]
    fn exec_session_flags_parse() {
        let s = |v: &str| v.to_string();
        let id = "1790000000-a1b2";
        assert_eq!(exec_args(&["web", "--attach"]).unwrap(), (Some(None), false, None, s("web"), vec![]));
        assert_eq!(exec_args(&["web", "--attach", id]).unwrap(), (Some(Some(s(id))), false, None, s("web"), vec![]));
        assert_eq!(exec_args(&["--attach=1790000000-a1b2", "web"]).unwrap().0, Some(Some(s(id))));
        assert_eq!(exec_args(&["web", "--sessions"]).unwrap(), (None, true, None, s("web"), vec![]));
        assert_eq!(exec_args(&["--sessions", "web"]).unwrap().1, true);
        assert_eq!(
            exec_args(&["--session", id, "web", "--", "zidane"]).unwrap(),
            (None, false, Some(s(id)), s("web"), vec![s("zidane")])
        );
        assert_eq!(exec_args(&["web", "--session", id]).unwrap().2, Some(s(id)));
        // After the command starts, the flags are the command's.
        assert_eq!(exec_args(&["web", "--", "cmd", "--attach"]).unwrap(), (None, false, None, s("web"), vec![s("cmd"), s("--attach")]));
        assert_eq!(exec_args(&["web", "ls", "--sessions"]).unwrap().4, vec![s("ls"), s("--sessions")]);
        for conflict in [
            &["web", "--attach", "--", "ls"][..],
            &["web", "--sessions", "--", "ls"],
            &["web", "--attach", "--sessions"],
            &["--session", id, "web", "--attach"],
            &["--session", id, "web", "--sessions"],
            &["-t", "web", "--attach"],
        ] {
            let err = exec_args(conflict).unwrap_err();
            assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict, "{conflict:?}");
        }
    }
}
