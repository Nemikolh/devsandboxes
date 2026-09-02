mod config;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use config::Config;

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
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Ls => {
            let config = Config::load(&cli.dir)?;
            for sandbox in config.resolve_all()? {
                println!(
                    "{}\t{}\t{}",
                    sandbox.name,
                    sandbox.source(),
                    sandbox.folder().unwrap_or("-")
                );
            }
        }
    }

    Ok(())
}
