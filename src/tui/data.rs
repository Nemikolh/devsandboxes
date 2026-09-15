//! Docker/state data collection for the dashboard. Runs off the UI thread
//! (see `mod::run`), so everything here is plain blocking I/O producing a
//! [`Snapshot`] that the state machine consumes verbatim. Parsing lives in
//! free functions to stay unit-testable without docker.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

use serde::Deserialize;

use crate::config::Config;
use crate::docker::{self, NAME_PREFIX};
use crate::state::{Instance, State};

/// Container liveness, joined from `docker ps` against the state's instance list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContainerStatus {
    /// Container exists and is up. Carries docker's raw `Status` text (e.g.
    /// "Up 3 minutes").
    Running(String),
    /// Container exists but is stopped. Carries the raw `Status` text.
    Exited(String),
    /// No container by that name in `docker ps --all`.
    Missing,
}

impl ContainerStatus {
    /// Short label for the STATUS column.
    pub fn label(&self) -> &str {
        match self {
            ContainerStatus::Running(_) => "running",
            ContainerStatus::Exited(_) => "exited",
            ContainerStatus::Missing => "missing",
        }
    }
}

/// One instance row, everything the Instances view needs pre-joined.
#[derive(Clone, Debug)]
pub struct InstanceRow {
    pub name: String,
    pub sandbox: String,
    pub container: String,
    pub status: ContainerStatus,
    pub uptime_secs: u64,
    pub cpu: Option<String>,
    pub mem: Option<String>,
    pub folder: String,
    pub worktree: bool,
    pub services: Vec<String>,
    pub workspace: String,
    pub remote_user: Option<String>,
    pub remote_env_len: usize,
    pub base_folder: String,
    pub drift: bool,
}

/// A point-in-time view of instances plus any collection error (docker/config
/// unavailable). Rows are always present from state even when docker is down.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub instances: Vec<InstanceRow>,
    /// When collection finished. Reserved for staleness display (later steps);
    /// carried now so the collect/receive plumbing is stable.
    #[allow(dead_code)]
    pub collected_at: Instant,
    pub error: Option<String>,
}

/// Parsed line of `docker ps --format {{json .}}`.
#[derive(Debug, Deserialize)]
struct PsLine {
    #[serde(rename = "Names")]
    names: String,
    #[serde(rename = "Status")]
    status: String,
    #[serde(rename = "State")]
    state: String,
}

/// Parsed line of `docker stats --no-stream --format {{json .}}`.
#[derive(Debug, Deserialize)]
struct StatsLine {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "CPUPerc")]
    cpu: String,
    #[serde(rename = "MemUsage")]
    mem: String,
}

/// Classify a container by name against parsed `docker ps` lines. `docker ps`
/// may list several comma-separated names per container; match any of them.
fn classify(container: &str, ps: &[PsLine]) -> ContainerStatus {
    for line in ps {
        if line.names.split(',').any(|n| n.trim() == container) {
            // docker's State is one of created/restarting/running/removing/
            // paused/exited/dead. Treat "running" as up, everything else stopped.
            if line.state == "running" {
                return ContainerStatus::Running(line.status.clone());
            }
            return ContainerStatus::Exited(line.status.clone());
        }
    }
    ContainerStatus::Missing
}

/// Parse `docker ps --format {{json .}}` output (one JSON object per line).
/// Unparseable lines are skipped rather than failing the whole collection.
fn parse_ps(out: &str) -> Vec<PsLine> {
    out.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<PsLine>(l).ok())
        .collect()
}

/// Parse `docker stats --no-stream --format {{json .}}` output into a
/// name→(cpu, mem) map. Unparseable lines are skipped.
fn parse_stats(out: &str) -> BTreeMap<String, (String, String)> {
    out.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<StatsLine>(l).ok())
        .map(|s| (s.name, (s.cpu, s.mem)))
        .collect()
}

/// Humanize an elapsed-seconds duration: `3d4h`, `2h05m`, `12m`, `40s`.
pub fn humanize_secs(secs: u64) -> String {
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    let mins = (secs % 3_600) / 60;
    if days > 0 {
        format!("{days}d{hours}h")
    } else if hours > 0 {
        format!("{hours}h{mins:02}m")
    } else if mins > 0 {
        format!("{mins}m")
    } else {
        format!("{secs}s")
    }
}

/// Collect a fresh [`Snapshot`]. Blocking; run off the UI thread.
///
/// State is the source of truth for which rows exist; docker enriches them with
/// liveness and resource usage. When docker or config is unavailable the rows
/// still render (status `Missing`, no cpu/mem) and `error` is set.
pub fn collect(dir: &Path) -> Snapshot {
    let collected_at = Instant::now();
    let now_unix = Instance::now();

    let state = match State::load() {
        Ok(s) => s,
        Err(e) => {
            return Snapshot {
                instances: Vec::new(),
                collected_at,
                error: Some(format!("state: {e:#}")),
            };
        }
    };

    let mut errors: Vec<String> = Vec::new();

    // One `docker ps` and one `docker stats` call for the whole snapshot.
    let ps = match docker::output_quiet(&[
        "ps",
        "--all",
        "--filter",
        &format!("name=^/{NAME_PREFIX}"),
        "--format",
        "{{json .}}",
    ]) {
        Ok(out) => parse_ps(&out),
        Err(e) => {
            errors.push(format!("docker unavailable: {e:#}"));
            Vec::new()
        }
    };

    let stats = match docker::output_quiet(&["stats", "--no-stream", "--format", "{{json .}}"]) {
        Ok(out) => parse_stats(&out),
        // Only report a stats error if ps succeeded; otherwise the ps error
        // already covers "docker is down".
        Err(e) => {
            if errors.is_empty() {
                errors.push(format!("docker stats: {e:#}"));
            }
            BTreeMap::new()
        }
    };

    // Load config once; resolve each distinct sandbox for services + drift hash.
    let config = Config::load(dir);
    let mut resolved: BTreeMap<String, (Vec<String>, String)> = BTreeMap::new();
    match &config {
        Ok(cfg) => {
            for inst in state.instances.values() {
                if resolved.contains_key(&inst.sandbox) {
                    continue;
                }
                match cfg.resolve_sandbox(&inst.sandbox) {
                    Ok(rs) => {
                        let services =
                            rs.properties.services.clone().unwrap_or_default();
                        resolved.insert(inst.sandbox.clone(), (services, rs.config_hash));
                    }
                    Err(e) => errors.push(format!("sandbox `{}`: {e:#}", inst.sandbox)),
                }
            }
        }
        Err(e) => errors.push(format!("config: {e:#}")),
    }

    let mut instances = Vec::with_capacity(state.instances.len());
    for (name, inst) in &state.instances {
        let status = classify(&inst.container, &ps);
        let (cpu, mem) = match stats.get(&inst.container) {
            Some((c, m)) => (Some(c.clone()), Some(m.clone())),
            None => (None, None),
        };
        let (services, drift) = match resolved.get(&inst.sandbox) {
            Some((svcs, hash)) => (svcs.clone(), drifted(&inst.container, hash)),
            None => (Vec::new(), false),
        };

        instances.push(InstanceRow {
            name: name.clone(),
            sandbox: inst.sandbox.clone(),
            container: inst.container.clone(),
            status,
            uptime_secs: now_unix.saturating_sub(inst.created_unix),
            cpu,
            mem,
            folder: inst.folder.display().to_string(),
            worktree: inst.worktree.is_some(),
            services,
            workspace: inst.workspace.clone(),
            remote_user: inst.remote_user.clone(),
            remote_env_len: inst.remote_env.len(),
            base_folder: inst.base_folder.display().to_string(),
            drift,
        });
    }

    Snapshot {
        instances,
        collected_at,
        error: if errors.is_empty() { None } else { Some(errors.join("; ")) },
    }
}

/// True when the container's recorded config hash differs from the freshly
/// resolved one (same rule as `commands::run::warn_on_drift`). A missing label
/// or a failed inspect is treated as "no drift".
fn drifted(container: &str, expected: &str) -> bool {
    match docker::inspect(container, "{{index .Config.Labels \"devsandbox.config_hash\"}}") {
        Ok(Some(hash)) => !hash.is_empty() && hash != expected,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humanize_secs_scales() {
        assert_eq!(humanize_secs(0), "0s");
        assert_eq!(humanize_secs(40), "40s");
        assert_eq!(humanize_secs(60), "1m");
        assert_eq!(humanize_secs(12 * 60), "12m");
        assert_eq!(humanize_secs(2 * 3600 + 5 * 60), "2h05m");
        assert_eq!(humanize_secs(3 * 86_400 + 4 * 3600 + 30 * 60), "3d4h");
    }

    #[test]
    fn classify_from_ps_lines() {
        let out = concat!(
            r#"{"Names":"devsandbox-repo-abc1","Status":"Up 3 minutes","State":"running"}"#,
            "\n",
            r#"{"Names":"devsandbox-repo-xyz2","Status":"Exited (0) 1 hour ago","State":"exited"}"#,
            "\n",
            "not json\n",
        );
        let ps = parse_ps(out);
        assert_eq!(ps.len(), 2); // bad line skipped

        assert_eq!(
            classify("devsandbox-repo-abc1", &ps),
            ContainerStatus::Running("Up 3 minutes".into()),
        );
        assert_eq!(
            classify("devsandbox-repo-xyz2", &ps),
            ContainerStatus::Exited("Exited (0) 1 hour ago".into()),
        );
        assert_eq!(classify("devsandbox-nope", &ps), ContainerStatus::Missing);
    }

    #[test]
    fn classify_matches_multiname() {
        let out =
            r#"{"Names":"other-name,devsandbox-repo-abc1","Status":"Up 1s","State":"running"}"#;
        let ps = parse_ps(out);
        assert_eq!(
            classify("devsandbox-repo-abc1", &ps),
            ContainerStatus::Running("Up 1s".into()),
        );
    }

    #[test]
    fn stats_parse_maps_by_name() {
        let out = concat!(
            r#"{"Name":"devsandbox-repo-abc1","CPUPerc":"1.20%","MemUsage":"50MiB / 2GiB"}"#,
            "\n",
            "garbage\n",
        );
        let stats = parse_stats(out);
        assert_eq!(
            stats.get("devsandbox-repo-abc1"),
            Some(&("1.20%".to_string(), "50MiB / 2GiB".to_string())),
        );
        assert_eq!(stats.len(), 1);
    }
}
