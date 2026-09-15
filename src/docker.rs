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

/// Like `output` but with stderr captured instead of inherited, for callers
/// that own the screen (the TUI): stray stderr would corrupt the alternate
/// screen. The first stderr line is folded into the error message.
pub fn output_quiet(args: &[&str]) -> Result<String> {
    let out = Command::new("docker")
        .args(args)
        .output()
        .context("failed to run docker (is it installed?)")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!(
            "docker {}: {}",
            args.first().unwrap_or(&""),
            stderr.lines().next().unwrap_or("non-zero exit").trim()
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

/// `docker inspect -f <format> <name>`, returning `None` when the object does
/// not exist (any inspect failure is treated as "not found").
pub fn inspect(name: &str, format: &str) -> Result<Option<String>> {
    let out = Command::new("docker")
        .args(["inspect", "-f", format, name])
        .output()
        .context("failed to run docker (is it installed?)")?;
    if !out.status.success() {
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&out.stdout).trim().to_string()))
}
