mod commands;
mod config;
mod devsbd;
mod features;
mod inbox;
mod json_stdout;
mod render;
mod runtime;
#[cfg(unix)]
mod serve;
mod snapshot;
mod state;
#[cfg(test)]
mod test_support;
mod tui;

use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "devsandbox", about = "Manage devcontainer-based sandboxes")]
struct Cli {
    /// Directory containing devsandboxes.toml (defaults to the current directory).
    #[arg(short = 'C', long, global = true, default_value = ".")]
    dir: PathBuf,

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
        /// Sandbox config to start; interactive menu on a TTY when omitted
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
    },
    /// Recreate an instance's container from the current config (keeps the
    /// worktree, branch, and per-instance state); no-op when there is no drift
    #[command(visible_alias = "recreate")]
    Rebuild {
        /// Instance name, sandbox config name, or repository folder name
        #[arg(required_unless_present = "all")]
        name: Option<String>,
        /// Rebuild every instance whose config has drifted
        #[arg(long, conflicts_with = "name")]
        all: bool,
        /// Recreate even without config drift (picks up devsandbox-side
        /// behavior changes the drift hashes cannot see)
        #[arg(long)]
        force: bool,
    },
    /// Rename an instance (state only; the container keeps its old name)
    Rename {
        /// Exact instance name (no sandbox/folder resolution)
        name: String,
        /// New instance name
        new_name: String,
    },
    /// Remove a sandbox instance (container, worktree, state entry)
    Rm {
        /// Instance name, sandbox config name, or repository folder name
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
    },
    /// Stop a sandbox instance (docker stop; `start` restarts it)
    Stop {
        /// Instance name, sandbox config name, or repository folder name
        #[arg(required_unless_present = "all")]
        name: Option<String>,
        /// Stop every running instance
        #[arg(long, conflicts_with = "name")]
        all: bool,
    },
    /// Start a stopped sandbox instance (services included)
    Start {
        /// Instance name, sandbox config name, or repository folder name
        #[arg(required_unless_present = "all")]
        name: Option<String>,
        /// Start every stopped instance
        #[arg(long, conflicts_with = "name")]
        all: bool,
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
    /// Show the last lines of a container's logs
    Logs {
        /// Instance name, sandbox config name, repository folder name, or
        /// devsandbox container name (service containers included)
        name: String,
        /// Number of log lines to show
        #[arg(short = 'n', long, default_value_t = 50)]
        lines: usize,
    },
    /// Pretty-print the runtime's inspect JSON for a container
    Inspect {
        /// Instance name, sandbox config name, repository folder name, or
        /// devsandbox container name (service containers included)
        name: String,
        /// Output JSON instead of a table
        #[arg(long)]
        json: bool,
    },
    /// Open VS Code attached to a sandbox instance (same as `o` in the TUI)
    Vscode {
        /// Instance name, sandbox config name, or repository folder name
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
        /// Instance name, sandbox config name, or repository folder name
        name: String,
    },
    /// Clear an instance's done mark
    Undone {
        /// Instance name, sandbox config name, or repository folder name
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
    Exec {
        /// Keep stdin open
        #[arg(short)]
        interactive: bool,
        /// Allocate a pseudo-TTY
        #[arg(short)]
        tty: bool,
        /// Instance name, sandbox config name, or repository folder name
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
    Port {
        /// Instance name, sandbox config name, or repository folder name;
        /// omit only with `--service <global-svc>`
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
    /// Run the per-user host daemon in the foreground (docs/serve.md)
    ///
    /// Plumbing: commands start it on demand, detached, and it exits after
    /// 10 minutes without clients. Exits at once (status 0) when one is
    /// already running.
    #[cfg(unix)]
    Serve {
        /// Never exit for idleness
        #[arg(long)]
        keep_alive: bool,
        /// Socket dir (the lazy start passes the one its client resolved)
        #[arg(long, hide = true, value_name = "DIR")]
        socket_dir: Option<PathBuf>,
    },
    /// Relay the daemon's API (docs/api.md) over stdin/stdout, for clients
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
    let cli = Cli::parse();

    // Here rather than in `Config::load`: that also runs on TUI threads, where
    // a notice would garble the screen. A failure leaves the legacy file read
    // in place, so it only warns.
    match config::migrate_legacy(&cli.dir) {
        Ok(Some(new)) => eprintln!("devsandbox: renamed config.toml to {}", new.display()),
        Ok(None) => {}
        Err(e) => eprintln!("devsandbox: warning: {e:#}"),
    }

    let Some(command) = cli.command else {
        if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
            return tui::dashboard(&cli.dir);
        }
        Cli::command().print_help()?;
        std::process::exit(2);
    };

    match command {
        Command::Ls { json } => commands::ls::ls(&cli.dir, json),
        Command::Ps { all, json } => commands::ps::ps(all, json),
        Command::Run { sandbox, name, branch, base, env, dispatcher, json } => {
            // Taken before any work so every child process (and autostart
            // below) writes to stderr; only the final document hits stdout.
            let json_out = json.then(json_stdout::JsonStdout::capture).transpose()?;
            let extras = commands::run::RunExtras { env, dispatcher };
            let result = commands::run::run(&cli.dir, sandbox, name, branch, base, extras);
            if let (None, Ok(Some(key))) = (&json_out, &result) {
                println!("{key}");
            }
            // After, not before: `run web` for an autostart `web` must not
            // create two instances. Whatever `run` returned is kept.
            commands::autostart::autostart(&cli.dir);
            // After autostart, so the daemon's own pass finds this boot done.
            #[cfg(unix)]
            if matches!(result, Ok(Some(_))) {
                serve::client::ensure_running("cli");
            }
            match (result?, json_out) {
                (Some(key), Some(out)) => out.emit(&commands::run::run_record_json(&key)?),
                // Only a written example config gets here: no instance to report.
                (None, Some(_)) => anyhow::bail!("no instance created"),
                (_, None) => Ok(()),
            }
        }
        Command::Rebuild { name, all, force } => {
            commands::rebuild::rebuild(&cli.dir, name, all, force)
        }
        Command::Rename { name, new_name } => commands::rename::rename(&name, &new_name),
        Command::Rm { name, delete_branch, keep_branch, force } => {
            commands::rm::rm(&name, rm_branch_flag(delete_branch, keep_branch), force)
        }
        Command::Stop { name, all } => commands::stop::stop(name, all),
        Command::Start { name, all } => {
            let result = commands::start::start(&cli.dir, name, all);
            commands::autostart::autostart(&cli.dir);
            #[cfg(unix)]
            if result.is_ok() {
                serve::client::ensure_running("cli");
            }
            result
        }
        Command::Gc { force } => commands::services::gc(&cli.dir, force),
        Command::Service { cmd } => match cmd {
            ServiceCommand::Rebuild { name } => commands::services::rebuild(&cli.dir, &name),
            ServiceCommand::Ls { json } => commands::services::ls(&cli.dir, json),
        },
        Command::Logs { name, lines } => commands::logs::logs(&name, lines),
        Command::Inspect { name, json } => commands::inspect::inspect(&name, json),
        Command::Vscode { name, goto } => commands::vscode::vscode(&cli.dir, &name, goto.as_ref()),
        Command::Done { name } => commands::done::done(&name, true),
        Command::Undone { name } => commands::done::done(&name, false),
        Command::Stats { json } => commands::stats::stats(json),
        Command::Status { json: _ } => commands::status::status(&cli.dir),
        Command::Exec { interactive, tty, name, command } => {
            commands::exec::exec(&name, interactive, tty, &command)
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
        Command::Serve { keep_alive, socket_dir } => serve::daemon::serve(socket_dir, keep_alive),
        #[cfg(unix)]
        Command::Api { stdio: _, client } => std::process::exit(serve::relay::stdio(&client)?),
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
}
