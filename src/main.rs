mod broker;
mod build_log;
mod commands;
mod config;
mod config_identity;
mod credentials;
mod daemon_config;
mod devsbd;
mod exe;
mod features;
mod inbox;
mod json_stdout;
mod key_guard;
#[cfg(unix)]
mod open;
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

/// The tail of a refusal for a feature not built yet: `<feature>: {NOT_SUPPORTED_YET}`.
pub const NOT_SUPPORTED_YET: &str = "not supported yet! open an issue in https://github.com/Nemikolh/devsandboxes";

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
        /// Profile for this instance (one of the sandbox's `profiles`);
        /// default: the last one chosen for this sandbox, else its first
        #[arg(long)]
        profile: Option<String>,
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
        /// Ask the fleet's control plane where to run this host's sandbox (not with
        /// `--machine` or a `<machine>/<sandbox>`)
        #[arg(long)]
        place: bool,
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
    /// Trust the origin URL a repo's config holds now: host git fetches only
    /// the URL pinned on its first fetch, so a sandbox can't redirect it
    TrustOrigin {
        /// The repo's checkout (a folder with a `.git`)
        folder: PathBuf,
    },
    /// Show or switch an instance's profile (one of its sandbox's `profiles`)
    Profile {
        /// Exact instance name (no sandbox/folder resolution);
        /// `<machine>/<name>` for a machine's, `@/<name>` for this host's
        name: String,
        /// The profile to switch to (without it: show the current one and
        /// what its sandbox allows)
        profile: Option<String>,
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
        /// Open an instance with credential claims anyway: VS Code's attach
        /// gives it your host's git logins and whole ssh-agent
        #[arg(long)]
        allow_credential_leak: bool,
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
    /// Fetch an instance this host placed on a machine back into the local
    /// checkout it came from, as hidden refs
    ///
    /// Its branch and a snapshot of its working tree land as
    /// `refs/devsandbox/<machine>/<instance>/heads/<branch>` and
    /// `…/wip`; your branches, index and working tree are left alone
    /// (docs/architecture/daemon/fleet.md, *Fetch-back*). Needs devsandbox serve.
    Fetch {
        /// `<machine>/<instance>` (a bare `<instance>` when only one
        /// placement has that name)
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
    ///     devsandbox exec web --end 1790000000-a1b2
    ///     devsandbox exec web --service db
    Exec {
        /// Keep stdin open
        #[arg(short)]
        interactive: bool,
        /// Allocate a pseudo-TTY
        #[arg(short)]
        tty: bool,
        /// Attach to session ID; without ID, to the instance's one detached
        /// session, else its one running session, taking it over (several:
        /// a pick list on a TTY). Give ID as `--attach=ID` before the name,
        /// or after it
        #[arg(long, value_name = "ID", num_args = 0..=1, conflicts_with_all = ["command", "sessions", "session", "interactive", "tty", "end"])]
        attach: Option<Option<String>>,
        /// List the instance's sessions: id, state, attached or detached,
        /// started (UTC), command
        #[arg(long, conflicts_with_all = ["command", "session", "interactive", "tty", "end"])]
        sessions: bool,
        /// End session ID: SIGHUP to its program (SIGKILL if still there
        /// after 2 s), then the session is gone. Sessions have no idle
        /// timeout: they end this way, when their program exits, or with the
        /// container
        #[arg(long, value_name = "ID", conflicts_with_all = ["command", "session", "interactive", "tty"])]
        end: Option<String>,
        /// Run in session ID, attaching if it is live (even without a TTY);
        /// without the helper, a plain exec
        #[arg(long, value_name = "ID")]
        session: Option<String>,
        /// Open a terminal in service NAME's container, the one this instance
        /// uses (the command, else a login shell; always -i -t, never a
        /// session); needs a TTY
        #[arg(long, value_name = "NAME", conflicts_with_all = ["attach", "sessions", "session", "end"])]
        service: Option<String>,
        /// Report a command's exit code 255 as 254, so 255 only ever means
        /// ssh's own failure: what the reconnect loops over ssh pass
        /// (`commands::exec::reported_code`)
        #[arg(long, hide = true)]
        reserve_255: bool,
        #[arg(help = NAME_HELP)]
        name: String,
        /// Command and arguments to run; omitted: a login shell (zsh, bash,
        /// or sh), with `-i`, plus `-t` when stdin is a TTY. Given from a
        /// terminal (stdin and stdout), it gets `-i -t` too. Either flag
        /// overrides both defaults
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
        /// Write the sentinel line before the daemon's bytes (`machine add`'s
        /// probe over ssh: docs/architecture/daemon/machines.md, *ssh*)
        #[arg(long, hide = true)]
        sentinel: bool,
    },
    /// The dialed end of a fleet link over stdin/stdout, a link a machine
    /// dialed to this one (docs/architecture/daemon/fleet.md, *Peer links*)
    #[cfg(unix)]
    #[command(hide = true)]
    FleetLink {
        /// The dialer's machine name: the link's inbound neighbor here,
        /// whose streams this host's trust table answers
        #[arg(long)]
        peer: String,
    },
    /// Print the build this binary says in a fleet link's `Hello`
    /// (docs/architecture/daemon/fleet.md, *Versions*): what a member
    /// checks a received pinned binary by
    #[cfg(unix)]
    #[command(hide = true)]
    FleetHello,
    /// Register remote hosts (over ssh) whose sandboxes this one manages
    /// (docs/architecture/daemon/machines.md)
    #[cfg(unix)]
    Machine {
        #[command(subcommand)]
        cmd: MachineCommand,
    },
    /// Store a credential's secret on this host (its home) and approve the
    /// claim it is for (docs/reference/credentials-cli.md)
    ///
    /// By the claim's login methods: the device flow or a browser login
    /// (OAuth), or paste from stdin, unechoed on a terminal or piped
    /// (`login sentry-work < token`).
    #[cfg(unix)]
    Login {
        /// The credential's name in credentials.toml (`sentry-work`)
        credential: String,
        /// The feature whose claim to approve: an OCI ref, or a local folder
        /// (`./…`, `../…`, `/…`, from the current dir). Without it: the claim
        /// version pending for the credential, else the secret only
        #[arg(long, value_name = "REF")]
        feature: Option<String>,
        /// The claim's kind, when the feature claims several
        #[arg(long)]
        kind: Option<String>,
        /// A profile a new credential is granted to (repeatable; none: every
        /// profile)
        #[arg(long = "profile", value_name = "PROFILE")]
        profiles: Vec<String>,
        /// Approve the claim, and with --broad a broad credential, without
        /// asking (required off a terminal)
        #[arg(long)]
        yes: bool,
        /// A new broad credential: it takes the claim's broad tokens (gh's
        /// own `gho_…`), which reach every repo your account can access, by
        /// the claim's broad logins or paste
        #[arg(long)]
        broad: bool,
        /// The login method, one the claim lists (default: the first listed
        /// of device, browser, paste on a terminal; paste, device, browser
        /// off one; browser only where a browser opens)
        #[arg(long, value_parser = ["paste", "device", "browser"])]
        method: Option<String>,
        /// Log in to this OAuth app instead of the claim's (an app you own)
        #[arg(long, value_name = "ID")]
        client_id: Option<String>,
        /// The user or org a pasted token's creation page prefills as its
        /// resource owner
        #[arg(long, value_name = "USER|ORG")]
        owner: Option<String>,
    },
    /// This host's credentials: list them, approve claim versions, revoke
    /// (docs/reference/credentials-cli.md)
    #[cfg(unix)]
    Access {
        #[command(subcommand)]
        cmd: AccessCommand,
    },
}

#[cfg(unix)]
#[derive(Subcommand)]
enum AccessCommand {
    /// List credentials: grant, token kind and expiry (never the value),
    /// claim versions, and the instances here that take each
    Ls {
        /// Only this credential
        credential: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Delete a credential's secret and entry, or drop one claim version
    /// of it
    Revoke {
        /// The credential's name in credentials.toml
        credential: String,
        /// Drop only this approved or pending version (a prefix of it)
        #[arg(long)]
        version: Option<String>,
        /// Revoke without asking (required off a terminal)
        #[arg(long)]
        yes: bool,
    },
    /// Approve the claim version pending for a credential, after its diff
    /// against the latest approved one; the secret stays
    Approve {
        /// The credential's name in credentials.toml
        credential: String,
        /// The pending version (a prefix of it), when there are several
        #[arg(long)]
        version: Option<String>,
        /// Approve without asking (required off a terminal)
        #[arg(long)]
        yes: bool,
    },
    /// Drop a repo from a folder's recorded GitHub fence (narrow only);
    /// its instances take it at their next start or rebuild
    Forget {
        /// The folder (a sandbox's `folder` or `folders` entry)
        folder: PathBuf,
        /// `owner/repo`, or its GitHub URL
        repo: String,
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
    /// Its config roots are the ones its own daemon lists (its configs, and
    /// those `devsandbox machine roots add` there names); add picks none.
    ///
    /// When the machine has no devsandbox or another version, add offers to
    /// copy this one there (to ~/.local/bin/devsandbox when missing, in
    /// place when updating); off a TTY it fails unless --auto-install.
    ///
    /// The machine's key fingerprint is shown to confirm (compare it with
    /// `devsandbox machine key` run there); off a TTY it fails unless --yes.
    ///
    /// Both ends end up in one fleet: the machine joins this host's, this
    /// host joins the machine's, or (neither in one) a fleet of two with
    /// this host the control plane. Two different fleets are refused.
    ///
    ///     devsandbox machine add me@my-server
    ///
    ///     devsandbox machine add ssh://me@box:2200 --name box
    Add {
        /// What ssh takes (`me@host`, an ssh-config alias) or `ssh://[user@]host[:port]`
        target: String,
        /// The machine's name (default: its member name; renames it only while it joins a fleet)
        #[arg(long)]
        name: Option<String>,
        /// Grant this host's ssh-agent to the machine, by its member key (off by default; see `machine set`)
        #[arg(long)]
        agent: bool,
        /// Make the machine the control plane of the new fleet (neither end in a fleet yet; in one, only if it already is the control plane)
        #[arg(long)]
        control_plane: bool,
        /// Install or update devsandbox on the machine without asking (copies this binary)
        #[arg(long)]
        auto_install: bool,
        /// Take the machine's key without asking to confirm its fingerprint
        #[arg(long)]
        yes: bool,
        /// Let work the fleet places on the machine use this (repeatable or comma-separated: privileged, runtime-socket, host-network, global-services, dispatcher, inbox, initialize-command, publish, gpus); any --allow* skips the question
        #[arg(long, value_name = "ALLOWANCE", value_delimiter = ',', value_parser = commands::machine::parse_allowance)]
        allow: Vec<&'static str>,
        /// Let binds and build contexts use this path on the machine (absolute)
        #[arg(long, value_name = "PATH")]
        allow_bind: Vec<String>,
        /// Let a config read this env var of the machine's with `${localEnv:NAME}`
        #[arg(long, value_name = "NAME")]
        allow_local_env: Vec<String>,
    },
    /// List the registered machines
    Ls {
        /// Output JSON instead of a table
        #[arg(long)]
        json: bool,
    },
    /// Change a machine's settings (registered, inbound or routed)
    ///
    ///     devsandbox machine set my-server --agent on
    Set {
        name: String,
        /// Grant (on) or withdraw (off) this host's ssh-agent for the machine, by its member key
        #[arg(long, required = true, action = clap::ArgAction::Set, value_name = "on|off", value_parser = commands::machine::parse_on_off)]
        agent: bool,
    },
    /// Copy this devsandbox over a registered machine's, when its build differs
    ///
    /// Replaces the devsandbox the machine's `command` names (ssh, as at
    /// `machine add`; on the same platform only), then the daemon there
    /// hands off to the copy and this host's link to the machine redials
    /// onto it. Asks first on a TTY; off one it fails unless --yes.
    ///
    ///     devsandbox machine upgrade my-server
    Upgrade {
        name: String,
        /// Replace it without asking
        #[arg(long)]
        yes: bool,
    },
    /// Drop this host's link to a registered machine (it stays a fleet member; nothing on it changes)
    Unlink { name: String },
    /// Take a machine out of the fleet; this host's own name leaves it
    ///
    /// On the fleet's authority it revokes the machine's key, sends the
    /// change to the machines it links to, then the control plane removes it: the
    /// machine clears its control plane and drops its links, every other member the
    /// control plane reaches forgets it. On another member it's proposed to the
    /// authority (linked directly), approved there with `machine approve`.
    /// Each outcome is printed; an unreachable machine or member is
    /// reported, not an error.
    ///
    ///     devsandbox machine rm my-server
    Rm { name: String },
    /// Make a member of the fleet its control plane
    ///
    /// The role moves, links stay. On the fleet's authority it's signed at
    /// once and sent to the machines it links to; each then redials saying
    /// the new control plane, and a member not reached learns it on its next link.
    /// On another member it's proposed to the authority, approved there
    /// with `machine approve`. The member needn't be reachable, so a dead
    /// control plane can be replaced.
    ///
    ///     devsandbox machine control-plane my-server
    ControlPlane { name: String },
    /// List the membership changes proposed to this host, the fleet's
    /// authority, waiting for `machine approve` or `machine deny`
    ///
    ///     devsandbox machine pending
    Pending,
    /// Sign a proposed membership change here (the authority) and run it
    ///
    ///     devsandbox machine approve 3
    Approve { id: u64 },
    /// Drop a proposed membership change, signing nothing
    ///
    ///     devsandbox machine deny 3
    Deny { id: u64 },
    /// Show or set what other machines reach on this host (its fleet trust)
    ///
    /// `worker` (the default): this host's own roots and its fleet root,
    /// placed work under the worker policy. `full`: its whole API, as a
    /// local client has it (any root, any path, no worker policy, the control plane's
    /// dev builds taken). Only ever set here, on this host.
    ///
    ///     devsandbox machine trust full
    Trust {
        #[arg(value_name = "worker|full", value_parser = commands::machine::parse_trust)]
        trust: Option<serve::fleet::member::Trust>,
    },
    /// Show or change the config roots this host shows the fleet besides
    /// those it has instances in (its picked roots)
    ///
    /// Under `worker` trust, other machines reach only this host's own
    /// roots: those its instances are in, plus these. Without a subcommand,
    /// lists the picked roots.
    ///
    ///     devsandbox machine roots add ~/sandboxes
    Roots {
        #[command(subcommand)]
        cmd: Option<RootsCommand>,
    },
    /// Show or change what work other machines place here may ask of this
    /// host (its worker policy's allowances, `[allow]` in member.toml)
    ///
    /// Every allowance is off unless set; none applies under `full` trust.
    /// Only ever set here, on this host: no other machine sets them.
    /// Without flags, lists them.
    ///
    ///     devsandbox machine allow --gpus on --bind-add ~/datasets
    Allow {
        #[command(flatten)]
        allow: AllowArgs,
    },
    /// Show this host's member key and its fingerprint, or rotate it
    ///
    /// Prints `<name> ed25519:<hex>`, then the fingerprint to compare with
    /// what another machine shows for this one, and a pending rotation's
    /// next key. The key (`member.key`, next to `member.toml`) is made on
    /// first use.
    ///
    ///     devsandbox machine key
    Key {
        #[command(subcommand)]
        cmd: Option<KeyCommand>,
    },
}

#[cfg(unix)]
#[derive(Subcommand)]
enum KeyCommand {
    /// Replace this host's member key with a new one, countersigned by it
    ///
    /// On the fleet's authority the change is signed at once and sent to
    /// the machines it links to; this host then signs and proves with the
    /// new key. On another member it's proposed to the authority (linked
    /// directly), approved there with `machine approve`; this host keeps
    /// its key until the signed change reaches it. Running it again while
    /// one is pending proposes the same new key again.
    ///
    ///     devsandbox machine key rotate
    Rotate,
}

#[cfg(unix)]
#[derive(clap::Args)]
struct AllowArgs {
    /// Run privileged containers
    #[arg(long, value_name = "on|off", value_parser = commands::machine::parse_on_off)]
    privileged: Option<bool>,
    /// Bind a container runtime's socket
    #[arg(long, value_name = "on|off", value_parser = commands::machine::parse_on_off)]
    runtime_socket: Option<bool>,
    /// Use the host's network (`--network host`, run or build)
    #[arg(long, value_name = "on|off", value_parser = commands::machine::parse_on_off)]
    host_network: Option<bool>,
    /// Run `scope = "global"` services
    #[arg(long, value_name = "on|off", value_parser = commands::machine::parse_on_off)]
    global_services: Option<bool>,
    /// Use a `dispatcher` (children created on this host)
    #[arg(long, value_name = "on|off", value_parser = commands::machine::parse_on_off)]
    dispatcher: Option<bool>,
    /// Open Inbox threads on this host (`inbox = true`)
    #[arg(long, value_name = "on|off", value_parser = commands::machine::parse_on_off)]
    inbox: Option<bool>,
    /// Run `initializeCommand` (on this host, outside any container)
    #[arg(long, value_name = "on|off", value_parser = commands::machine::parse_on_off)]
    initialize_command: Option<bool>,
    /// Publish ports beyond loopback
    #[arg(long, value_name = "on|off", value_parser = commands::machine::parse_on_off)]
    publish: Option<bool>,
    /// Use this host's NVIDIA GPUs
    #[arg(long, value_name = "on|off", value_parser = commands::machine::parse_on_off)]
    gpus: Option<bool>,
    /// Let binds and build contexts use this host path (`~` is this host's home, relative paths are against the current directory)
    #[arg(long, value_name = "PATH")]
    bind_add: Vec<String>,
    /// Drop an allowed host path
    #[arg(long, value_name = "PATH")]
    bind_rm: Vec<String>,
    /// Let a config read this host's env var with `${localEnv:NAME}`
    #[arg(long, value_name = "NAME")]
    local_env_add: Vec<String>,
    /// Drop an allowed env var
    #[arg(long, value_name = "NAME")]
    local_env_rm: Vec<String>,
}

#[cfg(unix)]
impl AllowArgs {
    fn change(self) -> commands::machine::AllowChange {
        let flags = [
            ("privileged", self.privileged),
            ("runtime_socket", self.runtime_socket),
            ("host_network", self.host_network),
            ("global_services", self.global_services),
            ("dispatcher", self.dispatcher),
            ("inbox", self.inbox),
            ("initialize_command", self.initialize_command),
            ("publish", self.publish),
            ("gpus", self.gpus),
        ];
        commands::machine::AllowChange {
            flags: flags.into_iter().filter_map(|(key, on)| on.map(|on| (key, on))).collect(),
            bind_add: self.bind_add,
            bind_rm: self.bind_rm,
            local_env_add: self.local_env_add,
            local_env_rm: self.local_env_rm,
        }
    }
}

#[cfg(unix)]
#[derive(Subcommand)]
enum RootsCommand {
    /// Pick roots: existing directories (`~` is this host's home, relative
    /// paths are against the current directory)
    Add {
        #[arg(required = true, value_name = "PATH")]
        paths: Vec<String>,
    },
    /// Drop picked roots (a directory that no longer exists too)
    Rm {
        #[arg(required = true, value_name = "PATH")]
        paths: Vec<String>,
    },
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

    // The five verbs and `service rebuild` (and `--machine m gc`) go to
    // the daemon
    // (docs/architecture/daemon/verbs.md, docs/reference/api.md *Verbs*); it runs
    // them as `--in-process` subprocesses, which take the arms below. With
    // no daemon to reach, `fallback` says why and they run here instead.
    //
    // Before that, machines (docs/architecture/daemon/machines.md): a name may be a machine's,
    // and `--machine` runs a nameless command there. A command that ran on a
    // machine over ssh ends here; the rest come back with their name as this
    // host knows it. The registry is read from the file (no daemon round
    // trip unless an inbound machine's link socket is there to ask about:
    // `machine_names`), never under `--in-process`.
    #[cfg(unix)]
    let machines = if cli.in_process { Vec::new() } else { serve::machines::cli::machine_names() };
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
    let (command, fallback) = match serve::verbs_client::from_command(&command, cli.machine.as_deref()).filter(|_| !cli.in_process) {
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
        // Placement is the daemon's: here, `--place` can only say it isn't.
        Command::Run { sandbox, name, branch, base, profile, env, dispatcher, json, reprobe, detach, place } => {
            if detach {
                return Err(detach_refused());
            }
            if place {
                eprintln!("note: --place needs devsandbox serve to ask the control plane: running here");
            }
            // Taken before any work so every child process (and autostart
            // below) writes to stderr; only the final document hits stdout.
            let json_out = json.then(json_stdout::JsonStdout::capture).transpose()?;
            // A CLI process: the build log may tee fds 1/2 (on top of
            // `--json`'s redirect). Autostart's runs below get their own.
            build_log::enable();
            let extras = commands::run::RunExtras { reprobe, env, dispatcher, profile };
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
        Command::TrustOrigin { folder } => commands::trust_origin::trust_origin(&folder),
        Command::Profile { name, profile } => commands::profile::profile(&name, profile.as_deref()),
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
        Command::Vscode { name, goto, allow_credential_leak } => {
            commands::vscode::vscode(&cli.dir, &name, goto.as_ref(), allow_credential_leak)
        }
        Command::Done { name } => commands::done::done(&name, true),
        Command::Undone { name } => commands::done::done(&name, false),
        Command::Fetch { name } => {
            #[cfg(unix)]
            {
                commands::fetch::fetch(&name)
            }
            #[cfg(not(unix))]
            {
                let _ = name;
                anyhow::bail!("`devsandbox fetch` needs devsandbox serve, which is unix-only for now")
            }
        }
        Command::Stats { json } => commands::stats::stats(json),
        Command::Status { json: _ } => commands::status::status(&cli.dir),
        Command::Exec { interactive, tty, attach, sessions, end, session, service, reserve_255, name, command } => {
            use commands::exec::Exec;
            let what = match (&attach, sessions, &end, &service) {
                (_, _, _, Some(service)) => Exec::Service { service, command: &command },
                (Some(id), _, _, _) => Exec::Attach(id.as_deref()),
                (None, true, _, _) => Exec::Sessions,
                (None, false, Some(id), None) => Exec::End(id),
                (None, false, None, None) => Exec::Run { interactive, tty, command: &command, session: session.as_deref() },
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
        Command::Api { stdio: _, client, sentinel } => std::process::exit(serve::relay::stdio(&client, sentinel)?),
        #[cfg(unix)]
        Command::FleetLink { peer } => commands::fleet_link::fleet_link_peer(&peer),
        #[cfg(unix)]
        Command::FleetHello => {
            println!("{}", serve::fleet::update::HELLO);
            Ok(())
        }
        #[cfg(unix)]
        Command::Machine { cmd } => match cmd {
            MachineCommand::Add { target, name, agent, control_plane, auto_install, yes, allow, allow_bind, allow_local_env } => {
                let allow = commands::machine::AddAllow { flags: allow, binds: allow_bind, local_env: allow_local_env };
                commands::machine::add(&target, name.as_deref(), agent, control_plane, auto_install, yes, &allow)
            }
            MachineCommand::Ls { json } => commands::machine::ls(json),
            MachineCommand::Set { name, agent } => commands::machine::set(&name, agent),
            MachineCommand::Upgrade { name, yes } => commands::machine::upgrade(&name, yes),
            MachineCommand::Unlink { name } => commands::machine::unlink(&name),
            MachineCommand::Rm { name } => commands::machine::rm(&name),
            MachineCommand::ControlPlane { name } => commands::machine::control_plane(&name),
            MachineCommand::Pending => commands::machine::pending(),
            MachineCommand::Approve { id } => commands::machine::approve(id),
            MachineCommand::Deny { id } => commands::machine::deny(id),
            MachineCommand::Trust { trust } => commands::machine::trust(trust),
            MachineCommand::Key { cmd: None } => commands::machine::key(),
            MachineCommand::Key { cmd: Some(KeyCommand::Rotate) } => commands::machine::rotate_key(),
            MachineCommand::Roots { cmd } => commands::machine::roots(match cmd {
                None => commands::machine::RootsOp::Show,
                Some(RootsCommand::Add { paths }) => commands::machine::RootsOp::Add(paths),
                Some(RootsCommand::Rm { paths }) => commands::machine::RootsOp::Rm(paths),
            }),
            MachineCommand::Allow { allow } => commands::machine::allow(allow.change()),
        },
        #[cfg(unix)]
        Command::Login { credential, feature, kind, profiles, yes, method, client_id, owner, broad } => {
            commands::login::login(commands::login::Args { credential, feature, kind, profiles, yes, method, client_id, owner, broad })
        }
        #[cfg(unix)]
        Command::Access { cmd: AccessCommand::Approve { credential, version, yes } } => {
            commands::access::approve(&credential, version.as_deref(), yes)
        }
        #[cfg(unix)]
        Command::Access { cmd: AccessCommand::Ls { credential, json } } => {
            commands::access::ls(credential.as_deref(), json)
        }
        #[cfg(unix)]
        Command::Access { cmd: AccessCommand::Forget { folder, repo } } => commands::access::forget(&folder, &repo),
        #[cfg(unix)]
        Command::Access { cmd: AccessCommand::Revoke { credential, version, yes } } => {
            commands::access::revoke(&credential, version.as_deref(), yes)
        }
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
    fn fleet_link_parses() {
        let parse = |args: &[&str]| Cli::try_parse_from(std::iter::once("devsandbox").chain(args.iter().copied()));
        match parse(&["fleet-link", "--peer", "lap"]).unwrap().command {
            Some(Command::FleetLink { peer }) => assert_eq!(peer, "lap"),
            _ => panic!("not fleet-link --peer"),
        }
        assert!(parse(&["fleet-link"]).is_err(), "a peer is required");
        for gone in [&["fleet-link", "--worker", "w", "--root", "/r"][..], &["lend"], &["fleet", "join", "me@control_plane"]] {
            assert!(parse(gone).is_err(), "{gone:?}");
        }
        assert!(matches!(parse(&["fleet-hello"]).unwrap().command, Some(Command::FleetHello)));
    }

    #[cfg(unix)]
    #[test]
    fn machine_upgrade_parses() {
        use super::MachineCommand;
        let parse = |args: &[&str]| Cli::try_parse_from(std::iter::once("devsandbox").chain(args.iter().copied()));
        let upgrade = |args: &[&str]| match parse(args).unwrap().command {
            Some(Command::Machine { cmd: MachineCommand::Upgrade { name, yes } }) => (name, yes),
            _ => panic!("not machine upgrade"),
        };
        assert_eq!(upgrade(&["machine", "upgrade", "box"]), ("box".into(), false));
        assert_eq!(upgrade(&["machine", "upgrade", "box", "--yes"]), ("box".into(), true));
        assert!(parse(&["machine", "upgrade"]).is_err(), "a name is required");
    }

    #[cfg(unix)]
    #[test]
    fn machine_allow_parses() {
        use super::MachineCommand;
        let parse = |args: &[&str]| Cli::try_parse_from(std::iter::once("devsandbox").chain(args.iter().copied()));
        let change = |args: &[&str]| match parse(args).unwrap().command {
            Some(Command::Machine { cmd: MachineCommand::Allow { allow } }) => allow.change(),
            _ => panic!("not machine allow"),
        };
        assert!(change(&["machine", "allow"]).flags.is_empty());
        let c = change(&["machine", "allow", "--gpus", "on", "--runtime-socket", "off", "--bind-add", "/a", "--bind-add", "b", "--local-env-rm", "T"]);
        assert_eq!(c.flags, [("runtime_socket", false), ("gpus", true)]);
        assert_eq!((c.bind_add, c.local_env_rm), (vec!["/a".to_string(), "b".into()], vec!["T".to_string()]));
        assert!(parse(&["machine", "allow", "--gpus", "yes"]).is_err());
        assert!(parse(&["machine", "allow", "--gpus"]).is_err(), "on or off is required");
    }

    #[cfg(unix)]
    #[test]
    fn machine_add_takes_allowances() {
        use super::MachineCommand;
        let parse = |args: &[&str]| Cli::try_parse_from(std::iter::once("devsandbox").chain(args.iter().copied()));
        let allow = |args: &[&str]| match parse(args).unwrap().command {
            Some(Command::Machine { cmd: MachineCommand::Add { allow, allow_bind, allow_local_env, .. } }) => (allow, allow_bind, allow_local_env),
            _ => panic!("not machine add"),
        };
        assert_eq!(allow(&["machine", "add", "box"]), (vec![], vec![], vec![]));
        let args = ["machine", "add", "box", "--allow", "gpus,host-network", "--allow", "inbox", "--allow-bind", "/srv", "--allow-local-env", "T"];
        assert_eq!(allow(&args), (vec!["gpus", "host_network", "inbox"], vec!["/srv".to_string()], vec!["T".to_string()]));
        assert!(parse(&["machine", "add", "box", "--allow", "root"]).is_err());
    }

    #[test]
    fn profile_parses_with_and_without_a_profile() {
        let parse = |args: &[&str]| Cli::try_parse_from(std::iter::once("devsandbox").chain(args.iter().copied()));
        match parse(&["profile", "web-1", "work"]).unwrap().command {
            Some(Command::Profile { name, profile }) => assert_eq!((name.as_str(), profile.as_deref()), ("web-1", Some("work"))),
            _ => panic!("not profile"),
        }
        match parse(&["profile", "box/web-1"]).unwrap().command {
            Some(Command::Profile { name, profile: None }) => assert_eq!(name, "box/web-1"),
            _ => panic!("not profile without one"),
        }
        assert!(parse(&["profile"]).is_err(), "an instance is required");
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

    /// `exec`'s `--end`.
    fn end_of(args: &[&str]) -> Result<Option<String>, clap::Error> {
        match Cli::try_parse_from(["devsandbox", "exec"].iter().chain(args))?.command {
            Some(Command::Exec { end, .. }) => Ok(end),
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
        assert_eq!(end_of(&["web", "--end", id]).unwrap(), Some(s(id)));
        assert_eq!(end_of(&["--end=1790000000-a1b2", "web"]).unwrap(), Some(s(id)));
        assert_eq!(end_of(&["web"]).unwrap(), None);
        assert!(end_of(&["web", "--end"]).is_err(), "--end needs its ID");
        for conflict in [
            &["web", "--end", id, "--", "ls"][..],
            &["web", "--end", id, "--attach"],
            &["web", "--end", id, "--sessions"],
            &["--session", id, "web", "--end", id],
            &["-i", "web", "--end", id],
            &["-t", "web", "--end", id],
            &["web", "--end", id, "--service", "db"],
            &["web", "--attach", "--", "ls"],
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

    #[test]
    fn exec_service_parses_with_a_command_and_conflicts_with_sessions() {
        let service = |args: &[&str]| -> Result<(Option<String>, String, Vec<String>), clap::Error> {
            let cli = Cli::try_parse_from(["devsandbox", "exec"].iter().chain(args))?;
            match cli.command {
                Some(Command::Exec { service, name, command, .. }) => Ok((service, name, command)),
                _ => panic!("not exec"),
            }
        };
        let s = |v: &str| v.to_string();
        assert_eq!(service(&["web", "--service", "db"]).unwrap(), (Some(s("db")), s("web"), vec![]));
        assert_eq!(service(&["--service=db", "web", "--", "psql", "-l"]).unwrap(), (Some(s("db")), s("web"), vec![s("psql"), s("-l")]));
        assert_eq!(service(&["-it", "web", "--service", "db"]).unwrap().0, Some(s("db")), "-i/-t are what it does anyway");
        assert_eq!(service(&["web"]).unwrap().0, None);
        for conflict in [&["web", "--service", "db", "--attach"][..], &["web", "--service", "db", "--sessions"], &["--session", "1790000000-a1b2", "web", "--service", "db"]] {
            assert_eq!(service(conflict).unwrap_err().kind(), clap::error::ErrorKind::ArgumentConflict, "{conflict:?}");
        }
    }
}
