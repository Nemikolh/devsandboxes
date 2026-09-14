mod commands;
mod config;
mod docker;
mod state;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "devsandbox", about = "Manage devcontainer-based sandboxes")]
struct Cli {
    /// Directory containing config.toml (defaults to the current directory).
    #[arg(short = 'C', long, global = true, default_value = ".")]
    dir: PathBuf,

    #[command(subcommand)]
    command: Command,
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

    match cli.command {
        Command::Ls => commands::ls::ls(&cli.dir),
        Command::Ps { all } => commands::ps::ps(all),
        Command::Run { sandbox, name } => commands::run::run(&cli.dir, sandbox, name),
        Command::Rm { name } => commands::rm::rm(&name),
        Command::Exec { interactive, tty, name, command } => {
            commands::exec::exec(&name, interactive, tty, &command)
        }
    }
}
