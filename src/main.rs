mod commands;
mod config;
mod features;
mod render;
mod runtime;
mod snapshot;
mod state;
mod tui;

use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "devsandbox", about = "Manage devcontainer-based sandboxes")]
struct Cli {
    /// Directory containing config.toml (defaults to the current directory).
    #[arg(short = 'C', long, global = true, default_value = ".")]
    dir: PathBuf,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// List the sandbox configs defined in config.toml
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
        /// `worktree-branch` (supports `${instance}`)
        #[arg(long)]
        branch: Option<String>,
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
    },
    /// Rename an instance (state only; the container keeps its old name)
    Rename {
        /// Instance name, sandbox config name, or repository folder name
        name: String,
        /// New instance name
        new_name: String,
    },
    /// Remove a sandbox instance (container, worktree, state entry)
    Rm {
        /// Instance name, sandbox config name, or repository folder name
        name: String,
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
    /// Run a command in a sandbox instance
    Exec {
        /// Keep stdin open
        #[arg(short)]
        interactive: bool,
        /// Allocate a pseudo-TTY
        #[arg(short)]
        tty: bool,
        /// Instance name, sandbox config name, or repository folder name
        name: String,
        /// Command and arguments to run
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
}

#[derive(Subcommand)]
enum ServiceCommand {
    /// Recreate a service's container(s) from the current config and rewire
    /// every running sandbox that references it (no sandbox restart)
    Rebuild {
        /// Service name from config.toml
        name: String,
    },
    /// List the services defined in config.toml
    Ls {
        /// Output JSON instead of a table
        #[arg(long)]
        json: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

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
        Command::Run { sandbox, name, branch } => {
            commands::run::run(&cli.dir, sandbox, name, branch)
        }
        Command::Rebuild { name, all } => commands::rebuild::rebuild(&cli.dir, name, all),
        Command::Rename { name, new_name } => commands::rename::rename(&name, &new_name),
        Command::Rm { name } => commands::rm::rm(&name),
        Command::Stop { name, all } => commands::stop::stop(name, all),
        Command::Start { name, all } => commands::start::start(&cli.dir, name, all),
        Command::Gc { force } => commands::services::gc(&cli.dir, force),
        Command::Service { cmd } => match cmd {
            ServiceCommand::Rebuild { name } => commands::services::rebuild(&cli.dir, &name),
            ServiceCommand::Ls { json } => commands::services::ls(&cli.dir, json),
        },
        Command::Logs { name, lines } => commands::logs::logs(&name, lines),
        Command::Inspect { name, json } => commands::inspect::inspect(&name, json),
        Command::Stats { json } => commands::stats::stats(json),
        Command::Status { json: _ } => commands::status::status(&cli.dir),
        Command::Exec { interactive, tty, name, command } => {
            commands::exec::exec(&name, interactive, tty, &command)
        }
    }
}
