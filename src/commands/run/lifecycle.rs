//! Lifecycle hooks: `initializeCommand` on the host, the rest via `exec` in the
//! container.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::config::LifecycleCommand;
use crate::runtime::backend;

/// Run a lifecycle command's argv lists on the host (initializeCommand).
pub(super) fn run_host_commands(dir: &Path, cmd: &LifecycleCommand) -> Result<()> {
    for argv in cmd.commands() {
        let Some((program, rest)) = argv.split_first() else { continue };
        let status = std::process::Command::new(program)
            .args(rest)
            .current_dir(dir)
            .status()
            .with_context(|| format!("cannot run `{program}`"))?;
        if !status.success() {
            bail!("`{}` exited with status {}", argv.join(" "), status.code().unwrap_or(1));
        }
    }
    Ok(())
}

/// Run a lifecycle command inside the container via `exec`. `env` is the
/// instance's `Instance::exec_env` (remoteEnv, then the saved `--env`), the
/// same set `devsandbox exec` passes.
pub(crate) fn exec_lifecycle(
    container: &str,
    workspace: &str,
    env: Option<&BTreeMap<String, String>>,
    remote_user: Option<&str>,
    ssh_auth_sock: Option<&str>,
    cmd: &LifecycleCommand,
) -> Result<()> {
    for argv in cmd.commands() {
        if argv.is_empty() {
            continue;
        }
        let args = lifecycle_argv(container, workspace, env, remote_user, ssh_auth_sock, argv);
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        backend().run_checked(&arg_refs)?;
    }
    Ok(())
}

/// Pure argv builder for one lifecycle exec, mirroring
/// `commands::exec::exec_argv_with`'s flag order (`-w`, `-u`, env,
/// `SSH_AUTH_SOCK`, container, command) so lifecycle execs carry the same agent
/// env the CLI/TUI do. `ssh_auth_sock` is the shared rule's result
/// (`commands::exec::ssh_auth_sock_env`).
fn lifecycle_argv(
    container: &str,
    workspace: &str,
    env: Option<&BTreeMap<String, String>>,
    remote_user: Option<&str>,
    ssh_auth_sock: Option<&str>,
    argv: Vec<String>,
) -> Vec<String> {
    let mut args: Vec<String> = vec!["exec".into(), "-w".into(), workspace.into()];
    if let Some(user) = remote_user {
        args.push("-u".into());
        args.push(user.to_string());
    }
    for (key, value) in env.into_iter().flatten() {
        args.push("-e".into());
        args.push(format!("{key}={value}"));
    }
    if let Some(sock) = ssh_auth_sock {
        args.push("-e".into());
        args.push(format!("SSH_AUTH_SOCK={sock}"));
    }
    args.push(container.into());
    args.extend(argv);
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_argv_flag_order_and_env() {
        let mut env = BTreeMap::new();
        env.insert("FOO".to_string(), "bar".to_string());
        env.insert("BAZ".to_string(), "qux".to_string());
        let argv = lifecycle_argv(
            "devsandbox-repo-abc1",
            "/workspaces/repository-1",
            Some(&env),
            Some("vscode"),
            Some("/run/devsandbox/ssh-agent.sock"),
            vec!["sh".into(), "-c".into(), "echo hi".into()],
        );
        assert_eq!(
            argv,
            vec![
                "exec",
                "-w",
                "/workspaces/repository-1",
                "-u",
                "vscode",
                // BTreeMap iterates sorted: BAZ before FOO,
                "-e",
                "BAZ=qux",
                "-e",
                "FOO=bar",
                // then SSH_AUTH_SOCK, before the container name.
                "-e",
                "SSH_AUTH_SOCK=/run/devsandbox/ssh-agent.sock",
                "devsandbox-repo-abc1",
                "sh",
                "-c",
                "echo hi",
            ]
        );
    }

    #[test]
    fn lifecycle_argv_omits_sock_when_none() {
        let argv = lifecycle_argv(
            "devsandbox-repo-abc1",
            "/workspaces/repository-1",
            None,
            None,
            None,
            vec!["true".into()],
        );
        assert_eq!(
            argv,
            vec!["exec", "-w", "/workspaces/repository-1", "devsandbox-repo-abc1", "true"]
        );
    }
}
