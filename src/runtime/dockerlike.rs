//! Docker-CLI-compatible runtimes: `docker` and `podman`. Same subcommands and
//! flags, Go-template `--format`, `--filter`, `network connect`,
//! `--network-alias`, `top`. Podman's `ps --format {{json .}}` differs slightly
//! in field types (arrays/maps where docker emits strings); the parsers accept
//! both.

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use super::identity::{self, Canary, FastPath, Ids};
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

    /// No `event=` filter: docker says `die` where podman says `died`, so
    /// [`super::events::relevant`] whitelists instead.
    fn events_argv(&self) -> Option<Vec<&'static str>> {
        // Podman's `--format` takes `json`, not a Go template of it.
        let format = if self.name == "podman" { "json" } else { "{{json .}}" };
        Some(vec!["events", "--filter", "type=container", "--format", format])
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
        args.extend(["--format", PS_FORMAT]);
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

    fn supports_restart_policy(&self) -> bool {
        true
    }

    fn restart_policy(&self, container: &str) -> Result<Option<String>> {
        self.inspect_template(container, "{{.HostConfig.RestartPolicy.Name}}")
    }

    fn set_restart_policy(&self, container: &str, policy: &str) -> Result<()> {
        self.output_quiet(&["update", "--restart", policy, container])?;
        Ok(())
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

    fn image_user(&self, image: &str) -> Result<Option<String>> {
        let out = self.output_quiet(&["image", "inspect", "--format", "{{json .Config.User}}", image])?;
        parse_image_user(&out)
    }

    fn image_env(&self, image: &str) -> Result<Vec<(String, String)>> {
        let out = self.output_quiet(&["image", "inspect", "--format", "{{json .Config.Env}}", image])?;
        parse_image_env(&out)
    }

    fn image_id(&self, image: &str) -> Result<Option<String>> {
        let out = self.output_quiet(&["image", "inspect", "--format", "{{.Id}}", image])?;
        Ok(Some(out).filter(|id| !id.is_empty()))
    }

    fn image_passwd(&self, image: &str) -> Result<Option<String>> {
        let args = passwd_argv(image);
        let out = std::process::Command::new(self.bin)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .output()
            .with_context(|| format!("failed to run {} (is it installed?)", self.bin))?;
        let stderr = String::from_utf8_lossy(&out.stderr);
        passwd_outcome(out.status.code(), &String::from_utf8_lossy(&out.stdout), &stderr)
            .with_context(|| format!("cannot read /etc/passwd from `{image}`"))
    }

    fn remove_volume(&self, name: &str) -> Result<bool> {
        match self.output_quiet(&["volume", "rm", name]) {
            Ok(_) => Ok(true),
            Err(e) if super::is_missing_volume(&e.to_string()) => Ok(false),
            Err(e) => Err(e),
        }
    }

    fn identity_fast_path(&self) -> Result<FastPath> {
        let linux = cfg!(target_os = "linux");
        let var = |key| std::env::var(key).ok().filter(|v| !v.is_empty());
        if self.name == "podman" {
            // Podman's `--format` takes `json`, not a Go template of it.
            let out = self.output_quiet(&["info", "--format", "json"])?;
            identity::podman_fast_path(&out, var("CONTAINER_HOST").as_deref(), linux)
        } else {
            let out = self.output_quiet(&["info", "--format", "{{json .}}"])?;
            identity::docker_fast_path(&out, var("DOCKER_HOST").as_deref(), linux)
        }
    }

    fn identity_canary(&self, image: &str, dir: &std::path::Path, relabel: bool) -> Result<Canary> {
        let host_dir = dir.to_str().context("identity canary dir is not UTF-8")?;
        // `-v`'s separators; `--mount` would take them but has no relabel.
        if host_dir.contains([':', ',']) {
            bail!("identity canary dir `{host_dir}` has a `:` or `,`");
        }
        let root = Ids { uid: 0, gid: 0 };
        for user in [root, identity::ASSUMED] {
            let args = canary_argv(image, host_dir, user, relabel);
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            self.output_quiet(&refs)
                .with_context(|| format!("identity canary as uid {} from `{image}` (it needs {})", user.uid, super::SH))?;
        }
        Ok(Canary { as_root: owner(&dir.join("0"))?, as_1000: owner(&dir.join(identity::ASSUMED.uid.to_string()))? })
    }
}

/// Where the canary's host dir is bound inside.
const CANARY_TARGET: &str = "/devsandbox-probe";

/// One canary run: a throwaway container from the local `image` (never
/// pulled, no network, entrypoint replaced) creating `<uid>` in the bound
/// `host_dir` as `user`. `:z` relabels the bind for SELinux.
fn canary_argv(image: &str, host_dir: &str, user: Ids, relabel: bool) -> Vec<String> {
    let relabel = if relabel { ":z" } else { "" };
    vec![
        "run".into(),
        "--rm".into(),
        "--pull=never".into(),
        "--network=none".into(),
        "--user".into(),
        format!("{}:{}", user.uid, user.gid),
        "--entrypoint".into(),
        super::SH.into(),
        "-v".into(),
        format!("{host_dir}:{CANARY_TARGET}{relabel}"),
        image.into(),
        "-c".into(),
        // A builtin and a redirect: no tool needed from the image.
        format!(": > {CANARY_TARGET}/{}", user.uid),
    ]
}

/// `Backend::image_passwd`'s run: like the canary (local image only, no
/// network, `sh` for the entrypoint), as root so any image's file is
/// readable.
fn passwd_argv(image: &str) -> Vec<String> {
    ["run", "--rm", "--pull=never", "--network=none", "--user", "0:0", "--entrypoint", super::SH, image, "-c", "cat /etc/passwd"]
        .map(String::from)
        .into()
}

/// The passwd run's outcome: stdout on success; `None` on 126/127 (no `sh`
/// to start, or no `cat` in it: the runtime and `sh` both say so that way);
/// any other failure is an error with stderr's first line.
fn passwd_outcome(code: Option<i32>, stdout: &str, stderr: &str) -> Result<Option<String>> {
    match code {
        Some(0) => Ok(Some(stdout.to_string())),
        Some(126 | 127) => Ok(None),
        _ => bail!("{}", stderr.lines().next().unwrap_or("non-zero exit").trim()),
    }
}

/// A canary file's owner, as the host sees it.
#[cfg(unix)]
fn owner(path: &std::path::Path) -> Result<Ids> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(path).with_context(|| {
        format!(
            "identity canary file {} isn't on this host: the engine sees another filesystem \
             (a remote engine, or a VM share not covering it)",
            path.display()
        )
    })?;
    Ok(Ids { uid: meta.uid(), gid: meta.gid() })
}

#[cfg(not(unix))]
fn owner(_path: &std::path::Path) -> Result<Ids> {
    bail!("the identity canary needs a unix host")
}

/// `{{json .Config.Entrypoint}}`: `null` (unset) or a string array.
fn parse_entrypoint(out: &str) -> Result<Vec<String>> {
    let value: Option<Vec<String>> = serde_json::from_str(out.trim())
        .with_context(|| format!("unexpected image entrypoint `{}`", out.trim()))?;
    Ok(value.unwrap_or_default())
}

/// `{{json .Config.User}}`: a JSON string, empty (or `null`) when unset.
fn parse_image_user(out: &str) -> Result<Option<String>> {
    let value: Option<String> = serde_json::from_str(out.trim())
        .with_context(|| format!("unexpected image user `{}`", out.trim()))?;
    Ok(value.filter(|u| !u.is_empty()))
}

/// `{{json .Config.Env}}`: a JSON list of `KEY=VALUE`, `null` when unset.
fn parse_image_env(out: &str) -> Result<Vec<(String, String)>> {
    let value: Option<Vec<String>> = serde_json::from_str(out.trim())
        .with_context(|| format!("unexpected image env `{}`", out.trim()))?;
    Ok(super::split_env(value.unwrap_or_default()))
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

/// `ps --format` template naming only the [`PsLine`] fields. Never `{{json .}}`:
/// a template that touches `.Size` (as `{{json .}}` does) makes the docker CLI
/// request `size=1`, so the daemon computes every listed container's disk usage
/// on each call (~300ms and a dockerd CPU spike per TUI tick, vs ~20ms).
const PS_FORMAT: &str = concat!(
    r#"{"Names":{{json .Names}},"Image":{{json .Image}},"Status":{{json .Status}},"#,
    r#""State":{{json .State}},"Labels":{{json .Labels}},"Ports":{{json .Ports}}}"#,
);

/// Parse `ps --format` [`PS_FORMAT`] output (one JSON object per line).
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

    /// Every field `PsLine` reads, and nothing that triggers `size=1`.
    #[test]
    fn ps_format_names_fields_without_size() {
        for field in ["Names", "Image", "Status", "State", "Labels", "Ports"] {
            assert!(PS_FORMAT.contains(&format!("\"{field}\":{{{{json .{field}}}}}")), "{field}");
        }
        assert!(!PS_FORMAT.contains("Size") && !PS_FORMAT.contains("json .}}"));
    }

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
    fn parse_image_env_splits_at_the_first_equals() {
        // Captured from `docker image inspect --format '{{json .Config.Env}}' alpine:3.20`, plus one.
        let out = "[\"PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin\",\"OPTS=a=b\"]\n";
        assert_eq!(
            parse_image_env(out).unwrap(),
            [
                ("PATH".to_string(), "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string()),
                ("OPTS".into(), "a=b".into())
            ]
        );
        assert!(parse_image_env("null").unwrap().is_empty());
        assert!(parse_image_env("garbage").is_err());
    }

    #[test]
    fn parse_image_user_empty_is_none() {
        assert_eq!(parse_image_user("\"vscode\"\n").unwrap().as_deref(), Some("vscode"));
        assert_eq!(parse_image_user("\"1000:1000\"").unwrap().as_deref(), Some("1000:1000"));
        assert_eq!(parse_image_user("\"\"").unwrap(), None);
        assert_eq!(parse_image_user("null").unwrap(), None);
        assert!(parse_image_user("garbage").is_err());
    }

    /// Docker-gated: a removed volume reports `true`, a second removal hits the
    /// real "no such volume" error and reports `false` instead of failing.
    #[test_utils::docker_test]
    fn remove_volume_reports_missing() -> Result<(), &'static str> {
        let name = format!(
            "devsandbox-vol-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        crate::runtime::backend().run_checked(&["volume", "create", &name]).map_err(|_| "volume create failed")?;
        assert!(crate::runtime::backend().remove_volume(&name).unwrap());
        assert!(!crate::runtime::backend().remove_volume(&name).unwrap());
        Ok(())
    }

    #[test]
    fn canary_argv_per_user() {
        let root = canary_argv("devsandbox-web:abc", "/home/u/.local/share/devsandbox/identity-probe-7", Ids { uid: 0, gid: 0 }, false);
        assert_eq!(
            root,
            [
                "run", "--rm", "--pull=never", "--network=none", "--user", "0:0", "--entrypoint", "/bin/sh", "-v",
                "/home/u/.local/share/devsandbox/identity-probe-7:/devsandbox-probe", "devsandbox-web:abc", "-c",
                ": > /devsandbox-probe/0",
            ]
        );
        let user = canary_argv("img", "/d", identity::ASSUMED, true);
        assert_eq!(user[5], "1000:1000");
        assert_eq!(user[9], "/d:/devsandbox-probe:z");
        assert_eq!(user[12], ": > /devsandbox-probe/1000");
    }

    #[test]
    fn passwd_run_and_outcome() {
        assert_eq!(
            passwd_argv("img"),
            ["run", "--rm", "--pull=never", "--network=none", "--user", "0:0", "--entrypoint", "/bin/sh", "img", "-c", "cat /etc/passwd"]
        );
        assert_eq!(passwd_outcome(Some(0), "root:x:0:0::/root:/bin/sh\n", "").unwrap().as_deref(), Some("root:x:0:0::/root:/bin/sh\n"));
        // No `sh` (the runtime's 127/126) or no `cat` (sh's 127): nothing to read.
        assert_eq!(passwd_outcome(Some(127), "", "exec: \"/bin/sh\": stat /bin/sh: no such file").unwrap(), None);
        assert_eq!(passwd_outcome(Some(126), "", "").unwrap(), None);
        let err = passwd_outcome(Some(125), "", "Error: No such image: img\nmore").unwrap_err();
        assert_eq!(err.to_string(), "Error: No such image: img");
        assert!(passwd_outcome(None, "", "").is_err());
    }

    /// Docker-gated: alpine's passwd through a throwaway container, and an id
    /// to key it by. Runs under rootless docker too (no bind involved).
    #[test_utils::docker_test]
    fn image_passwd_and_id_from_alpine() -> Result<(), &'static str> {
        let image = "alpine:3.20";
        crate::runtime::backend().output_quiet(&["pull", "-q", image]).map_err(|_| "cannot pull alpine:3.20")?;
        let passwd = crate::runtime::backend().image_passwd(image).unwrap().expect("alpine has sh");
        assert_eq!(identity::account(&passwd, "root").map(|a| (a.uid, a.home)), Some((0, "/root".into())));
        let id = crate::runtime::backend().image_id(image).unwrap().expect("an id");
        // docker says `sha256:<hex>`, podman the bare hex.
        let hex = id.strip_prefix("sha256:").unwrap_or(&id);
        assert!(hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()), "{id}");
        assert!(crate::runtime::backend().image_id("devsandbox-no-such-image:never").is_err());
        Ok(())
    }

    /// Runtime-gated: the canary agrees with the fast path (CI: rootful
    /// docker, rootless podman). Unverified on a dev Mac (no docker there).
    #[cfg(unix)]
    #[test_utils::docker_test]
    fn canary_agrees_with_the_fast_path() -> Result<(), &'static str> {
        use std::os::unix::fs::PermissionsExt;
        let rt = crate::runtime::backend();
        let expected = rt.identity_fast_path().map_err(|_| "no fast path")?.mode.map_err(|_| "fast path ambiguous")?;
        let image = "alpine:3.20";
        crate::runtime::backend().output_quiet(&["pull", "-q", image]).map_err(|_| "cannot pull alpine:3.20")?;
        let dir = std::env::temp_dir().join(format!("devsandbox-canary-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        let canary = crate::test_support::with_cleanup(
            || drop(std::fs::remove_dir_all(&dir)),
            || crate::runtime::backend().identity_canary(image, &dir, false),
        )
        .unwrap();
        let host = Ids { uid: unsafe { libc::getuid() }, gid: unsafe { libc::getgid() } };
        let engine = if rt.name() == "podman" { identity::Engine::Podman } else { identity::Engine::Docker };
        let mode = identity::classify(host, canary.as_root, canary.as_1000, engine);
        assert_eq!(mode, Some(expected), "{canary:?}");
        Ok(())
    }

    #[test]
    fn network_args_first_at_run_rest_connected() {
        let nets = vec!["a".to_string(), "b".to_string()];
        assert_eq!(Dockerlike::DOCKER.network_run_args(&nets), vec!["--network", "a"]);
        assert!(Dockerlike::DOCKER.network_run_args(&[]).is_empty());
    }
}
