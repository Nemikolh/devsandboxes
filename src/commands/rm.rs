use std::io::{IsTerminal, Write};
use std::path::Path;

use anyhow::{bail, Context, Result};

use super::pick;
use crate::docker;
use crate::state::State;

pub fn rm(name: &str) -> Result<()> {
    let mut state = State::load()?;

    // <name> may be an instance name, a sandbox config name, or a repository
    // (folder basename); collect every instance it could refer to.
    let mut matches: Vec<String> = state
        .instances
        .iter()
        .filter(|(instance, info)| {
            *instance == name
                || info.sandbox == name
                || info.folder.file_name().is_some_and(|f| f == name)
        })
        .map(|(instance, _)| instance.clone())
        .collect();
    matches.sort();

    let key = match matches.len() {
        0 => bail!("no sandbox instance matches `{name}` (see `devsandbox ps -a`)"),
        1 => matches.remove(0),
        _ => {
            if !std::io::stdin().is_terminal() {
                bail!("`{name}` is ambiguous: {}", matches.join(", "));
            }
            let refs: Vec<&str> = matches.iter().map(String::as_str).collect();
            matches.remove(pick(&format!("`{name}` matches multiple instances"), &refs)?)
        }
    };

    let info = state.instances.get(&key).expect("key came from state");
    let container = info.container.clone();
    let worktree = info.worktree.clone();
    let base_folder = info.base_folder.clone();
    let project = info.project.clone();

    // Remove the container; ignore failure (it may already be gone).
    let _ = docker::run_inherit(&["rm", "-f", &container]);

    // Reap this instance's isolated services and its per-instance network. Global
    // services are shared and left to `gc`. Empty `project` = pre-upgrade state.
    if !project.is_empty() {
        let filter_project = format!("label=devsandbox.project={project}");
        let filter_instance = format!("label=devsandbox.instance={key}");
        if let Ok(listing) = docker::output(&[
            "ps", "-aq", "--filter", &filter_project, "--filter", &filter_instance,
        ]) {
            for svc in listing.lines().filter(|l| !l.is_empty()) {
                let _ = docker::run_inherit(&["rm", "-f", svc]);
            }
        }
        let network = crate::commands::services::instance_network(&project, &key);
        let _ = docker::run_inherit(&["network", "rm", &network]);
    }

    if let Some(worktree) = worktree {
        remove_worktree(&base_folder, &worktree)?;
        let branch = format!("sandbox/{key}");
        if confirm(&format!("delete branch `{branch}`?"))? {
            let _ = std::process::Command::new("git")
                .args(["-C", &base_folder.to_string_lossy(), "branch", "-D", &branch])
                .status();
        }
    }

    state.instances.remove(&key);
    state.save()?;
    println!("removed {key}");
    Ok(())
}

fn remove_worktree(base: &Path, worktree: &Path) -> Result<()> {
    let status = std::process::Command::new("git")
        .args([
            "-C",
            &base.to_string_lossy(),
            "worktree",
            "remove",
            &worktree.to_string_lossy(),
        ])
        .status()
        .context("failed to run git (is it installed?)")?;
    if !status.success() {
        bail!(
            "git worktree remove failed for `{}` (commit or discard changes, \
             then retry, or remove it manually)",
            worktree.display()
        );
    }
    Ok(())
}

fn confirm(prompt: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        return Ok(false);
    }
    eprint!("{prompt} [y/N] ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}
