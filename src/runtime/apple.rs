//! Apple's `container` CLI (github.com/apple/container, 1.x). Shares the
//! `run`/`exec`/`build`/`start`/`stop` surface with docker, but `inspect` and
//! `ls` are JSON-only (no Go templates, no `--filter`), there is no `network
//! connect`, no `--network-alias` and no `top`, and `logs` tails with `-n`.
//! JSON keys are the Swift property names verbatim (no key strategy).

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use super::{
    human_bytes, without_self_ps, Backend, ContainerRow, ProcList, ServiceEndpoint, StatsRow,
    PROC_PS_ARGS,
};

pub struct AppleContainer;

/// Element of `container inspect` / `container ls --format json`.
#[derive(Debug, Deserialize)]
struct ManagedContainer {
    id: String,
    #[serde(default)]
    configuration: Configuration,
    #[serde(default)]
    status: Status,
}

#[derive(Debug, Default, Deserialize)]
struct Configuration {
    #[serde(default)]
    image: Option<Image>,
    #[serde(default)]
    labels: BTreeMap<String, String>,
    /// Shape not pinned; scanned for a `hostPort` key.
    #[serde(rename = "publishedPorts", default)]
    published_ports: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct Image {
    #[serde(default)]
    reference: String,
}

#[derive(Debug, Default, Deserialize)]
struct Status {
    /// `running` | `stopped` | `stopping` | `unknown`.
    #[serde(default)]
    state: String,
    #[serde(default)]
    networks: Vec<Attachment>,
}

#[derive(Debug, Deserialize)]
struct Attachment {
    /// CIDR form, e.g. `192.168.64.3/24`.
    #[serde(rename = "ipv4Address", default)]
    ipv4_address: Option<String>,
}

impl ManagedContainer {
    fn into_row(self) -> ContainerRow {
        let host_ports = self
            .configuration
            .published_ports
            .iter()
            .filter_map(|p| p.get("hostPort"))
            .filter_map(|p| p.as_u64().map(|n| n.to_string()).or_else(|| p.as_str().map(str::to_string)))
            .collect();
        ContainerRow {
            name: self.id,
            image: self.configuration.image.map(|i| i.reference).unwrap_or_default(),
            status: self.status.state.clone(),
            state: self.status.state,
            labels: self.configuration.labels,
            host_ports,
        }
    }

    /// First IPv4 address without its prefix length.
    fn ipv4(&self) -> Option<&str> {
        self.status
            .networks
            .iter()
            .find_map(|a| a.ipv4_address.as_deref())
            .map(|cidr| cidr.split('/').next().unwrap_or(cidr))
    }
}

fn parse_containers(json: &str) -> Result<Vec<ManagedContainer>> {
    serde_json::from_str(json).context("unexpected `container` JSON")
}

impl AppleContainer {
    /// `inspect <name>`, `None` when the container does not exist (inspect
    /// exits non-zero for unknown ids).
    fn inspect_one(&self, name: &str) -> Result<Option<ManagedContainer>> {
        let Ok(out) = self.output_quiet(&["inspect", name]) else {
            return Ok(None);
        };
        Ok(parse_containers(&out)?.into_iter().next())
    }
}

/// Element of `container stats --format json`. Counters are raw; a single
/// sample has no CPU percentage.
#[derive(Debug, Deserialize)]
struct Stats {
    id: String,
    #[serde(rename = "memoryUsageBytes")]
    memory_usage_bytes: Option<u64>,
    #[serde(rename = "memoryLimitBytes")]
    memory_limit_bytes: Option<u64>,
}

fn parse_stats(json: &str) -> Result<Vec<StatsRow>> {
    let stats: Vec<Stats> = serde_json::from_str(json).context("unexpected `container stats` JSON")?;
    Ok(stats
        .into_iter()
        .map(|s| StatsRow {
            name: s.id,
            cpu: "-".into(),
            mem: match (s.memory_usage_bytes, s.memory_limit_bytes) {
                (Some(used), Some(limit)) => {
                    format!("{} / {}", human_bytes(used), human_bytes(limit))
                }
                (Some(used), None) => human_bytes(used),
                _ => "-".into(),
            },
        })
        .collect())
}

/// Element of `container system version --format json`: `[cli, server?]`.
#[derive(Debug, Deserialize)]
struct VersionInfo {
    version: String,
}

fn parse_version(json: &str) -> Result<String> {
    let versions: Vec<VersionInfo> =
        serde_json::from_str(json).context("unexpected `container system version` JSON")?;
    // The API server entry (index 1) is present only when it answers; fall
    // back to the CLI's own version.
    versions
        .last()
        .map(|v| v.version.clone())
        .context("empty version list")
}

/// Aliases and addresses are interpolated into a shell script; keep them to
/// hostname/IP characters so quoting can't be broken.
fn hosts_token_ok(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':'))
}

/// `sh -c` script that replaces `alias`'s `/etc/hosts` line with `ip alias`.
/// Rewrites in place (no rename) so it also works when `/etc/hosts` is a mount.
fn hosts_script(entries: &[(String, String)]) -> String {
    let mut script = String::from("set -e; h=/etc/hosts; ");
    for (ip, alias) in entries {
        script.push_str(&format!(
            "t=$(grep -v '[[:space:]]{alias}$' $h || true); printf '%s\\n{ip} {alias}\\n' \"$t\" > $h; "
        ));
    }
    script
}

impl Backend for AppleContainer {
    fn name(&self) -> &'static str {
        "container"
    }

    fn bin(&self) -> &'static str {
        "container"
    }

    fn is_running(&self, container: &str) -> Result<Option<bool>> {
        Ok(self.inspect_one(container)?.map(|c| c.status.state == "running"))
    }

    fn label(&self, container: &str, key: &str) -> Result<Option<String>> {
        Ok(self
            .inspect_one(container)?
            .and_then(|c| c.configuration.labels.get(key).cloned())
            .filter(|v| !v.is_empty()))
    }

    fn network_exists(&self, network: &str) -> Result<bool> {
        Ok(self.output_quiet(&["network", "inspect", network]).is_ok())
    }

    fn inspect_json(&self, container: &str) -> Result<String> {
        self.output_quiet(&["inspect", container])
    }

    fn list(&self, all: bool, name_prefix: &str) -> Result<Vec<ContainerRow>> {
        let mut args = vec!["ls"];
        if all {
            args.push("--all");
        }
        args.extend(["--format", "json"]);
        let out = self.output_quiet(&args)?;
        Ok(parse_containers(&out)?
            .into_iter()
            .filter(|c| c.id.starts_with(name_prefix))
            .map(ManagedContainer::into_row)
            .collect())
    }

    fn stats(&self) -> Result<Vec<StatsRow>> {
        let out = self.output_quiet(&["stats", "--no-stream", "--format", "json"])?;
        parse_stats(&out)
    }

    fn server_version(&self) -> Result<String> {
        let out = self.output_quiet(&["system", "version", "--format", "json"])?;
        parse_version(&out)
    }

    fn logs_tail(&self, container: &str, n: usize) -> Result<String> {
        self.output_merged(&["logs", "-n", &n.to_string(), container])
    }

    fn proc_list(&self, container: &str) -> Result<ProcList> {
        // No `top` subcommand; ask procps inside the container instead, so pids
        // are container-namespace and signalable.
        let mut exec = vec!["exec", container];
        exec.extend_from_slice(&PROC_PS_ARGS);
        let text = self.output_quiet(&exec)?;
        Ok(ProcList { text: without_self_ps(&text), container_pids: true })
    }

    fn remove_force(&self, container: &str) -> Result<i32> {
        self.run_inherit(&["delete", "--force", container])
    }

    fn network_run_args(&self, networks: &[String]) -> Vec<String> {
        networks
            .iter()
            .flat_map(|n| ["--network".to_string(), n.clone()])
            .collect()
    }

    fn connect_networks(&self, _container: &str, _networks: &[String]) -> Result<()> {
        // All networks were attached at `run`; there is no `network connect`.
        Ok(())
    }

    fn service_alias_args(&self, _alias: &str) -> Vec<String> {
        Vec::new()
    }

    fn wire_service_dns(&self, container: &str, endpoints: &[ServiceEndpoint]) -> Result<()> {
        if endpoints.is_empty() {
            return Ok(());
        }
        let mut entries = Vec::with_capacity(endpoints.len());
        for endpoint in endpoints {
            let service = self
                .inspect_one(&endpoint.container)?
                .with_context(|| format!("service container `{}` not found", endpoint.container))?;
            let ip = service.ipv4().with_context(|| {
                format!("service container `{}` has no IPv4 address yet", endpoint.container)
            })?;
            if !hosts_token_ok(&endpoint.alias) || !hosts_token_ok(ip) {
                bail!("cannot map service `{}` to `{ip}` in /etc/hosts", endpoint.alias);
            }
            entries.push((ip.to_string(), endpoint.alias.clone()));
        }
        let script = hosts_script(&entries);
        self.run_checked(&["exec", "-u", "root", container, "sh", "-c", &script])
    }

    fn supports_cache_from(&self) -> bool {
        false
    }

    fn supports_file_binds(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INSPECT: &str = r#"[{
      "id": "devsandbox-repo",
      "configuration": {
        "id": "devsandbox-repo",
        "image": {"reference": "docker.io/library/node:22", "descriptor": {"mediaType": "x", "digest": "sha256:0", "size": 1}},
        "labels": {"devsandbox.sandbox": "repo", "devsandbox.config_hash": "abc"},
        "publishedPorts": [{"hostAddress": "0.0.0.0", "hostPort": 5432, "containerPort": 5432, "proto": "tcp"}],
        "creationDate": "2026-09-15T10:00:00Z"
      },
      "status": {
        "state": "running",
        "networks": [{"network": "devsandbox-net-p", "hostname": "devsandbox-repo", "ipv4Address": "192.168.64.3/24", "ipv4Gateway": "192.168.64.1"}],
        "startedDate": "2026-09-15T10:00:01Z"
      }
    }]"#;

    #[test]
    fn inspect_maps_to_row() {
        let containers = parse_containers(INSPECT).unwrap();
        assert_eq!(containers[0].ipv4(), Some("192.168.64.3"));
        let row = containers.into_iter().next().unwrap().into_row();
        assert_eq!(row.name, "devsandbox-repo");
        assert_eq!(row.image, "docker.io/library/node:22");
        assert!(row.is_running());
        assert_eq!(row.label("devsandbox.config_hash"), Some("abc"));
        assert_eq!(row.host_ports, vec!["5432"]);
    }

    #[test]
    fn inspect_tolerates_sparse_objects() {
        let rows = parse_containers(r#"[{"id":"x","status":{"state":"stopped"}}]"#).unwrap();
        let row = rows.into_iter().next().unwrap().into_row();
        assert_eq!(row.name, "x");
        assert!(!row.is_running());
        assert!(row.image.is_empty());
    }

    #[test]
    fn stats_render_memory_without_cpu() {
        let json = r#"[{"id":"devsandbox-repo","memoryUsageBytes":52428800,"memoryLimitBytes":2147483648,"cpuUsageUsec":1234}]"#;
        let stats = parse_stats(json).unwrap();
        assert_eq!(stats[0].name, "devsandbox-repo");
        assert_eq!(stats[0].cpu, "-");
        assert_eq!(stats[0].mem, "50.0MiB / 2.0GiB");
    }

    #[test]
    fn version_prefers_server_entry() {
        let json = r#"[{"version":"1.4.1","buildType":"release","commit":"a","appName":"container"},{"version":"1.4.0","buildType":"release","commit":"b","appName":"container-apiserver"}]"#;
        assert_eq!(parse_version(json).unwrap(), "1.4.0");
        let cli_only = r#"[{"version":"1.4.1","buildType":"release","commit":"a","appName":"container"}]"#;
        assert_eq!(parse_version(cli_only).unwrap(), "1.4.1");
    }

    #[test]
    fn network_args_repeat_per_network() {
        let nets = vec!["a".to_string(), "b".to_string()];
        assert_eq!(
            AppleContainer.network_run_args(&nets),
            vec!["--network", "a", "--network", "b"]
        );
    }

    #[test]
    fn hosts_script_replaces_alias_line() {
        let script = hosts_script(&[("192.168.64.3".into(), "db".into())]);
        assert!(script.contains("grep -v '[[:space:]]db$'"));
        assert!(script.contains("192.168.64.3 db"));
        assert!(hosts_token_ok("db-primary.local"));
        assert!(!hosts_token_ok("db; rm -rf /"));
    }
}
