mod config;
mod docker;
mod state;

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use toml::Value;

use config::{Config, ResolvedSandbox, CONFIG_FILE};
use docker::NAME_PREFIX;
use state::{Instance, State};

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
            Ok(())
        }
        Command::Ps { all } => ps(all),
        Command::Run { sandbox, name } => run(&cli.dir, sandbox, name),
        Command::Exec { interactive, tty, name, command } => {
            exec(&name, interactive, tty, &command)
        }
    }
}

fn ps(all: bool) -> Result<()> {
    let filter = format!("name=^/{NAME_PREFIX}");
    let mut args = vec!["ps", "--filter", &filter];
    if all {
        args.push("--all");
    }
    args.extend(["--format", "table {{.Names}}\t{{.Image}}\t{{.Status}}"]);
    std::process::exit(docker::run_inherit(&args)?);
}

fn run(dir: &Path, sandbox_name: Option<String>, instance_name: Option<String>) -> Result<()> {
    let config = match Config::load(dir) {
        Ok(config) if !config.sandboxes.is_empty() => config,
        _ => return offer_example_config(dir),
    };

    let sandbox_name = match sandbox_name {
        Some(name) => name,
        None => {
            let names: Vec<&str> = config.sandboxes.keys().map(String::as_str).collect();
            if !std::io::stdin().is_terminal() {
                bail!("no sandbox specified; available: {}", names.join(", "));
            }
            names[pick("Select a sandbox", &names)?].to_string()
        }
    };
    let sandbox = config.resolve_sandbox(&sandbox_name)?;

    if sandbox.properties.contains_key("dockerComposeFile") {
        bail!("compose-based sandboxes are not supported yet");
    }

    let image = image_for(dir, &sandbox)?;

    let folder = sandbox
        .folder()
        .with_context(|| format!("sandbox `{}` has no `folder`", sandbox.name))?;
    let folder = dir
        .join(folder)
        .canonicalize()
        .with_context(|| format!("sandbox folder `{folder}` does not exist"))?;
    let basename = folder
        .file_name()
        .context("sandbox folder has no basename")?
        .to_string_lossy()
        .into_owned();
    let workspace = sandbox
        .properties
        .get("workspaceFolder")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("/workspaces/{basename}"));

    let mut state = State::load()?;
    let instance = match instance_name {
        Some(name) => {
            if state.instances.contains_key(&name) {
                bail!("instance `{name}` already exists");
            }
            name
        }
        None => format!("{sandbox_name}-{}", suffix()),
    };
    let container = format!("{NAME_PREFIX}{instance}");

    let mount = format!("{}:{workspace}", folder.display());
    let sandbox_label = format!("devsandbox.sandbox={sandbox_name}");
    let instance_label = format!("devsandbox.instance={instance}");
    let mut args: Vec<String> = [
        "run", "-d", "--name", &container,
        "--label", &sandbox_label, "--label", &instance_label,
        "-v", &mount, "-w", &workspace,
    ]
    .map(str::to_string)
    .into();
    if let Some(env) = sandbox.properties.get("containerEnv").and_then(Value::as_table) {
        for (key, value) in env {
            let value = value.as_str().map(str::to_string).unwrap_or_else(|| value.to_string());
            args.push("-e".into());
            args.push(format!("{key}={value}"));
        }
    }
    args.push(image);
    // Same trick as devcontainers: keep the container alive, work happens via exec.
    args.extend(["sleep".into(), "infinity".into()]);

    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    docker::run_checked(&arg_refs)?;

    state.instances.insert(
        instance.clone(),
        Instance {
            sandbox: sandbox_name,
            container,
            folder,
            workspace,
            created_unix: Instance::now(),
        },
    );
    state.save()?;

    println!("{instance}");
    Ok(())
}

/// Image to run: the `image` property as-is, or a local build for
/// `build.dockerfile` sandboxes.
fn image_for(dir: &Path, sandbox: &ResolvedSandbox) -> Result<String> {
    if let Some(image) = sandbox.properties.get("image").and_then(Value::as_str) {
        return Ok(image.to_string());
    }
    let build = sandbox
        .properties
        .get("build")
        .and_then(Value::as_table)
        .with_context(|| format!("sandbox `{}` has neither `image` nor `build`", sandbox.name))?;
    let dockerfile = build
        .get("dockerfile")
        .and_then(Value::as_str)
        .with_context(|| format!("sandbox `{}`: `build.dockerfile` is required", sandbox.name))?;
    let context = build.get("context").and_then(Value::as_str).unwrap_or(".");
    let tag = format!("{NAME_PREFIX}img-{}", sandbox.name);
    docker::run_checked(&[
        "build",
        "-t", &tag,
        "-f", &dir.join(dockerfile).to_string_lossy(),
        &dir.join(context).to_string_lossy(),
    ])?;
    Ok(tag)
}

fn exec(name: &str, interactive: bool, tty: bool, command: &[String]) -> Result<()> {
    let state = State::load()?;

    // <name> may be an instance name, a sandbox config name, or a repository
    // (folder basename); collect every instance it could refer to.
    let mut matches: Vec<(&String, &Instance)> = state
        .instances
        .iter()
        .filter(|(instance, info)| {
            *instance == name
                || info.sandbox == name
                || info.folder.file_name().is_some_and(|f| f == name)
        })
        .collect();

    let (_, instance) = match matches.len() {
        0 => bail!("no sandbox instance matches `{name}` (see `devsandbox ps`)"),
        1 => matches.remove(0),
        _ => {
            let labels: Vec<String> = matches
                .iter()
                .map(|(instance, info)| format!("{instance} (sandbox {})", info.sandbox))
                .collect();
            let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
            if !std::io::stdin().is_terminal() {
                bail!("`{name}` is ambiguous: {}", labels.join(", "));
            }
            matches.remove(pick(&format!("`{name}` is ambiguous"), &label_refs)?)
        }
    };

    let mut args = vec!["exec"];
    if interactive {
        args.push("-i");
    }
    if tty {
        args.push("-t");
    }
    args.extend(["-w", &instance.workspace]);
    args.push(&instance.container);
    args.extend(command.iter().map(String::as_str));
    std::process::exit(docker::run_inherit(&args)?);
}

fn offer_example_config(dir: &Path) -> Result<()> {
    let path = dir.join(CONFIG_FILE);
    eprintln!("No sandboxes defined in {}.", path.display());
    if !std::io::stdin().is_terminal() {
        bail!("nothing to run");
    }
    eprint!("Generate an example config? [y/N] ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if !matches!(answer.trim(), "y" | "Y") {
        bail!("nothing to run");
    }
    if path.exists() {
        bail!("{} already exists, not overwriting", path.display());
    }
    std::fs::write(&path, EXAMPLE_CONFIG)
        .with_context(|| format!("cannot write {}", path.display()))?;
    eprintln!("Wrote {}. Edit it, then re-run `devsandbox run`.", path.display());
    Ok(())
}

const EXAMPLE_CONFIG: &str = r#"# devsandbox example config

[template.base]
cache-folder = ".pnpm-store"

[sandbox.example]
extends = "base"
folder = "../example"
image = "mcr.microsoft.com/devcontainers/base:ubuntu"
"#;

/// Numbered menu on stderr; returns the selected index.
fn pick(prompt: &str, options: &[&str]) -> Result<usize> {
    eprintln!("{prompt}:");
    for (i, option) in options.iter().enumerate() {
        eprintln!("  {}) {option}", i + 1);
    }
    eprint!("> ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    let choice: usize = answer.trim().parse().context("invalid selection")?;
    if choice == 0 || choice > options.len() {
        bail!("selection out of range");
    }
    Ok(choice - 1)
}

/// Short unique-enough suffix for generated instance names.
fn suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 + d.as_secs() * 1_000_000_000)
        .unwrap_or(0);
    let mut n = nanos % 36u64.pow(4);
    let mut out = String::new();
    for _ in 0..4 {
        out.push(char::from_digit((n % 36) as u32, 36).unwrap());
        n /= 36;
    }
    out
}
