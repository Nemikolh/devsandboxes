//! Whether the runtime can give a container NVIDIA GPUs, and with which
//! `run` flags (docs/architecture/runtimes.md, *GPUs*). Pure over the
//! engine's `info` and the CDI spec files' contents; the one read of spec
//! files is [`read_cdi_dir`].

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::de::IgnoredAny;
use serde::Deserialize;

/// The CDI kind NVIDIA's `nvidia-ctk cdi generate` writes.
const NVIDIA_KIND: &str = "nvidia.com/gpu";

/// Where podman reads CDI specs by default (containers.conf
/// `cdi_spec_dirs`).
pub const PODMAN_CDI_DIRS: [&str; 2] = ["/etc/cdi", "/var/run/cdi"];

/// How a container gets every NVIDIA GPU on this runtime, or why it can't.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GpuSupport {
    /// docker's `--gpus all`: the `nvidia` runtime is registered.
    Gpus,
    /// `--device nvidia.com/gpu=all`: a CDI spec for NVIDIA GPUs.
    Cdi,
    /// No way to a GPU; the reason, for the user.
    None(String),
}

impl GpuSupport {
    /// The `run` flags that give the container every GPU.
    pub fn run_args(&self) -> Vec<String> {
        let args: &[&str] = match self {
            GpuSupport::Gpus => &["--gpus", "all"],
            GpuSupport::Cdi => &["--device", "nvidia.com/gpu=all"],
            GpuSupport::None(_) => &[],
        };
        args.iter().map(|a| a.to_string()).collect()
    }
}

/// The fields of `docker info --format '{{json .}}'` GPU support reads.
#[derive(Debug, Deserialize)]
struct DockerGpuInfo {
    #[serde(rename = "Runtimes", default)]
    runtimes: Option<BTreeMap<String, IgnoredAny>>,
    /// Empty or missing when the daemon has CDI off.
    #[serde(rename = "CDISpecDirs", default)]
    cdi_spec_dirs: Option<Vec<String>>,
}

/// A CDI spec file's fields read here (YAML or JSON).
#[derive(Debug, Deserialize)]
struct CdiSpec {
    kind: String,
    #[serde(default)]
    devices: Vec<CdiDevice>,
}

#[derive(Debug, Deserialize)]
struct CdiDevice {
    #[serde(default)]
    name: String,
}

/// The NVIDIA specs among a dir's files; a file that doesn't parse as a
/// spec is skipped, as the engine would.
fn nvidia_specs<P: AsRef<Path>>(dirs: &[P], cdi: &impl Fn(&Path) -> Vec<String>) -> Vec<CdiSpec> {
    dirs.iter()
        .flat_map(|dir| cdi(dir.as_ref()))
        .filter_map(|text| serde_saphyr::from_str::<CdiSpec>(&text).ok())
        .filter(|spec| spec.kind == NVIDIA_KIND)
        .collect()
}

fn dir_list<P: AsRef<Path>>(dirs: &[P]) -> String {
    dirs.iter().map(|d| d.as_ref().display().to_string()).collect::<Vec<_>>().join(", ")
}

/// docker, from `docker info --format '{{json .}}'`: `Gpus` when
/// `Runtimes` has `nvidia` (the devcontainers CLI's own check), else `Cdi`
/// when `native` (the engine is this host's kernel, `EngineInfo::native`)
/// and a spec of kind `nvidia.com/gpu` sits in one of `CDISpecDirs`. A
/// non-native engine's spec dirs are on its own host, unreadable here.
/// `cdi` returns the spec files' contents in a dir ([`read_cdi_dir`]).
pub fn docker(info_json: &str, native: bool, cdi: impl Fn(&Path) -> Vec<String>) -> anyhow::Result<GpuSupport> {
    let info: DockerGpuInfo = serde_json::from_str(info_json.trim())
        .map_err(|e| anyhow::anyhow!("unexpected `docker info` output: {e}"))?;
    if info.runtimes.unwrap_or_default().contains_key("nvidia") {
        return Ok(GpuSupport::Gpus);
    }
    if !native {
        return Ok(GpuSupport::None(
            "docker has no `nvidia` runtime, and its engine isn't on this host's kernel, so its CDI specs can't be read"
                .into(),
        ));
    }
    let dirs = info.cdi_spec_dirs.unwrap_or_default();
    if dirs.is_empty() {
        return Ok(GpuSupport::None("docker has no `nvidia` runtime and CDI is off (no `CDISpecDirs`)".into()));
    }
    if nvidia_specs(&dirs, &cdi).is_empty() {
        return Ok(GpuSupport::None(format!(
            "docker has no `nvidia` runtime and no CDI spec for `{NVIDIA_KIND}` in {}",
            dir_list(&dirs)
        )));
    }
    Ok(GpuSupport::Cdi)
}

/// podman: `Cdi` when `native` and a spec of kind `nvidia.com/gpu` sits
/// in [`PODMAN_CDI_DIRS`]; podman has no `--gpus` runtime of its own.
pub fn podman(native: bool, cdi: impl Fn(&Path) -> Vec<String>) -> GpuSupport {
    if !native {
        return GpuSupport::None("podman's engine isn't on this host's kernel, so its CDI specs can't be read".into());
    }
    if nvidia_specs(&PODMAN_CDI_DIRS, &cdi).is_empty() {
        return GpuSupport::None(format!(
            "podman has no CDI spec for `{NVIDIA_KIND}` in {}",
            dir_list(&PODMAN_CDI_DIRS)
        ));
    }
    GpuSupport::Cdi
}

/// The GPUs the NVIDIA CDI specs in `dirs` name: distinct device names
/// but `all` (which every generated spec adds). A spec generated with
/// both index and UUID names counts each GPU twice.
#[allow(dead_code)] // the capacity report's, later
pub fn cdi_gpu_count<P: AsRef<Path>>(dirs: &[P], cdi: impl Fn(&Path) -> Vec<String>) -> usize {
    let names: BTreeSet<String> = nvidia_specs(dirs, &cdi)
        .into_iter()
        .flat_map(|spec| spec.devices)
        .map(|d| d.name)
        .filter(|n| !n.is_empty() && n != "all")
        .collect();
    names.len()
}

/// The contents of the `*.yaml`, `*.yml` and `*.json` files in `dir`, in
/// name order; a missing dir or unreadable file is none.
pub fn read_cdi_dir(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| matches!(p.extension().and_then(|e| e.to_str()), Some("yaml" | "yml" | "json")))
        .collect();
    paths.sort();
    paths.iter().filter_map(|p| std::fs::read_to_string(p).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NVIDIA_YAML: &str = "cdiVersion: 0.5.0\nkind: nvidia.com/gpu\ndevices:\n  - name: \"0\"\n    containerEdits: {}\n  - name: \"1\"\n    containerEdits: {}\n  - name: all\n    containerEdits: {}\ncontainerEdits: {}\n";
    const NVIDIA_JSON: &str = r#"{"cdiVersion":"0.6.0","kind":"nvidia.com/gpu","devices":[{"name":"1"},{"name":"2"},{"name":"all"}]}"#;
    const OTHER_YAML: &str = "cdiVersion: 0.5.0\nkind: vendor.com/fpga\ndevices:\n  - name: \"0\"\n";

    fn info(runtimes: &str, dirs: &str) -> String {
        format!(r#"{{"Runtimes":{runtimes},"CDISpecDirs":{dirs},"NCPU":8}}"#)
    }

    const RUNC: &str = r#"{"runc":{"path":"runc"}}"#;
    const WITH_NVIDIA: &str = r#"{"runc":{"path":"runc"},"nvidia":{"path":"nvidia-container-runtime"}}"#;
    const DIRS: &str = r#"["/etc/cdi","/var/run/cdi"]"#;

    /// Spec files by dir.
    fn fs(files: &[(&str, &str)]) -> impl Fn(&Path) -> Vec<String> {
        let files: Vec<(String, String)> = files.iter().map(|(d, t)| (d.to_string(), t.to_string())).collect();
        move |dir| files.iter().filter(|(d, _)| Path::new(d) == dir).map(|(_, t)| t.clone()).collect()
    }

    fn reason(s: GpuSupport) -> String {
        match s {
            GpuSupport::None(why) => why,
            other => panic!("expected None, got {other:?}"),
        }
    }

    #[test]
    fn run_args_per_support() {
        assert_eq!(GpuSupport::Gpus.run_args(), ["--gpus", "all"]);
        assert_eq!(GpuSupport::Cdi.run_args(), ["--device", "nvidia.com/gpu=all"]);
        assert!(GpuSupport::None("x".into()).run_args().is_empty());
    }

    #[test]
    fn docker_nvidia_runtime_wins_native_or_not() {
        for native in [true, false] {
            assert_eq!(docker(&info(WITH_NVIDIA, "[]"), native, fs(&[])).unwrap(), GpuSupport::Gpus);
        }
        let only = format!(r#"{{"Runtimes":{WITH_NVIDIA}}}"#);
        assert_eq!(docker(&only, true, fs(&[("/etc/cdi", NVIDIA_YAML)])).unwrap(), GpuSupport::Gpus);
    }

    #[test]
    fn docker_cdi_spec_on_a_native_engine() {
        let text = info(RUNC, DIRS);
        assert_eq!(docker(&text, true, fs(&[("/etc/cdi", NVIDIA_YAML)])).unwrap(), GpuSupport::Cdi);
        assert_eq!(docker(&text, true, fs(&[("/var/run/cdi", NVIDIA_JSON)])).unwrap(), GpuSupport::Cdi);
        // Only the dirs docker reports are read.
        let custom = info(RUNC, r#"["/opt/cdi"]"#);
        assert_eq!(docker(&custom, true, fs(&[("/opt/cdi", NVIDIA_YAML)])).unwrap(), GpuSupport::Cdi);
        let why = reason(docker(&custom, true, fs(&[("/etc/cdi", NVIDIA_YAML)])).unwrap());
        assert!(why.contains("in /opt/cdi"), "{why}");
    }

    #[test]
    fn docker_cdi_unreadable_off_a_native_engine() {
        let why = reason(docker(&info(RUNC, DIRS), false, fs(&[("/etc/cdi", NVIDIA_YAML)])).unwrap());
        assert!(why.contains("isn't on this host's kernel"), "{why}");
    }

    #[test]
    fn docker_nothing_usable() {
        let why = reason(docker(&info(RUNC, DIRS), true, fs(&[])).unwrap());
        assert_eq!(why, "docker has no `nvidia` runtime and no CDI spec for `nvidia.com/gpu` in /etc/cdi, /var/run/cdi");
        // Other kinds and malformed files don't count.
        let others = fs(&[("/etc/cdi", OTHER_YAML), ("/etc/cdi", "kind: [unclosed"), ("/etc/cdi", "devices: []\n")]);
        assert!(matches!(docker(&info(RUNC, DIRS), true, others).unwrap(), GpuSupport::None(_)));
        // A malformed file beside a good one is skipped.
        let mixed = fs(&[("/etc/cdi", "{{{"), ("/etc/cdi", NVIDIA_YAML)]);
        assert_eq!(docker(&info(RUNC, DIRS), true, mixed).unwrap(), GpuSupport::Cdi);
        // CDI off: no dirs at all.
        for dirs in ["[]", "null"] {
            let why = reason(docker(&info(RUNC, dirs), true, fs(&[("/etc/cdi", NVIDIA_YAML)])).unwrap());
            assert!(why.contains("CDI is off"), "{why}");
        }
        assert!(matches!(docker("{}", true, fs(&[])).unwrap(), GpuSupport::None(_)));
    }

    #[test]
    fn docker_malformed_info_is_an_error() {
        assert!(docker("Cannot connect to the Docker daemon", true, fs(&[])).is_err());
        assert!(docker(r#"{"Runtimes":["runc"]}"#, true, fs(&[])).is_err());
    }

    #[test]
    fn podman_reads_its_default_dirs() {
        assert_eq!(podman(true, fs(&[("/etc/cdi", NVIDIA_YAML)])), GpuSupport::Cdi);
        assert_eq!(podman(true, fs(&[("/var/run/cdi", NVIDIA_JSON)])), GpuSupport::Cdi);
        let why = reason(podman(true, fs(&[("/etc/cdi", OTHER_YAML), ("/etc/cdi", "not: [yaml")])));
        assert_eq!(why, "podman has no CDI spec for `nvidia.com/gpu` in /etc/cdi, /var/run/cdi");
        assert!(matches!(podman(true, fs(&[("/opt/cdi", NVIDIA_YAML)])), GpuSupport::None(_)));
        let why = reason(podman(false, fs(&[("/etc/cdi", NVIDIA_YAML)])));
        assert!(why.contains("isn't on this host's kernel"), "{why}");
    }

    #[test]
    fn cdi_gpu_count_counts_distinct_named_devices() {
        assert_eq!(cdi_gpu_count(&PODMAN_CDI_DIRS, fs(&[("/etc/cdi", NVIDIA_YAML)])), 2);
        // The same GPU in two specs counts once; `all` and other kinds never.
        let both = fs(&[("/etc/cdi", NVIDIA_YAML), ("/var/run/cdi", NVIDIA_JSON), ("/etc/cdi", OTHER_YAML)]);
        assert_eq!(cdi_gpu_count(&PODMAN_CDI_DIRS, both), 3);
        assert_eq!(cdi_gpu_count(&PODMAN_CDI_DIRS, fs(&[])), 0);
    }

    #[test]
    fn read_cdi_dir_reads_spec_extensions_in_order() {
        let dir = std::env::temp_dir().join(format!("devsandbox-gpu-cdi-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (name, text) in [("b.yaml", "b"), ("a.json", "a"), ("c.yml", "c"), ("d.txt", "d")] {
            std::fs::write(dir.join(name), text).unwrap();
        }
        std::fs::create_dir(dir.join("e.yaml")).unwrap();
        assert_eq!(read_cdi_dir(&dir), ["a", "b", "c"]);
        assert!(read_cdi_dir(&dir.join("missing")).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
