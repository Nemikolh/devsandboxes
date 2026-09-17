//! Container-runtime backends. Every runtime invocation in devsandbox goes
//! through [`Backend`], so the rest of the code never spells a binary name.
//!
//! Two shapes exist: docker-CLI-compatible runtimes (`docker`, `podman`), which
//! share [`dockerlike::Dockerlike`] and differ only in binary name, and Apple's
//! `container`, whose `inspect`/`ls` are JSON-only (no Go templates, no
//! `--filter`) and which lacks `network connect`, `--network-alias` and `top`.

mod apple;
mod dockerlike;

#[cfg(test)]
pub use apple::AppleContainer;
#[cfg(test)]
pub use dockerlike::Dockerlike;

use std::collections::BTreeMap;
use std::process::Command;
use std::sync::OnceLock;

use anyhow::{bail, Context, Result};

/// Prefix for everything devsandbox creates in the runtime, so listings can
/// filter on it.
pub const NAME_PREFIX: &str = "devsandbox-";

/// Env var that overrides runtime selection: `docker`, `podman` or `container`.
pub const RUNTIME_ENV: &str = "DEVSANDBOX_RUNTIME";

/// One container as reported by the runtime's listing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ContainerRow {
    pub name: String,
    pub image: String,
    /// Human status (`Up 3 minutes`, `Exited (0) 1 hour ago`, or the bare
    /// state on runtimes without one).
    pub status: String,
    /// Normalized state: `running` or anything else (stopped/exited/created…).
    pub state: String,
    pub labels: BTreeMap<String, String>,
    /// Host ports the container publishes.
    pub host_ports: Vec<String>,
}

impl ContainerRow {
    pub fn is_running(&self) -> bool {
        self.state == "running"
    }

    pub fn label(&self, key: &str) -> Option<&str> {
        self.labels.get(key).map(String::as_str)
    }
}

/// Resource usage of one running container, pre-rendered for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatsRow {
    pub name: String,
    pub cpu: String,
    pub mem: String,
}

/// A service a sandbox must reach by name: `(alias, service container)`.
pub struct ServiceEndpoint {
    pub alias: String,
    pub container: String,
}

pub trait Backend: Send + Sync {
    /// Short runtime name for messages and the dashboard header.
    fn name(&self) -> &'static str;
    /// Executable to invoke.
    fn bin(&self) -> &'static str;

    // --- process runners (shared) ---

    /// Run with stdio inherited (build output, exec, ps). Returns the exit code.
    fn run_inherit(&self, args: &[&str]) -> Result<i32> {
        let status = Command::new(self.bin())
            .args(args)
            .status()
            .with_context(|| format!("failed to run {} (is it installed?)", self.bin()))?;
        Ok(status.code().unwrap_or(1))
    }

    /// Capture stdout (stderr inherited); non-zero exit is an error.
    fn output(&self, args: &[&str]) -> Result<String> {
        let out = Command::new(self.bin())
            .args(args)
            .stderr(std::process::Stdio::inherit())
            .output()
            .with_context(|| format!("failed to run {} (is it installed?)", self.bin()))?;
        if !out.status.success() {
            bail!(
                "{} {} exited with status {}",
                self.bin(),
                args.first().unwrap_or(&""),
                out.status.code().unwrap_or(1)
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// Like `output` but with stderr captured instead of inherited, for callers
    /// that own the screen (the TUI): stray stderr would corrupt the alternate
    /// screen. The first stderr line is folded into the error message.
    fn output_quiet(&self, args: &[&str]) -> Result<String> {
        let out = Command::new(self.bin())
            .args(args)
            .output()
            .with_context(|| format!("failed to run {} (is it installed?)", self.bin()))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            bail!(
                "{} {}: {}",
                self.bin(),
                args.first().unwrap_or(&""),
                stderr.lines().next().unwrap_or("non-zero exit").trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// Capture stdout and stderr merged into one string (container logs go to
    /// both). Non-zero exit is an error with the first stderr line folded in.
    fn output_merged(&self, args: &[&str]) -> Result<String> {
        let out = Command::new(self.bin())
            .args(args)
            .output()
            .with_context(|| format!("failed to run {} (is it installed?)", self.bin()))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            bail!(
                "{} {}: {}",
                self.bin(),
                args.first().unwrap_or(&""),
                stderr.lines().next().unwrap_or("non-zero exit").trim()
            );
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let mut merged = String::new();
        merged.push_str(stdout.trim_end());
        let stderr = stderr.trim_end();
        if !stderr.is_empty() {
            if !merged.is_empty() {
                merged.push('\n');
            }
            merged.push_str(stderr);
        }
        Ok(merged.trim().to_string())
    }

    /// Like `run_inherit` but treats a non-zero exit as an error.
    fn run_checked(&self, args: &[&str]) -> Result<()> {
        let code = self.run_inherit(args)?;
        if code != 0 {
            bail!("{} {} exited with status {code}", self.bin(), args.first().unwrap_or(&""));
        }
        Ok(())
    }

    // --- queries (diverge per runtime) ---

    /// `Some(true|false)` for an existing container, `None` when it does not
    /// exist (any lookup failure is treated as "not found").
    fn is_running(&self, container: &str) -> Result<Option<bool>>;

    /// A container label's value; `None` when the container or the label is
    /// missing (an empty value counts as missing).
    fn label(&self, container: &str, key: &str) -> Result<Option<String>>;

    fn network_exists(&self, network: &str) -> Result<bool>;

    /// Raw `inspect` JSON for a container, for the dashboard's inspect pane.
    fn inspect_json(&self, container: &str) -> Result<String>;

    /// Containers whose name starts with `name_prefix` (empty = all). stderr is
    /// captured so this is safe from the TUI.
    fn list(&self, all: bool, name_prefix: &str) -> Result<Vec<ContainerRow>>;

    /// Resource usage of running containers.
    fn stats(&self) -> Result<Vec<StatsRow>>;

    /// Runtime (server) version string.
    fn server_version(&self) -> Result<String>;

    /// Last `n` lines of a container's stdout+stderr.
    fn logs_tail(&self, container: &str, n: usize) -> Result<String>;

    /// `pid,ppid,args` process listing of a container, `ps -eo` style text.
    fn proc_list(&self, container: &str) -> Result<String>;

    // --- mutations that differ in shape ---

    /// Force-remove a container.
    fn remove_force(&self, container: &str) -> Result<i32>;

    /// `run` args attaching `networks`. Docker-likes accept a single `--network`
    /// at creation (the rest join via [`Self::connect_networks`]); Apple's
    /// `container` takes them all up front.
    fn network_run_args(&self, networks: &[String]) -> Vec<String>;

    /// Join networks not covered by [`Self::network_run_args`] after creation.
    fn connect_networks(&self, container: &str, networks: &[String]) -> Result<()>;

    /// `run` args making a service reachable as `alias` (`--network-alias` where
    /// supported; otherwise nothing and [`Self::wire_service_dns`] does it).
    fn service_alias_args(&self, alias: &str) -> Vec<String>;

    /// Make every endpoint resolvable by alias from inside `container`. No-op on
    /// runtimes where the alias was applied at service creation.
    fn wire_service_dns(&self, container: &str, endpoints: &[ServiceEndpoint]) -> Result<()>;

    /// Whether `build --cache-from` is supported.
    fn supports_cache_from(&self) -> bool;
}

/// Pick a backend by name: `docker`, `podman` or `container`.
pub fn backend_named(name: &str) -> Option<Box<dyn Backend>> {
    Some(match name {
        "docker" => Box::new(dockerlike::Dockerlike::DOCKER),
        "podman" => Box::new(dockerlike::Dockerlike::PODMAN),
        "container" | "apple" => Box::new(apple::AppleContainer),
        _ => return None,
    })
}

/// Whether the `docker` client resolves on `PATH`. Client-only (`--version`
/// prints the CLI version without contacting a daemon), so it stays fast and
/// does not hang when no engine is running.
fn docker_on_path() -> bool {
    Command::new("docker")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Backend name devsandbox uses on this host: [`RUNTIME_ENV`] when set, else on
/// macOS `docker` when the `docker` client is on `PATH` (e.g. OrbStack, which
/// ships it and auto-selects its own context, or Docker Desktop) and Apple's
/// `container` otherwise; `docker` everywhere else.
pub fn default_backend_name(env: Option<&str>, macos: bool, docker_on_path: bool) -> &'static str {
    match env {
        Some("docker") => "docker",
        Some("podman") => "podman",
        Some("container") | Some("apple") => "container",
        Some(_) | None if macos => {
            if docker_on_path {
                "docker"
            } else {
                "container"
            }
        }
        _ => "docker",
    }
}

/// The process-wide backend, resolved once.
pub fn backend() -> &'static dyn Backend {
    static BACKEND: OnceLock<Box<dyn Backend>> = OnceLock::new();
    BACKEND
        .get_or_init(|| {
            let env = std::env::var(RUNTIME_ENV).ok();
            if let Some(value) = &env
                && backend_named(value).is_none()
            {
                eprintln!(
                    "warning: unknown {RUNTIME_ENV}=`{value}` (expected docker, podman or container); using the default"
                );
            }
            let macos = cfg!(target_os = "macos");
            // The PATH probe only affects the macOS default arm (no explicit
            // backend selected); skip it whenever it can't change the outcome.
            let explicit = matches!(env.as_deref(), Some("docker" | "podman" | "container" | "apple"));
            let docker = macos && !explicit && docker_on_path();
            let name = default_backend_name(env.as_deref(), macos, docker);
            backend_named(name).expect("default backend name is valid")
        })
        .as_ref()
}

/// Split `k=v` into `(k, v)`; a bare key maps to an empty value.
pub(crate) fn split_kv(pair: &str) -> (String, String) {
    match pair.split_once('=') {
        Some((k, v)) => (k.trim().to_string(), v.trim().to_string()),
        None => (pair.trim().to_string(), String::new()),
    }
}

/// `1.5GiB`-style rendering of a byte count, matching docker's stats output.
pub(crate) fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}B")
    } else if value >= 100.0 {
        format!("{value:.0}{}", UNITS[unit])
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_backend_prefers_env_then_os() {
        // macOS default: docker when the client is on PATH, else Apple container.
        assert_eq!(default_backend_name(None, true, true), "docker");
        assert_eq!(default_backend_name(None, true, false), "container");
        // Non-macOS is always docker; the PATH flag is irrelevant.
        assert_eq!(default_backend_name(None, false, false), "docker");
        // Explicit env wins regardless of OS or PATH.
        assert_eq!(default_backend_name(Some("docker"), true, false), "docker");
        assert_eq!(default_backend_name(Some("podman"), true, true), "podman");
        assert_eq!(default_backend_name(Some("podman"), false, false), "podman");
        assert_eq!(default_backend_name(Some("container"), false, true), "container");
        // Unknown values fall back to the OS default arm.
        assert_eq!(default_backend_name(Some("nope"), true, true), "docker");
        assert_eq!(default_backend_name(Some("nope"), true, false), "container");
        assert_eq!(default_backend_name(Some("nope"), false, false), "docker");
    }

    #[test]
    fn backend_named_maps_binaries() {
        assert_eq!(backend_named("docker").unwrap().bin(), "docker");
        assert_eq!(backend_named("podman").unwrap().bin(), "podman");
        assert_eq!(backend_named("container").unwrap().bin(), "container");
        assert!(backend_named("lxc").is_none());
    }

    #[test]
    fn human_bytes_scales() {
        assert_eq!(human_bytes(512), "512B");
        assert_eq!(human_bytes(50 * 1024 * 1024), "50.0MiB");
        assert_eq!(human_bytes(2 * 1024 * 1024 * 1024), "2.0GiB");
        assert_eq!(human_bytes(150 * 1024 * 1024), "150MiB");
    }
}
