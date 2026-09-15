mod commands;
mod config;
mod docker;
mod render;
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
    },
    /// Remove a sandbox instance (container, worktree, state entry)
    Rm {
        /// Instance name, sandbox config name, or repository folder name
        name: String,
    },
    /// Stop a sandbox instance (docker stop; `run` restarts it)
    Stop {
        /// Instance name, sandbox config name, or repository folder name
        name: String,
    },
    /// Remove shared services no live instance references
    Gc,
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
        Command::Run { sandbox, name } => commands::run::run(&cli.dir, sandbox, name),
        Command::Rm { name } => commands::rm::rm(&name),
        Command::Stop { name } => commands::stop::stop(&name),
        Command::Gc => commands::services::gc(&cli.dir),
        Command::Exec { interactive, tty, name, command } => {
            commands::exec::exec(&name, interactive, tty, &command)
        }
    }
}
