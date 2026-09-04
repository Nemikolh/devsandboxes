use std::process::Command;

use anyhow::{bail, Context, Result};

/// Prefix for everything devsandbox creates in docker, so `ps` can filter on it.
pub const NAME_PREFIX: &str = "devsandbox-";

/// Run docker with stdio inherited (build output, exec, ps). Returns the exit code.
pub fn run_inherit(args: &[&str]) -> Result<i32> {
    let status = Command::new("docker")
        .args(args)
        .status()
        .context("failed to run docker (is it installed?)")?;
    Ok(status.code().unwrap_or(1))
}

/// Run docker capturing stdout (stderr inherited); non-zero exit is an error.
pub fn output(args: &[&str]) -> Result<String> {
    let out = Command::new("docker")
        .args(args)
        .stderr(std::process::Stdio::inherit())
        .output()
        .context("failed to run docker (is it installed?)")?;
    if !out.status.success() {
        bail!(
            "docker {} exited with status {}",
            args.first().unwrap_or(&""),
            out.status.code().unwrap_or(1)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Like `run_inherit` but treats a non-zero exit as an error.
pub fn run_checked(args: &[&str]) -> Result<()> {
    let code = run_inherit(args)?;
    if code != 0 {
        bail!("docker {} exited with status {code}", args.first().unwrap_or(&""));
    }
    Ok(())
}
