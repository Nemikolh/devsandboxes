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
    Ls,
    /// List running sandbox instances
    Ps {
        /// Also show stopped instances
        #[arg(short, long)]
        all: bool,
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
    },
    /// Show CPU/memory usage of running devsandbox containers
    Stats,
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
        Command::Ls => commands::ls::ls(&cli.dir),
        Command::Ps { all } => commands::ps::ps(all),
        Command::Run { sandbox, name, branch } => {
            commands::run::run(&cli.dir, sandbox, name, branch)
        }
        Command::Rebuild { name, all } => commands::rebuild::rebuild(&cli.dir, name, all),
        Command::Rm { name } => commands::rm::rm(&name),
        Command::Stop { name, all } => commands::stop::stop(name, all),
        Command::Start { name, all } => commands::start::start(&cli.dir, name, all),
        Command::Gc { force } => commands::services::gc(&cli.dir, force),
        Command::Logs { name, lines } => commands::logs::logs(&name, lines),
        Command::Inspect { name } => commands::inspect::inspect(&name),
        Command::Stats => commands::stats::stats(),
        Command::Exec { interactive, tty, name, command } => {
            commands::exec::exec(&name, interactive, tty, &command)
        }
    }
}
