use std::io::IsTerminal;

use anyhow::{bail, Result};

use super::pick;
use crate::runtime::backend;
use crate::state::{Instance, State};

/// CLI entry point: run the command, then exit the process with its status.
pub fn exec(name: &str, interactive: bool, tty: bool, command: &[String]) -> Result<()> {
    std::process::exit(exec_status(name, interactive, tty, command)?);
}

/// Run the command inside the matching instance and return its exit code.
/// Split out of [`exec`] so callers that must not terminate the process (the
/// dashboard prompt) can reuse it.
pub fn exec_status(name: &str, interactive: bool, tty: bool, command: &[String]) -> Result<i32> {
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

    let mut args: Vec<String> = vec!["exec".into()];
    if interactive {
        args.push("-i".into());
    }
    if tty {
        args.push("-t".into());
    }
    args.extend(["-w".into(), instance.workspace.clone()]);
    if let Some(user) = &instance.remote_user {
        args.push("-u".into());
        args.push(user.clone());
    }
    for (key, value) in &instance.remote_env {
        args.push("-e".into());
        args.push(format!("{key}={value}"));
    }
    args.push(instance.container.clone());
    args.extend(command.iter().cloned());
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    backend().run_inherit(&arg_refs)
}
