//! Docker-CLI-compatible runtimes: `docker` and `podman`. Same subcommands and
//! flags, Go-template `--format`, `--filter`, `network connect`,
//! `--network-alias`, `top`. Podman's `ps --format {{json .}}` differs slightly
//! in field types (arrays/maps where docker emits strings); the parsers accept
//! both.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::Deserialize;

use super::{
    split_kv, without_self_ps, Backend, ContainerRow, ProcList, ServiceEndpoint, StatsRow,
    PROC_PS_ARGS,
};

pub struct Dockerlike {
    name: &'static str,
    bin: &'static str,
}

impl Dockerlike {
    pub const DOCKER: Dockerlike = Dockerlike { name: "docker", bin: "docker" };
    pub const PODMAN: Dockerlike = Dockerlike { name: "podman", bin: "podman" };

    /// `inspect -f <template> <name>`, `None` when the object does not exist.
    fn inspect_template(&self, name: &str, template: &str) -> Result<Option<String>> {
        let out = std::process::Command::new(self.bin)
            .args(["inspect", "-f", template, name])
            .output()
            .map_err(|e| anyhow::anyhow!("failed to run {} (is it installed?): {e}", self.bin))?;
        if !out.status.success() {
            return Ok(None);
        }
        Ok(Some(String::from_utf8_lossy(&out.stdout).trim().to_string()))
    }
}

impl Backend for Dockerlike {
    fn name(&self) -> &'static str {
        self.name
    }

    fn bin(&self) -> &'static str {
        self.bin
    }

    fn is_running(&self, container: &str) -> Result<Option<bool>> {
        Ok(self
            .inspect_template(container, "{{.State.Running}}")?
            .map(|v| v == "true"))
    }

    fn label(&self, container: &str, key: &str) -> Result<Option<String>> {
        let template = format!("{{{{index .Config.Labels \"{key}\"}}}}");
        Ok(self
            .inspect_template(container, &template)?
            .filter(|v| !v.is_empty()))
    }

    fn network_exists(&self, network: &str) -> Result<bool> {
        Ok(self.inspect_template(network, "{{.Id}}")?.is_some())
    }

    fn inspect_json(&self, container: &str) -> Result<String> {
        self.output_quiet(&["inspect", container])
    }

    fn list(&self, all: bool, name_prefix: &str) -> Result<Vec<ContainerRow>> {
        let mut args = vec!["ps"];
        if all {
            args.push("--all");
        }
        // docker matches `name=` as a regex against `/name`, podman against the
        // bare name; a plain substring works for both and the prefix is
        // enforced below.
        let filter = format!("name={name_prefix}");
        if !name_prefix.is_empty() {
            args.extend(["--filter", &filter]);
        }
        args.extend(["--format", "{{json .}}"]);
        let out = self.output_quiet(&args)?;
        Ok(parse_ps(&out)
            .into_iter()
            .filter(|row| row.name.starts_with(name_prefix))
            .collect())
    }

    fn stats(&self) -> Result<Vec<StatsRow>> {
        let out = self.output_quiet(&["stats", "--no-stream", "--format", "{{json .}}"])?;
        Ok(parse_stats(&out))
    }

    fn server_version(&self) -> Result<String> {
        self.output_quiet(&["version", "--format", "{{.Server.Version}}"])
    }

    fn logs_tail(&self, container: &str, n: usize) -> Result<String> {
        self.output_merged(&["logs", "--tail", &n.to_string(), container])
    }

    fn proc_list(&self, container: &str) -> Result<ProcList> {
        // Prefer container-namespace pids via `exec ps` so process-row signals
        // can target the shown pid (`top` reports host-namespace pids, which
        // `exec kill` inside the container can't reach). Fall back to `top` for
        // images without `ps`: those still list, but as non-signalable host pids.
        let mut exec = vec!["exec", container];
        exec.extend_from_slice(&PROC_PS_ARGS);
        match self.output_quiet(&exec) {
            Ok(text) => Ok(ProcList { text: without_self_ps(&text), container_pids: true }),
            Err(_) => {
                let text = self.output_quiet(&["top", container, "-eo", "pid,ppid,args"])?;
                Ok(ProcList { text, container_pids: false })
            }
        }
    }

    fn remove_force(&self, container: &str) -> Result<i32> {
        self.run_inherit(&["rm", "-f", container])
    }

    fn network_run_args(&self, networks: &[String]) -> Vec<String> {
        match networks.first() {
            Some(first) => vec!["--network".into(), first.clone()],
            None => Vec::new(),
        }
    }

    fn connect_networks(&self, container: &str, networks: &[String]) -> Result<()> {
        for network in networks.iter().skip(1) {
            self.run_checked(&["network", "connect", network, container])?;
        }
        Ok(())
    }

    fn service_alias_args(&self, alias: &str) -> Vec<String> {
        vec!["--network-alias".into(), alias.to_string()]
    }

    fn wire_service_dns(&self, _container: &str, _endpoints: &[ServiceEndpoint]) -> Result<()> {
        Ok(())
    }

    fn supports_cache_from(&self) -> bool {
        true
    }

    fn supports_file_binds(&self) -> bool {
        true
    }

    fn supports_privileged(&self) -> bool {
        true
    }

    fn image_entrypoint(&self, image: &str) -> Result<Vec<String>> {
        let out = self.output_quiet(&[
            "image",
            "inspect",
            "--format",
            "{{json .Config.Entrypoint}}",
            image,
        ])?;
        parse_entrypoint(&out)
    }
}

/// `{{json .Config.Entrypoint}}`: `null` (unset) or a string array.
fn parse_entrypoint(out: &str) -> Result<Vec<String>> {
    let value: Option<Vec<String>> = serde_json::from_str(out.trim())
        .with_context(|| format!("unexpected image entrypoint `{}`", out.trim()))?;
    Ok(value.unwrap_or_default())
}

/// A string, or a list of strings (podman emits `Names` as an array).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum StringOrList {
    One(String),
    Many(Vec<String>),
}

impl StringOrList {
    fn first(self) -> String {
        match self {
            Self::One(s) => s,
            Self::Many(v) => v.into_iter().next().unwrap_or_default(),
        }
    }
}

/// `k=v,k2=v2` (docker) or a JSON object (podman).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum LabelsField {
    Csv(String),
    Map(BTreeMap<String, String>),
}

impl LabelsField {
    fn into_map(self) -> BTreeMap<String, String> {
        match self {
            Self::Map(m) => m,
            Self::Csv(s) => s
                .split(',')
                .filter(|p| !p.trim().is_empty())
                .map(split_kv)
                .collect(),
        }
    }
}

/// One line of `ps --format {{json .}}`.
#[derive(Debug, Deserialize)]
struct PsLine {
    #[serde(rename = "Names")]
    names: StringOrList,
    #[serde(rename = "Image", default)]
    image: String,
    #[serde(rename = "Status", default)]
    status: String,
    #[serde(rename = "State", default)]
    state: String,
    #[serde(rename = "Labels", default)]
    labels: Option<LabelsField>,
    #[serde(rename = "Ports", default)]
    ports: Option<serde_json::Value>,
}

/// Parse `ps --format {{json .}}` output (one JSON object per line).
/// Unparseable lines are skipped rather than failing the whole listing.
fn parse_ps(out: &str) -> Vec<ContainerRow> {
    out.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<PsLine>(l).ok())
        .map(|line| ContainerRow {
            name: line.names.first(),
            image: line.image,
            status: line.status,
            state: line.state,
            labels: line.labels.map(LabelsField::into_map).unwrap_or_default(),
            host_ports: host_ports(line.ports.as_ref()),
        })
        .collect()
}

/// Host ports out of docker's `Ports` string (`0.0.0.0:5432->5432/tcp, …`) or
/// podman's list of `{host_port, …}` objects.
fn host_ports(ports: Option<&serde_json::Value>) -> Vec<String> {
    let mut out = Vec::new();
    match ports {
        Some(serde_json::Value::String(s)) => {
            for part in s.split(',') {
                let Some((host, _)) = part.trim().split_once("->") else { continue };
                if let Some(port) = host.rsplit(':').next()
                    && !port.is_empty()
                {
                    out.push(port.to_string());
                }
            }
        }
        Some(serde_json::Value::Array(items)) => {
            for item in items {
                if let Some(port) = item.get("host_port").and_then(|p| p.as_u64()) {
                    out.push(port.to_string());
                }
            }
        }
        _ => {}
    }
    out.sort();
    out.dedup();
    out
}

/// One line of `stats --no-stream --format {{json .}}`.
#[derive(Debug, Deserialize)]
struct StatsLine {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "CPUPerc")]
    cpu: String,
    #[serde(rename = "MemUsage")]
    mem: String,
}

fn parse_stats(out: &str) -> Vec<StatsRow> {
    out.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<StatsLine>(l).ok())
        .map(|s| StatsRow { name: s.name, cpu: s.cpu, mem: s.mem })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ps_docker_shape() {
        let out = concat!(
            r#"{"Names":"devsandbox-repo","Image":"node:22","Status":"Up 3 minutes","State":"running","Labels":"devsandbox.sandbox=repo,devsandbox.instance=repo","Ports":"0.0.0.0:5432->5432/tcp, :::5432->5432/tcp"}"#,
            "\n",
            r#"{"Names":"devsandbox-x","Image":"alpine","Status":"Exited (0) 1 hour ago","State":"exited","Labels":"","Ports":""}"#,
            "\n",
            "not json\n",
        );
        let rows = parse_ps(out);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "devsandbox-repo");
        assert!(rows[0].is_running());
        assert_eq!(rows[0].label("devsandbox.sandbox"), Some("repo"));
        assert_eq!(rows[0].host_ports, vec!["5432"]);
        assert!(!rows[1].is_running());
        assert!(rows[1].labels.is_empty());
    }

    #[test]
    fn parse_ps_podman_shape() {
        let out = r#"{"Names":["devsandbox-repo"],"Image":"node:22","Status":"Up 5 seconds ago","State":"running","Labels":{"devsandbox.sandbox":"repo"},"Ports":[{"host_ip":"","container_port":80,"host_port":8080,"range":1,"protocol":"tcp"}]}"#;
        let rows = parse_ps(out);
        assert_eq!(rows[0].name, "devsandbox-repo");
        assert_eq!(rows[0].label("devsandbox.sandbox"), Some("repo"));
        assert_eq!(rows[0].host_ports, vec!["8080"]);
    }

    #[test]
    fn parse_stats_maps_fields() {
        let out = concat!(
            r#"{"Name":"devsandbox-repo","CPUPerc":"1.20%","MemUsage":"50MiB / 2GiB"}"#,
            "\n",
            "garbage\n",
        );
        let stats = parse_stats(out);
        assert_eq!(
            stats,
            vec![StatsRow {
                name: "devsandbox-repo".into(),
                cpu: "1.20%".into(),
                mem: "50MiB / 2GiB".into(),
            }]
        );
    }

    #[test]
    fn parse_entrypoint_null_or_array() {
        assert!(parse_entrypoint("null\n").unwrap().is_empty());
        assert!(parse_entrypoint("[]").unwrap().is_empty());
        assert_eq!(
            parse_entrypoint(r#"["/usr/bin/tini","--"]"#).unwrap(),
            ["/usr/bin/tini", "--"]
        );
        assert!(parse_entrypoint("garbage").is_err());
    }

    #[test]
    fn network_args_first_at_run_rest_connected() {
        let nets = vec!["a".to_string(), "b".to_string()];
        assert_eq!(Dockerlike::DOCKER.network_run_args(&nets), vec!["--network", "a"]);
        assert!(Dockerlike::DOCKER.network_run_args(&[]).is_empty());
    }
}
