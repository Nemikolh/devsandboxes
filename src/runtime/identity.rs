//! Who a sandbox runs as, per runtime mode (docs/architecture/instances/identity.md,
//! *The model*, *The plan*). The same devcontainer user fields mean opposite things on
//! rootful docker, rootless docker, rootless podman and VM-backed runtimes, so
//! the user model is decided here from the probed mode instead of taken
//! literally: [`classify`] turns the canary's two host-side `stat`s into a
//! [`Mode`], and [`plan`] maps (mode, [`Identity`], adopted account) to a
//! [`Plan`]. The probe's fast path reads the runtime's own metadata
//! ([`docker_fast_path`], [`podman_fast_path`]) and its cache is keyed by
//! [`context_key`]. Pure: no I/O and no `backend()`, so every row is
//! table-tested; running the commands and the canary, and applying the plan,
//! belong to the callers (docs/architecture/runtimes.md, *Mode detection*).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The user-namespace situation of a runtime context, as the probe found it.
/// Serialized as [`Mode::as_str`] (the probe cache in `state.toml`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// uid 0 inside is host root, uid 1000 inside is host 1000.
    Rootful,
    /// uid 0 inside is the host user; every other uid is a subuid.
    RootlessDocker,
    /// Same mapping as rootless docker, but podman's `keep-id` can map the
    /// host user onto a non-root uid.
    RootlessPodman,
    /// A VM's file share translates ownership (Docker Desktop, OrbStack,
    /// colima, podman machine, Apple `container`).
    #[serde(rename = "vm")]
    VmBacked,
    /// The probe failed: never cached, only recorded on a fallback plan
    /// (`identity = "image"` semantics, nothing relabeled).
    Unknown,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Rootful => "rootful",
            Mode::RootlessDocker => "rootless-docker",
            Mode::RootlessPodman => "rootless-podman",
            Mode::VmBacked => "vm",
            Mode::Unknown => "unknown",
        }
    }

    #[cfg_attr(not(test), allow(dead_code))] // for `Plan::parse_label`
    fn parse(s: &str) -> Option<Mode> {
        [Mode::Rootful, Mode::RootlessDocker, Mode::RootlessPodman, Mode::VmBacked, Mode::Unknown]
            .into_iter()
            .find(|m| m.as_str() == s)
    }

    /// How [`Plan::summary`] names the mode.
    fn describe(self) -> &'static str {
        match self {
            Mode::Rootful => "rootful",
            Mode::RootlessDocker => "rootless docker",
            Mode::RootlessPodman => "rootless podman",
            Mode::VmBacked => "VM-backed",
            Mode::Unknown => "unknown mode",
        }
    }
}

/// A probe's result for one runtime context (cached in `state.toml`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Probe {
    pub mode: Mode,
    /// SELinux is enforcing: binds need relabeling or every access is EACCES.
    pub selinux: bool,
}

/// The sandbox's `identity` field: what the user wants, as opposed to the
/// devcontainer fields' mechanism.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Identity {
    /// Inside you are the adopted account, with the host's uid/gid.
    #[default]
    Host,
    /// Users taken literally, no remapping (the pre-identity behavior).
    Image,
}

impl Identity {
    fn as_str(self) -> &'static str {
        match self {
            Identity::Host => "host",
            Identity::Image => "image",
        }
    }

    #[cfg_attr(not(test), allow(dead_code))] // for `Plan::parse_label`
    fn parse(s: &str) -> Option<Identity> {
        match s {
            "host" => Some(Identity::Host),
            "image" => Some(Identity::Image),
            _ => None,
        }
    }
}

/// A uid/gid pair: the host user's, or a file's owner as `stat` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ids {
    pub uid: u32,
    pub gid: u32,
}

/// The ids the adopted account is assumed to have when the image's
/// `/etc/passwd` can't be read (*Assumed ids*): what devcontainer base images
/// and common-utils' `automatic` user use. The canary writes its non-root
/// file as this uid too.
pub const ASSUMED: Ids = Ids { uid: 1000, gid: 1000 };

/// Which rootless runtime ran the canary: the stats alone can't tell rootless
/// docker from rootless podman, the backend's name can.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Docker,
    Podman,
}

/// Classify the canary: `as_root` and `as_1000` are the host-side owners of
/// the files written inside as uid 0 and uid 1000. Only uids are compared;
/// gids follow them and a squashing share may not translate groups the same
/// way. `None` when the outcome matches no known mode.
pub fn classify(host: Ids, as_root: Ids, as_1000: Ids, engine: Engine) -> Option<Mode> {
    if as_root.uid == 0 && as_1000.uid == ASSUMED.uid {
        Some(Mode::Rootful)
    } else if as_root.uid == host.uid && as_1000.uid == host.uid {
        // Everything squashed onto the host user: a VM's file share.
        Some(Mode::VmBacked)
    } else if as_root.uid == host.uid && as_1000.uid != 0 {
        // Root is you, 1000 a subuid.
        Some(match engine {
            Engine::Docker => Mode::RootlessDocker,
            Engine::Podman => Mode::RootlessPodman,
        })
    } else {
        None
    }
}

/// The canary's two host-side `stat`s: the owners of the files written inside
/// as uid 0 and as uid [`ASSUMED`] (fed to [`classify`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Canary {
    pub as_root: Ids,
    pub as_1000: Ids,
}

/// What the runtime's own metadata says (the probe's fast path).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FastPath {
    /// The mode, or why the metadata can't tell (the canary decides).
    pub mode: Result<Mode, &'static str>,
    /// The engine labels containers for SELinux: binds need relabeling. The
    /// canary doesn't test this, so it holds even when `mode` is ambiguous.
    pub selinux: bool,
}

/// The fields of `docker info --format '{{json .}}'` the fast path reads.
#[derive(Debug, Deserialize)]
struct DockerInfo {
    #[serde(rename = "SecurityOptions", default)]
    security_options: Option<Vec<String>>,
    #[serde(rename = "OperatingSystem", default)]
    operating_system: String,
    /// The engine host's hostname: `colima`, `lima-rancher-desktop`, …
    #[serde(rename = "Name", default)]
    name: String,
    /// Only in newer CLIs' output.
    #[serde(rename = "ClientInfo", default)]
    client_info: Option<DockerClientInfo>,
    /// The engine host's cpus and memory (a VM's, for a VM-backed engine).
    #[serde(rename = "NCPU", default)]
    ncpu: Option<u64>,
    #[serde(rename = "MemTotal", default)]
    mem_total: Option<u64>,
    /// Where the engine keeps images and containers, on the engine host.
    #[serde(rename = "DockerRootDir", default)]
    root_dir: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DockerClientInfo {
    #[serde(rename = "Context", default)]
    context: String,
}

/// `OperatingSystem` values of VM-backed engines.
const VM_OPERATING_SYSTEMS: [&str; 3] = ["Docker Desktop", "OrbStack", "Rancher Desktop"];
/// Engine hostnames (`Name`) and context names of VM-backed engines, exact or
/// as a `<marker>-` prefix (`colima-work`, `lima-rancher-desktop`).
const VM_NAMES: [&str; 6] = ["colima", "lima", "orbstack", "docker-desktop", "desktop-linux", "rancher-desktop"];
/// Socket paths of VM-backed engines, as found in `DOCKER_HOST`.
const VM_SOCKET_DIRS: [&str; 6] = ["/.colima/", "/.lima/", "/.rd/", "/.orbstack/", "/.docker/desktop/", "/.docker/run/"];

fn vm_name(name: &str) -> bool {
    VM_NAMES.iter().any(|m| name == *m || name.strip_prefix(m).is_some_and(|rest| rest.starts_with('-')))
}

/// An endpoint on another host: binds name that host's paths, so neither the
/// metadata nor this host's view of a bind says anything. Left to the canary,
/// which then fails (its files never show up here).
fn remote_endpoint(endpoint: Option<&str>) -> bool {
    endpoint.is_some_and(|e| e.starts_with("tcp://") || e.starts_with("ssh://"))
}

/// The fast path over `docker info --format '{{json .}}'`. `endpoint` is
/// `DOCKER_HOST` when set; `linux_host` whether devsandbox runs on Linux
/// (anywhere else a local engine is in a VM). In order:
/// - a remote endpoint (`tcp://`, `ssh://`): ambiguous;
/// - not Linux, or a VM marker (`OperatingSystem` Docker Desktop / OrbStack /
///   Rancher Desktop, a colima/lima/… hostname or context, a VM's socket
///   path): `VmBacked`;
/// - `name=rootless` in `SecurityOptions`: `RootlessDocker`;
/// - `name=userns` (a `userns-remap` daemon: root is a subuid, uid 1000
///   another): ambiguous;
/// - otherwise `Rootful`: a local daemon with no user namespace.
///
/// `selinux` is `name=selinux` (the daemon's `selinux-enabled`; without it
/// docker doesn't label containers and binds need no relabel).
pub fn docker_fast_path(info: &str, endpoint: Option<&str>, linux_host: bool) -> anyhow::Result<FastPath> {
    let info = parse_docker_info(info)?;
    let options: Vec<&str> = info
        .security_options
        .iter()
        .flatten()
        .filter_map(|o| o.split(',').find_map(|kv| kv.strip_prefix("name=")))
        .collect();
    let has = |name: &str| options.contains(&name);
    let mode = if remote_endpoint(endpoint) {
        Err("DOCKER_HOST is a remote engine")
    } else if docker_vm(&info, endpoint, linux_host) {
        Ok(Mode::VmBacked)
    } else if has("rootless") {
        Ok(Mode::RootlessDocker)
    } else if has("userns") {
        Err("the docker daemon remaps users (userns-remap)")
    } else {
        Ok(Mode::Rootful)
    };
    Ok(FastPath { mode, selinux: has("selinux") })
}

fn parse_docker_info(info: &str) -> anyhow::Result<DockerInfo> {
    serde_json::from_str(info.trim()).map_err(|e| anyhow::anyhow!("unexpected `docker info` output: {e}"))
}

/// The engine runs in a VM: not Linux, or a VM marker ([`docker_fast_path`]).
fn docker_vm(info: &DockerInfo, endpoint: Option<&str>, linux_host: bool) -> bool {
    let context = info.client_info.as_ref().map_or("", |c| c.context.as_str());
    !linux_host
        || VM_OPERATING_SYSTEMS.iter().any(|os| info.operating_system.contains(os))
        || vm_name(&info.name)
        || vm_name(context)
        || endpoint.is_some_and(|e| VM_SOCKET_DIRS.iter().any(|d| e.contains(d)))
}

/// What the runtime's metadata says about the engine host, for the daemon's
/// capacity report (docs/architecture/daemon/fleet.md, *Capacity*).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EngineInfo {
    pub cpus: Option<u64>,
    /// Bytes.
    pub mem_total: Option<u64>,
    /// Where the engine keeps its images and containers, as the engine host
    /// names it.
    pub root_dir: Option<PathBuf>,
    /// The engine runs on this host's own Linux kernel: local, no VM
    /// marker, the same rule as the fast path's `VmBacked` and remote
    /// checks. Only then are this host's free memory and `root_dir`'s disk
    /// the engine's.
    pub native: bool,
}

/// [`EngineInfo`] from `docker info --format '{{json .}}'` (`NCPU`,
/// `MemTotal`, `DockerRootDir`); `endpoint` and `linux_host` as for
/// [`docker_fast_path`]. Native: not remote and not a VM.
pub fn docker_engine_info(info: &str, endpoint: Option<&str>, linux_host: bool) -> anyhow::Result<EngineInfo> {
    let info = parse_docker_info(info)?;
    let native = !remote_endpoint(endpoint) && !docker_vm(&info, endpoint, linux_host);
    Ok(EngineInfo {
        cpus: info.ncpu.filter(|n| *n > 0),
        mem_total: info.mem_total.filter(|n| *n > 0),
        root_dir: info.root_dir.filter(|d| !d.is_empty()).map(PathBuf::from),
        native,
    })
}

/// The fields of `podman info --format json` the fast path reads (podman's
/// own lowerCamel keys under `host`).
#[derive(Debug, Deserialize)]
struct PodmanInfo {
    host: PodmanHost,
    #[serde(default)]
    store: PodmanStore,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PodmanStore {
    #[serde(default)]
    graph_root: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PodmanHost {
    #[serde(default)]
    security: PodmanSecurity,
    /// The client talks to a podman service elsewhere (podman machine, a
    /// connection, `--remote`).
    #[serde(default)]
    service_is_remote: bool,
    #[serde(default)]
    cpus: Option<u64>,
    /// Bytes.
    #[serde(default)]
    mem_total: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PodmanSecurity {
    #[serde(default)]
    rootless: bool,
    #[serde(default)]
    selinux_enabled: bool,
}

/// The fast path over `podman info --format json`. `endpoint` is
/// `CONTAINER_HOST` when set. Podman's metadata is never ambiguous:
/// - not Linux, or a remote service not on a local `unix://` socket (a
///   podman machine's `ssh://` connection, by name or by URL): `VmBacked`;
/// - `host.security.rootless`: `RootlessPodman`;
/// - otherwise `Rootful`.
///
/// `selinux` is `host.security.selinuxEnabled`: podman labels containers
/// whenever the engine host has SELinux on.
pub fn podman_fast_path(info: &str, endpoint: Option<&str>, linux_host: bool) -> anyhow::Result<FastPath> {
    let info = parse_podman_info(info)?;
    let host = info.host;
    let mode = if podman_vm(&host, endpoint, linux_host) {
        Mode::VmBacked
    } else if host.security.rootless {
        Mode::RootlessPodman
    } else {
        Mode::Rootful
    };
    Ok(FastPath { mode: Ok(mode), selinux: host.security.selinux_enabled })
}

fn parse_podman_info(info: &str) -> anyhow::Result<PodmanInfo> {
    serde_json::from_str(info.trim()).map_err(|e| anyhow::anyhow!("unexpected `podman info` output: {e}"))
}

/// Not Linux, or a remote service not on a local `unix://` socket.
fn podman_vm(host: &PodmanHost, endpoint: Option<&str>, linux_host: bool) -> bool {
    let local_socket = endpoint.is_some_and(|e| e.starts_with("unix://"));
    !linux_host || (host.service_is_remote && !local_socket)
}

/// [`EngineInfo`] from `podman info --format json` (`host.cpus`,
/// `host.memTotal`, `store.graphRoot`). Native: what [`podman_fast_path`]
/// doesn't call `VmBacked`.
pub fn podman_engine_info(info: &str, endpoint: Option<&str>, linux_host: bool) -> anyhow::Result<EngineInfo> {
    let info = parse_podman_info(info)?;
    Ok(EngineInfo {
        cpus: info.host.cpus.filter(|n| *n > 0),
        mem_total: info.host.mem_total.filter(|n| *n > 0),
        root_dir: info.store.graph_root.filter(|d| !d.is_empty()).map(PathBuf::from),
        native: !podman_vm(&info.host, endpoint, linux_host),
    })
}

/// The probe cache's key: the backend plus the runtime context it talks to,
/// as the runtime's own CLI resolves it, so pointing the CLI elsewhere is a
/// fresh probe. Never the machine that asked (a machine probes for itself).
/// - docker: `docker:host=<DOCKER_HOST>`, else
///   `docker:context=<DOCKER_CONTEXT>`, else the CLI config's
///   `currentContext` (`current_context`, read lazily), else
///   `docker:context=default` (docker's own order: `DOCKER_HOST` wins);
/// - podman: `podman:host=<CONTAINER_HOST>`, else
///   `podman:connection=<CONTAINER_CONNECTION>`, else `podman:default`;
/// - anything else: the backend name.
///
/// `env` returns non-empty values only.
pub fn context_key(
    backend: &str,
    env: impl Fn(&str) -> Option<String>,
    current_context: impl FnOnce() -> Option<String>,
) -> String {
    match backend {
        "docker" => match (env("DOCKER_HOST"), env("DOCKER_CONTEXT")) {
            (Some(host), _) => format!("docker:host={host}"),
            (None, Some(context)) => format!("docker:context={context}"),
            (None, None) => format!("docker:context={}", current_context().unwrap_or_else(|| "default".into())),
        },
        "podman" => match (env("CONTAINER_HOST"), env("CONTAINER_CONNECTION")) {
            (Some(host), _) => format!("podman:host={host}"),
            (None, Some(connection)) => format!("podman:connection={connection}"),
            (None, None) => "podman:default".into(),
        },
        other => other.into(),
    }
}

/// `currentContext` of the docker CLI's `config.json`; `None` when unset,
/// empty or unreadable.
pub fn docker_current_context(config_json: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct DockerConfig {
        #[serde(rename = "currentContext", default)]
        current_context: Option<String>,
    }
    let config: DockerConfig = serde_json::from_str(config_json).ok()?;
    config.current_context.filter(|c| !c.is_empty())
}

/// The devcontainer user fields a plan starts from. The image's `USER` is the
/// caller's to read (from the image config); `None` there means unknown or
/// unset.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Accounts<'a> {
    pub container_user: Option<&'a str>,
    pub remote_user: Option<&'a str>,
    pub image_user: Option<&'a str>,
    /// The adopted account's real ids, from the image's `/etc/passwd`
    /// ([`account`]); `None` when unknown ([`ASSUMED`] stands in).
    pub ids: Option<Ids>,
}

impl<'a> Accounts<'a> {
    /// The account `identity = "host"` adopts: `remoteUser`, else
    /// `containerUser`, else the image's `USER`, as an account name (a
    /// `user:group` spec loses its group). `None` when that is root or
    /// empty: there is no account to adopt.
    pub fn adopted(&self) -> Option<&'a str> {
        let spec = self.remote_user.or(self.container_user).or(self.image_user)?;
        let user = spec.split(':').next().unwrap_or_default().trim();
        (!user.is_empty() && user != "root" && user != "0").then_some(user)
    }
}

/// One account of an image's `/etc/passwd`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub uid: u32,
    pub gid: u32,
    pub home: String,
}

/// The account `name_or_uid` names in `passwd` (`/etc/passwd` text): by
/// name, else (all digits) by uid. Malformed lines are skipped.
pub fn account(passwd: &str, name_or_uid: &str) -> Option<Account> {
    let by_uid = name_or_uid.parse::<u32>().ok();
    fn parse(line: &str) -> Option<(&str, Account)> {
        let f: Vec<&str> = line.split(':').collect();
        if f.len() < 7 {
            return None;
        }
        Some((f[0], Account { uid: f[2].parse().ok()?, gid: f[3].parse().ok()?, home: f[5].to_string() }))
    }
    let accounts: Vec<(&str, Account)> = passwd.lines().filter_map(parse).collect();
    let named = accounts.iter().find(|(name, _)| *name == name_or_uid);
    named.or_else(|| accounts.iter().find(|(_, a)| Some(a.uid) == by_uid)).map(|(_, a)| a.clone())
}

/// The home `${containerEnv:HOME}` falls back to: the account `user` names
/// (remoteUser, else containerUser, else the image's `USER`; whatever the
/// plan) as `passwd` has it, else `/home/<user>`; `/root` when `user` is
/// root or unset. A rootless-docker root plan gets the account's home too
/// (its `HOME` is that home), so a config doesn't depend on the plan.
pub fn container_home(accounts: &Accounts, passwd: Option<&str>) -> String {
    let Some(user) = accounts.adopted() else {
        return "/root".into();
    };
    match passwd.and_then(|p| account(p, user)) {
        Some(a) if !a.home.is_empty() => a.home,
        Some(a) if a.uid == 0 => "/root".into(),
        _ => format!("/home/{user}"),
    }
}

/// The account `devsbd identity apply` moves to the host's ids (rootful).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remap {
    pub user: String,
    pub uid: u32,
    pub gid: u32,
}

/// How a sandbox is created and exec'd into, decided once at create and
/// recorded on the container ([`Plan::label`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub mode: Mode,
    pub identity: Identity,
    /// The adopted account (`identity = "host"` with one to adopt).
    pub account: Option<String>,
    /// `--user` at create; `None` leaves the image's (or keep-id's) user.
    pub create_user: Option<String>,
    /// `--userns` at create.
    pub userns: Option<String>,
    /// Move the account to these ids after create.
    pub remap: Option<Remap>,
    /// `-u` for every exec; `None` leaves the container's user.
    pub exec_user: Option<String>,
    /// Exec as root with `HOME` = the adopted account's home, chowned to 0
    /// (rootless docker, where root is you).
    pub root_home: bool,
    /// Relabel binds (SELinux enforcing).
    pub relabel: bool,
    /// Create with devsandbox's seccomp profile (`runtime::seccomp`), which
    /// allows unprivileged user namespaces. Set by `run`, not by [`plan`]:
    /// it depends on the runtime, the sandbox's `userns` and `--privileged`.
    pub seccomp: bool,
    /// Said once by `run`, never recorded.
    pub warnings: Vec<String>,
}

const ROOT: &str = "root";

/// The warning for an image with nothing to adopt (*The model*).
pub const NO_ACCOUNT_WARNING: &str = "no non-root account to adopt (remoteUser, containerUser and the image's USER are unset or root): running as root; \
     add the `ghcr.io/devcontainers/features/common-utils` feature to the config for one";

/// The strategy table (*The plan*) plus the no-account case (*The model*).
/// `Identity::Image` is the pre-identity behavior: create as
/// `containerUser`, exec as `remoteUser` (else `containerUser`), nothing
/// remapped. An unknown mode can only be taken literally, so it plans
/// `Image` whatever was asked. Relabeling applies to every plan.
pub fn plan(probe: Probe, identity: Identity, accounts: Accounts, host: Ids) -> Plan {
    let identity = if probe.mode == Mode::Unknown { Identity::Image } else { identity };
    let mut p = Plan {
        mode: probe.mode,
        identity,
        account: None,
        create_user: None,
        userns: None,
        remap: None,
        exec_user: None,
        root_home: false,
        relabel: probe.selinux,
        seccomp: false,
        warnings: Vec::new(),
    };
    if identity == Identity::Image {
        p.create_user = accounts.container_user.map(str::to_string);
        p.exec_user = accounts.remote_user.or(accounts.container_user).map(str::to_string);
        return p;
    }
    let Some(account) = accounts.adopted() else {
        p.create_user = Some(ROOT.into());
        p.exec_user = Some(ROOT.into());
        // Under rootless docker root is the plan anyway.
        if probe.mode != Mode::RootlessDocker {
            p.warnings.push(NO_ACCOUNT_WARNING.into());
        }
        return p;
    };
    p.account = Some(account.into());
    match probe.mode {
        Mode::Rootful => {
            p.create_user = Some(ROOT.into());
            p.remap = Some(Remap { user: account.into(), uid: host.uid, gid: host.gid });
            p.exec_user = Some(account.into());
        }
        Mode::RootlessDocker => {
            p.create_user = Some(ROOT.into());
            p.exec_user = Some(ROOT.into());
            p.root_home = true;
        }
        Mode::RootlessPodman => {
            // keep-id runs the container as the mapped uid unless `--user`
            // says otherwise.
            let ids = accounts.ids.unwrap_or(ASSUMED);
            p.userns = Some(format!("keep-id:uid={},gid={}", ids.uid, ids.gid));
            p.exec_user = Some(account.into());
        }
        Mode::VmBacked => {
            p.create_user = accounts.container_user.map(str::to_string);
            p.exec_user = Some(account.into());
        }
        Mode::Unknown => unreachable!("an unknown mode plans `Image`"),
    }
    p
}

impl Plan {
    /// The ids keep-id maps the account at (`--userns keep-id:uid=…,gid=…`),
    /// which `devsbd identity apply --expect` checks.
    pub fn keep_id(&self) -> Option<Ids> {
        let ids = self.userns.as_deref()?.strip_prefix("keep-id:")?;
        let (mut uid, mut gid) = (None, None);
        for kv in ids.split(',') {
            match kv.split_once('=') {
                Some(("uid", v)) => uid = v.parse().ok(),
                Some(("gid", v)) => gid = v.parse().ok(),
                _ => {}
            }
        }
        Some(Ids { uid: uid?, gid: gid? })
    }

    /// The one line `run` prints (*The plan*).
    pub fn summary(&self) -> String {
        let mode = self.mode.describe();
        let mut s = match (self.identity, self.mode, self.account.as_deref()) {
            (Identity::Image, ..) => {
                let user = self.exec_user.as_deref().unwrap_or("the image's user");
                format!("{mode}: identity = \"image\", exec as {user}, no remap")
            }
            (Identity::Host, _, None) => format!("{mode}: no account to adopt, exec as root"),
            (Identity::Host, Mode::Rootful, Some(a)) => {
                let (uid, gid) = self.remap.as_ref().map_or((0, 0), |r| (r.uid, r.gid));
                format!("{mode}: exec as {a}, remapped to {uid}:{gid}")
            }
            (Identity::Host, Mode::RootlessDocker, Some(a)) => {
                format!("{mode}: exec as root, root maps to you (HOME of {a})")
            }
            (Identity::Host, Mode::RootlessPodman, Some(a)) => {
                format!("{mode}: exec as {a}, keep-id maps it to you")
            }
            (Identity::Host, Mode::VmBacked, Some(a)) => {
                format!("{mode}: exec as {a}, the file share maps ownership")
            }
            (Identity::Host, Mode::Unknown, Some(a)) => format!("{mode}: exec as {a}"),
        };
        if self.relabel {
            s.push_str(", binds relabeled (SELinux)");
        }
        if self.seccomp {
            s.push_str(", user namespaces allowed (seccomp)");
        }
        s
    }

    /// The container label recording the plan: `;`-separated `key=value`
    /// fields (bare keys for flags), unset ones omitted. Warnings aren't
    /// recorded.
    pub fn label(&self) -> String {
        let mut fields = vec![format!("mode={}", self.mode.as_str()), format!("identity={}", self.identity.as_str())];
        let mut opt = |key: &str, v: &Option<String>| {
            if let Some(v) = v {
                fields.push(format!("{key}={}", escape(v)));
            }
        };
        opt("account", &self.account);
        opt("create", &self.create_user);
        opt("userns", &self.userns);
        opt("exec", &self.exec_user);
        if let Some(r) = &self.remap {
            fields.push(format!("remap={}:{}:{}", escape(&r.user), r.uid, r.gid));
        }
        if self.root_home {
            fields.push("root_home".into());
        }
        if self.relabel {
            fields.push("relabel".into());
        }
        if self.seccomp {
            fields.push("seccomp".into());
        }
        fields.join(";")
    }

    /// The inverse of [`Plan::label`]; `None` on anything it wouldn't write.
    // Nothing reads a container's label back yet (`inspect` shows it raw).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn parse_label(s: &str) -> Option<Plan> {
        let (mut mode, mut identity) = (None, None);
        let mut p = Plan {
            mode: Mode::Rootful,
            identity: Identity::Host,
            account: None,
            create_user: None,
            userns: None,
            remap: None,
            exec_user: None,
            root_home: false,
            relabel: false,
            seccomp: false,
            warnings: Vec::new(),
        };
        for field in s.split(';') {
            match field.split_once('=') {
                None => match field {
                    "root_home" => p.root_home = true,
                    "relabel" => p.relabel = true,
                    "seccomp" => p.seccomp = true,
                    _ => return None,
                },
                Some((key, v)) => match key {
                    "mode" => mode = Some(Mode::parse(v)?),
                    "identity" => identity = Some(Identity::parse(v)?),
                    "account" => p.account = Some(unescape(v)?),
                    "create" => p.create_user = Some(unescape(v)?),
                    "userns" => p.userns = Some(unescape(v)?),
                    "exec" => p.exec_user = Some(unescape(v)?),
                    "remap" => {
                        let mut it = v.rsplitn(3, ':');
                        let gid = it.next()?.parse().ok()?;
                        let uid = it.next()?.parse().ok()?;
                        let user = unescape(it.next()?)?;
                        p.remap = Some(Remap { user, uid, gid });
                    }
                    _ => return None,
                },
            }
        }
        p.mode = mode?;
        p.identity = identity?;
        Some(p)
    }
}

/// Keep `;` (the field separator) out of values; `%` escapes itself.
fn escape(v: &str) -> String {
    v.replace('%', "%25").replace(';', "%3B")
}

#[cfg_attr(not(test), allow(dead_code))] // for `Plan::parse_label`
fn unescape(v: &str) -> Option<String> {
    let mut out = String::with_capacity(v.len());
    let mut rest = v;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        match rest.get(i + 1..i + 3)? {
            "25" => out.push('%'),
            "3B" => out.push(';'),
            _ => return None,
        }
        rest = &rest[i + 3..];
    }
    out.push_str(rest);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: Ids = Ids { uid: 501, gid: 20 };
    const MODES: [Mode; 4] = [Mode::Rootful, Mode::RootlessDocker, Mode::RootlessPodman, Mode::VmBacked];

    fn ids(uid: u32, gid: u32) -> Ids {
        Ids { uid, gid }
    }

    fn probe(mode: Mode) -> Probe {
        Probe { mode, selinux: false }
    }

    fn vscode() -> Accounts<'static> {
        Accounts { container_user: Some("vscode"), remote_user: Some("vscode"), image_user: None, ids: None }
    }

    #[test]
    fn classify_outcomes() {
        // host-uid / root / subuid / squashed, and unknown.
        let rootful = classify(HOST, ids(0, 0), ids(1000, 1000), Engine::Docker);
        assert_eq!(rootful, Some(Mode::Rootful));
        let sub = ids(100_999, 100_999);
        assert_eq!(classify(HOST, HOST, sub, Engine::Docker), Some(Mode::RootlessDocker));
        assert_eq!(classify(HOST, HOST, sub, Engine::Podman), Some(Mode::RootlessPodman));
        assert_eq!(classify(HOST, HOST, HOST, Engine::Docker), Some(Mode::VmBacked));
        // A host user that is itself 1000 on a VM share still reads as VM.
        let h1000 = ids(1000, 1000);
        assert_eq!(classify(h1000, h1000, h1000, Engine::Podman), Some(Mode::VmBacked));
        // Root lands on a subuid: no known mode.
        assert_eq!(classify(HOST, sub, sub, Engine::Docker), None);
        // Root is root but 1000 lands elsewhere (e.g. userns-remap daemon).
        assert_eq!(classify(HOST, ids(0, 0), ids(231_072, 231_072), Engine::Docker), None);
        // Root is you but 1000 squashed to root.
        assert_eq!(classify(HOST, HOST, ids(0, 0), Engine::Docker), None);
    }

    const PASSWD: &str = "root:x:0:0:root:/root:/bin/bash\n\
        # a comment\n\
        broken:x:notanumber:1:::\n\
        short:x:5\n\
        node:x:1000:1000::/home/node:/bin/bash\n\
        vscode:x:1001:1002:VS Code:/home/vscode:/bin/zsh\n\
        svc:x:999:999::/srv/svc:/usr/sbin/nologin\n";

    #[test]
    fn account_by_name_or_uid() {
        let acc = |uid, gid, home: &str| Some(Account { uid, gid, home: home.into() });
        assert_eq!(account(PASSWD, "vscode"), acc(1001, 1002, "/home/vscode"));
        assert_eq!(account(PASSWD, "1000"), acc(1000, 1000, "/home/node"));
        assert_eq!(account(PASSWD, "root"), acc(0, 0, "/root"));
        assert_eq!(account(PASSWD, "nobody"), None);
        assert_eq!(account(PASSWD, "4242"), None);
        // Malformed lines are skipped, never matched.
        assert_eq!(account(PASSWD, "broken"), None);
        assert_eq!(account(PASSWD, "short"), None);
        assert_eq!(account("", "node"), None);
    }

    #[test]
    fn container_home_fallbacks() {
        let a = |r| Accounts { remote_user: r, ..Default::default() };
        assert_eq!(container_home(&a(Some("svc")), Some(PASSWD)), "/srv/svc");
        assert_eq!(container_home(&a(Some("1001")), Some(PASSWD)), "/home/vscode");
        // Not in passwd, or no passwd: the conventional home.
        assert_eq!(container_home(&a(Some("dev")), Some(PASSWD)), "/home/dev");
        assert_eq!(container_home(&a(Some("svc")), None), "/home/svc");
        // Root or nobody named.
        assert_eq!(container_home(&a(Some("root")), Some(PASSWD)), "/root");
        assert_eq!(container_home(&a(None), None), "/root");
        let image = Accounts { image_user: Some("node:node"), ..Default::default() };
        assert_eq!(container_home(&image, Some(PASSWD)), "/home/node");
    }

    #[test]
    fn keep_id_uses_the_real_ids_when_known() {
        let accounts = Accounts { ids: Some(ids(1001, 1002)), ..vscode() };
        let p = plan(probe(Mode::RootlessPodman), Identity::Host, accounts, HOST);
        assert_eq!(p.userns.as_deref(), Some("keep-id:uid=1001,gid=1002"));
        assert_eq!(p.keep_id(), Some(ids(1001, 1002)));
        // Unknown: the assumed ids.
        let p = plan(probe(Mode::RootlessPodman), Identity::Host, vscode(), HOST);
        assert_eq!(p.keep_id(), Some(ASSUMED));
        // Other modes have no keep-id.
        assert_eq!(plan(probe(Mode::Rootful), Identity::Host, accounts, HOST).keep_id(), None);
    }

    #[test]
    fn adopted_account_precedence() {
        let a = |c, r, i| Accounts { container_user: c, remote_user: r, image_user: i, ids: None };
        assert_eq!(a(Some("c"), Some("r"), Some("i")).adopted(), Some("r"));
        assert_eq!(a(Some("c"), None, Some("i")).adopted(), Some("c"));
        assert_eq!(a(None, None, Some("node:node")).adopted(), Some("node"));
        assert_eq!(a(None, None, None).adopted(), None);
        assert_eq!(a(None, None, Some("")).adopted(), None);
        assert_eq!(a(None, None, Some("0:0")).adopted(), None);
        // remoteUser root wins over a non-root containerUser.
        assert_eq!(a(Some("vscode"), Some("root"), None).adopted(), None);
    }

    #[test]
    fn host_plan_per_mode() {
        let p = plan(probe(Mode::Rootful), Identity::Host, vscode(), HOST);
        assert_eq!(p.create_user.as_deref(), Some("root"));
        assert_eq!(p.remap, Some(Remap { user: "vscode".into(), uid: 501, gid: 20 }));
        assert_eq!(p.exec_user.as_deref(), Some("vscode"));
        assert_eq!((p.userns, p.root_home), (None, false));

        let p = plan(probe(Mode::RootlessDocker), Identity::Host, vscode(), HOST);
        assert_eq!(p.create_user.as_deref(), Some("root"));
        assert_eq!(p.exec_user.as_deref(), Some("root"));
        assert_eq!(p.account.as_deref(), Some("vscode"));
        assert!(p.root_home);
        assert_eq!((p.userns, p.remap), (None, None));

        let p = plan(probe(Mode::RootlessPodman), Identity::Host, vscode(), HOST);
        assert_eq!(p.userns.as_deref(), Some("keep-id:uid=1000,gid=1000"));
        assert_eq!(p.exec_user.as_deref(), Some("vscode"));
        assert_eq!((p.create_user, p.remap, p.root_home), (None, None, false));

        let p = plan(probe(Mode::VmBacked), Identity::Host, vscode(), HOST);
        assert_eq!(p.create_user.as_deref(), Some("vscode"));
        assert_eq!(p.exec_user.as_deref(), Some("vscode"));
        assert_eq!((p.userns, p.remap, p.root_home), (None, None, false));

        for mode in MODES {
            let p = plan(probe(mode), Identity::Host, vscode(), HOST);
            assert!(p.warnings.is_empty(), "{mode:?}");
            assert!(!p.relabel, "{mode:?}");
        }
    }

    #[test]
    fn host_plan_adopts_image_user() {
        let accounts = Accounts { image_user: Some("node"), ..Default::default() };
        let p = plan(probe(Mode::Rootful), Identity::Host, accounts, HOST);
        assert_eq!(p.remap.map(|r| r.user).as_deref(), Some("node"));
        assert_eq!(p.exec_user.as_deref(), Some("node"));
    }

    #[test]
    fn no_account_runs_as_root_in_every_mode() {
        for mode in MODES {
            let p = plan(probe(mode), Identity::Host, Accounts::default(), HOST);
            assert_eq!(p.create_user.as_deref(), Some("root"), "{mode:?}");
            assert_eq!(p.exec_user.as_deref(), Some("root"), "{mode:?}");
            assert_eq!((&p.account, &p.userns, &p.remap, p.root_home), (&None, &None, &None, false), "{mode:?}");
            let want: &[&str] = if mode == Mode::RootlessDocker { &[] } else { &[NO_ACCOUNT_WARNING] };
            assert_eq!(p.warnings, want, "{mode:?}");
        }
    }

    #[test]
    fn image_identity_is_todays_behavior_in_every_mode() {
        let accounts = Accounts { container_user: Some("app:app"), remote_user: Some("dev"), image_user: Some("x"), ids: None };
        let only_container = Accounts { container_user: Some("app"), ..Default::default() };
        for mode in MODES {
            let p = plan(probe(mode), Identity::Image, accounts, HOST);
            assert_eq!(p.create_user.as_deref(), Some("app:app"), "{mode:?}");
            assert_eq!(p.exec_user.as_deref(), Some("dev"), "{mode:?}");
            assert_eq!((&p.account, &p.userns, &p.remap, p.root_home), (&None, &None, &None, false), "{mode:?}");
            assert!(p.warnings.is_empty());

            let p = plan(probe(mode), Identity::Image, only_container, HOST);
            assert_eq!(p.exec_user.as_deref(), Some("app"), "{mode:?}");

            let p = plan(probe(mode), Identity::Image, Accounts::default(), HOST);
            assert_eq!((p.create_user, p.exec_user), (None, None), "{mode:?}");
            assert!(p.warnings.is_empty());
        }
    }

    #[test]
    fn unknown_mode_plans_image() {
        let accounts = Accounts { container_user: Some("app"), remote_user: Some("dev"), image_user: None, ids: None };
        let p = plan(probe(Mode::Unknown), Identity::Host, accounts, HOST);
        let mut want = plan(probe(Mode::Unknown), Identity::Image, accounts, HOST);
        assert_eq!(p, want);
        assert_eq!(p.identity, Identity::Image);
        assert_eq!((p.create_user.as_deref(), p.exec_user.as_deref()), (Some("app"), Some("dev")));
        assert_eq!(p.summary(), "unknown mode: identity = \"image\", exec as dev, no remap");
        want.warnings.clear();
        assert_eq!(Plan::parse_label(&p.label()), Some(want));
    }

    #[test]
    fn selinux_relabels_every_plan() {
        for mode in MODES {
            for identity in [Identity::Host, Identity::Image] {
                let p = plan(Probe { mode, selinux: true }, identity, vscode(), HOST);
                assert!(p.relabel, "{mode:?} {identity:?}");
            }
        }
    }

    #[test]
    fn summaries() {
        let s = |mode, identity, accounts| plan(probe(mode), identity, accounts, HOST).summary();
        assert_eq!(
            s(Mode::RootlessDocker, Identity::Host, vscode()),
            "rootless docker: exec as root, root maps to you (HOME of vscode)"
        );
        assert_eq!(s(Mode::Rootful, Identity::Host, vscode()), "rootful: exec as vscode, remapped to 501:20");
        assert_eq!(s(Mode::RootlessPodman, Identity::Host, vscode()), "rootless podman: exec as vscode, keep-id maps it to you");
        assert_eq!(s(Mode::VmBacked, Identity::Host, Accounts::default()), "VM-backed: no account to adopt, exec as root");
        assert_eq!(s(Mode::Rootful, Identity::Image, vscode()), "rootful: identity = \"image\", exec as vscode, no remap");
        let p = plan(Probe { mode: Mode::Rootful, selinux: true }, Identity::Host, vscode(), HOST);
        assert!(p.summary().ends_with(", binds relabeled (SELinux)"));
        let p = Plan { seccomp: true, ..p };
        assert!(p.summary().ends_with(", binds relabeled (SELinux), user namespaces allowed (seccomp)"));
    }

    #[test]
    fn label_round_trips() {
        let odd = Accounts { container_user: Some("a;b%c:grp"), remote_user: None, image_user: None, ids: None };
        for mode in MODES {
            for selinux in [false, true] {
                for identity in [Identity::Host, Identity::Image] {
                    for accounts in [vscode(), Accounts::default(), odd] {
                        for seccomp in [false, true] {
                            let mut p = plan(Probe { mode, selinux }, identity, accounts, HOST);
                            p.seccomp = seccomp;
                            let label = p.label();
                            p.warnings.clear();
                            assert_eq!(Plan::parse_label(&label), Some(p), "{label}");
                        }
                    }
                }
            }
        }
        let label = plan(probe(Mode::RootlessPodman), Identity::Host, vscode(), HOST).label();
        assert_eq!(label, "mode=rootless-podman;identity=host;account=vscode;userns=keep-id:uid=1000,gid=1000;exec=vscode");
        let p = Plan { seccomp: true, ..plan(probe(Mode::Rootful), Identity::Host, vscode(), HOST) };
        assert!(p.label().ends_with(";seccomp"), "{}", p.label());
    }

    #[test]
    fn parse_label_rejects_garbage() {
        for bad in ["", "identity=host", "mode=rootful", "mode=nope;identity=host", "mode=rootful;identity=host;x=1"] {
            assert_eq!(Plan::parse_label(bad), None, "{bad}");
        }
        assert_eq!(Plan::parse_label("mode=rootful;identity=host;exec=a%zz"), None);
        assert_eq!(Plan::parse_label("mode=rootful;identity=host;remap=u:x:1"), None);
    }

    // `docker info --format '{{json .}}'` captures, trimmed to the fields
    // around the ones read (the real output carries ~60 more).
    const DOCKER_ROOTFUL: &str = r#"{"ID":"0c5d6d2e-7b1f-4f0e-9a43-2c1b8e1d9f10","Containers":3,"ContainersRunning":1,"Images":12,"Driver":"overlay2","CgroupDriver":"systemd","CgroupVersion":"2","KernelVersion":"6.8.0-45-generic","OperatingSystem":"Ubuntu 24.04.1 LTS","OSVersion":"24.04","OSType":"linux","Architecture":"x86_64","NCPU":8,"MemTotal":33363566592,"DockerRootDir":"/var/lib/docker","Name":"build-01","ServerVersion":"27.3.1","SecurityOptions":["name=apparmor","name=seccomp,profile=builtin","name=cgroupns"],"Warnings":null,"ClientInfo":{"Debug":false,"Version":"27.3.1","GitCommit":"ce12230","GoVersion":"go1.22.7","Os":"linux","Arch":"amd64","BuildTime":"Fri Sep 20 11:41:00 2024","Context":"default","Plugins":[],"Warnings":null}}"#;
    const DOCKER_ROOTLESS: &str = r#"{"ID":"8f1e2a3b-1c4d-4e5f-8a9b-0c1d2e3f4a5b","Containers":0,"Driver":"overlay2","CgroupDriver":"systemd","CgroupVersion":"2","OperatingSystem":"Ubuntu 24.04.1 LTS","OSType":"linux","Architecture":"x86_64","DockerRootDir":"/home/dev/.local/share/docker","Name":"build-01","ServerVersion":"27.3.1","SecurityOptions":["name=seccomp,profile=builtin","name=rootless","name=cgroupns"],"Warnings":["WARNING: No cpu cfs quota support"],"ClientInfo":{"Debug":false,"Version":"27.3.1","Os":"linux","Arch":"amd64","Context":"rootless","Plugins":[],"Warnings":null}}"#;
    const DOCKER_USERNS_REMAP: &str = r#"{"ID":"5a6b7c8d-9e0f-4a1b-8c2d-3e4f5a6b7c8d","Driver":"overlay2","OperatingSystem":"Debian GNU/Linux 12 (bookworm)","OSType":"linux","DockerRootDir":"/var/lib/docker/231072.231072","Name":"ci-runner","ServerVersion":"26.1.4","SecurityOptions":["name=apparmor","name=seccomp,profile=builtin","name=userns","name=cgroupns"],"ClientInfo":{"Context":"default"}}"#;
    const DOCKER_SELINUX: &str = r#"{"ID":"1b2c3d4e-5f6a-4b7c-8d9e-0f1a2b3c4d5e","Driver":"overlay2","OperatingSystem":"Fedora Linux 40 (Workstation Edition)","OSType":"linux","DockerRootDir":"/var/lib/docker","Name":"fedora","ServerVersion":"27.1.1","SecurityOptions":["name=seccomp,profile=builtin","name=selinux","name=cgroupns"],"ClientInfo":{"Context":"default"}}"#;
    const DOCKER_DESKTOP: &str = r#"{"ID":"c0ffee00-1111-4222-8333-444455556666","Driver":"overlayfs","KernelVersion":"6.10.14-linuxkit","OperatingSystem":"Docker Desktop","OSType":"linux","Architecture":"aarch64","DockerRootDir":"/var/lib/docker","Name":"docker-desktop","ServerVersion":"27.3.1","SecurityOptions":["name=seccomp,profile=unconfined","name=cgroupns"],"ClientInfo":{"Version":"27.3.1","Os":"darwin","Arch":"arm64","Context":"desktop-linux"}}"#;
    const DOCKER_ORBSTACK: &str = r#"{"ID":"2d3e4f5a-6b7c-4d8e-9f0a-1b2c3d4e5f6a","Driver":"overlay2","KernelVersion":"6.11.9-orbstack-00279-g5a4d8b3c5d1b","OperatingSystem":"OrbStack","OSType":"linux","Architecture":"aarch64","DockerRootDir":"/var/lib/docker","Name":"orbstack","ServerVersion":"27.3.1","SecurityOptions":["name=seccomp,profile=builtin","name=cgroupns"],"ClientInfo":{"Os":"darwin","Context":"orbstack"}}"#;
    const DOCKER_COLIMA: &str = r#"{"ID":"3e4f5a6b-7c8d-4e9f-8a0b-1c2d3e4f5a6b","Driver":"overlay2","OperatingSystem":"Ubuntu 24.04 LTS","OSType":"linux","DockerRootDir":"/var/lib/docker","Name":"colima","ServerVersion":"27.4.0","SecurityOptions":["name=apparmor","name=seccomp,profile=builtin","name=cgroupns"],"ClientInfo":{"Os":"linux","Context":"colima"}}"#;
    /// An older CLI: no `ClientInfo`, `SecurityOptions` null.
    const DOCKER_OLD: &str = r#"{"ID":"ABCD:EFGH","Driver":"overlay2","OperatingSystem":"CentOS Linux 7 (Core)","OSType":"linux","Name":"legacy","ServerVersion":"20.10.24","SecurityOptions":null}"#;

    // `podman info --format json` captures, trimmed the same way.
    const PODMAN_ROOTLESS: &str = r#"{"host":{"arch":"amd64","buildahVersion":"1.33.7","cgroupManager":"systemd","cgroupVersion":"v2","distribution":{"distribution":"ubuntu","version":"24.04","codename":"noble"},"hostname":"build-01","idMappings":{"gidmap":[{"container_id":0,"host_id":1000,"size":1},{"container_id":1,"host_id":100000,"size":65536}],"uidmap":[{"container_id":0,"host_id":1000,"size":1},{"container_id":1,"host_id":100000,"size":65536}]},"kernel":"6.8.0-45-generic","os":"linux","remoteSocket":{"exists":false,"path":"/run/user/1000/podman/podman.sock"},"security":{"apparmorEnabled":false,"capabilities":"CAP_CHOWN,CAP_DAC_OVERRIDE,CAP_FOWNER,CAP_FSETID,CAP_KILL,CAP_NET_BIND_SERVICE,CAP_SETFCAP,CAP_SETGID,CAP_SETPCAP,CAP_SETUID,CAP_SYS_CHROOT","rootless":true,"seccompEnabled":true,"seccompProfilePath":"/usr/share/containers/seccomp.json","selinuxEnabled":false},"serviceIsRemote":false},"store":{"graphDriverName":"overlay","graphRoot":"/home/dev/.local/share/containers/storage"},"registries":{},"plugins":{"log":["k8s-file","none","passthrough","journald"]},"version":{"APIVersion":"4.9.3","Version":"4.9.3","GoVersion":"go1.22.2","GitCommit":"","BuiltTime":"Thu Jan  1 00:00:00 1970","Built":0,"OsArch":"linux/amd64","Os":"linux"}}"#;
    const PODMAN_ROOTFUL_SELINUX: &str = r#"{"host":{"arch":"amd64","cgroupVersion":"v2","distribution":{"distribution":"fedora","variant":"workstation","version":"40"},"hostname":"fedora","idMappings":{"gidmap":null,"uidmap":null},"os":"linux","remoteSocket":{"exists":true,"path":"/run/podman/podman.sock"},"security":{"apparmorEnabled":false,"rootless":false,"seccompEnabled":true,"seccompProfilePath":"/usr/share/containers/seccomp.json","selinuxEnabled":true},"serviceIsRemote":false},"store":{"graphRoot":"/var/lib/containers/storage"},"version":{"APIVersion":"5.2.3","Version":"5.2.3","OsArch":"linux/amd64","Os":"linux"}}"#;
    /// macOS client talking to a podman machine (Fedora CoreOS in the VM).
    const PODMAN_MACHINE: &str = r#"{"host":{"arch":"arm64","cgroupVersion":"v2","distribution":{"distribution":"fedora","variant":"coreos","version":"40"},"hostname":"localhost.localdomain","os":"linux","remoteSocket":{"exists":true,"path":"/run/user/501/podman/podman.sock"},"security":{"apparmorEnabled":false,"rootless":true,"seccompEnabled":true,"selinuxEnabled":true},"serviceIsRemote":true},"store":{"graphRoot":"/var/home/core/.local/share/containers/storage"},"version":{"APIVersion":"5.2.3","Version":"5.2.3","OsArch":"linux/arm64","Os":"linux"}}"#;

    fn known(mode: Mode, selinux: bool) -> FastPath {
        FastPath { mode: Ok(mode), selinux }
    }

    #[test]
    fn docker_fast_path_per_host() {
        let fp = |info, endpoint, linux| docker_fast_path(info, endpoint, linux).unwrap();
        assert_eq!(fp(DOCKER_ROOTFUL, None, true), known(Mode::Rootful, false));
        assert_eq!(fp(DOCKER_OLD, None, true), known(Mode::Rootful, false));
        assert_eq!(fp(DOCKER_ROOTLESS, Some("unix:///run/user/1000/docker.sock"), true), known(Mode::RootlessDocker, false));
        assert_eq!(fp(DOCKER_SELINUX, None, true), known(Mode::Rootful, true));
        let remap = fp(DOCKER_USERNS_REMAP, None, true);
        assert!(remap.mode.is_err(), "{remap:?}");
        // VM markers, on a Linux host (Docker Desktop for Linux) or not.
        for info in [DOCKER_DESKTOP, DOCKER_ORBSTACK, DOCKER_COLIMA] {
            assert_eq!(fp(info, None, true), known(Mode::VmBacked, false), "{info}");
            assert_eq!(fp(info, None, false), known(Mode::VmBacked, false), "{info}");
        }
        // Off Linux any local engine is in a VM, rootless or remapped included.
        assert_eq!(fp(DOCKER_ROOTFUL, None, false), known(Mode::VmBacked, false));
        assert_eq!(fp(DOCKER_USERNS_REMAP, None, false), known(Mode::VmBacked, false));
        // A VM's socket in DOCKER_HOST, whatever the engine reports.
        let colima_sock = Some("unix:///home/dev/.colima/default/docker.sock");
        assert_eq!(fp(DOCKER_ROOTFUL, colima_sock, true), known(Mode::VmBacked, false));
        // A remote engine: ambiguous, even off Linux.
        for host in ["tcp://10.0.0.5:2376", "ssh://me@box"] {
            assert!(fp(DOCKER_ROOTFUL, Some(host), true).mode.is_err(), "{host}");
            assert!(fp(DOCKER_DESKTOP, Some(host), false).mode.is_err(), "{host}");
        }
        assert!(docker_fast_path("Cannot connect to the Docker daemon", None, true).is_err());
    }

    #[test]
    fn docker_engine_info_reads_cpus_memory_and_root() {
        let info = |text, endpoint, linux| docker_engine_info(text, endpoint, linux).unwrap();
        assert_eq!(
            info(DOCKER_ROOTFUL, None, true),
            EngineInfo { cpus: Some(8), mem_total: Some(33363566592), root_dir: Some("/var/lib/docker".into()), native: true }
        );
        // Missing fields are unknown; native follows the fast path's VM and
        // remote rules (a remapped daemon is still this host's kernel).
        let rootless = info(DOCKER_ROOTLESS, Some("unix:///run/user/1000/docker.sock"), true);
        assert_eq!((rootless.cpus, rootless.mem_total, rootless.native), (None, None, true));
        assert_eq!(rootless.root_dir, Some("/home/dev/.local/share/docker".into()));
        assert!(info(DOCKER_USERNS_REMAP, None, true).native);
        assert_eq!(info(DOCKER_OLD, None, true).root_dir, None);
        for vm in [DOCKER_DESKTOP, DOCKER_ORBSTACK, DOCKER_COLIMA] {
            assert!(!info(vm, None, true).native, "{vm}");
        }
        assert!(!info(DOCKER_ROOTFUL, None, false).native, "off Linux");
        assert!(!info(DOCKER_ROOTFUL, Some("tcp://10.0.0.5:2376"), true).native, "remote");
        assert!(!info(DOCKER_ROOTFUL, Some("unix:///home/dev/.colima/default/docker.sock"), true).native);
        let zero = DOCKER_ROOTFUL.replace("\"NCPU\":8", "\"NCPU\":0");
        assert_eq!(info(&zero, None, true).cpus, None, "0 is unknown");
        assert!(docker_engine_info("Cannot connect to the Docker daemon", None, true).is_err());
    }

    #[test]
    fn podman_engine_info_reads_cpus_memory_and_graph_root() {
        let with = PODMAN_ROOTLESS.replace("\"serviceIsRemote\":false", "\"serviceIsRemote\":false,\"cpus\":4,\"memTotal\":16471560192");
        let info = podman_engine_info(&with, None, true).unwrap();
        assert_eq!(
            info,
            EngineInfo {
                cpus: Some(4),
                mem_total: Some(16471560192),
                root_dir: Some("/home/dev/.local/share/containers/storage".into()),
                native: true
            }
        );
        assert!(!podman_engine_info(PODMAN_MACHINE, None, true).unwrap().native);
        assert!(podman_engine_info(PODMAN_MACHINE, Some("unix:///run/user/1000/podman/podman.sock"), true).unwrap().native);
        assert!(!podman_engine_info(PODMAN_ROOTLESS, None, false).unwrap().native);
        assert_eq!(podman_engine_info(r#"{"host":{}}"#, None, true).unwrap().root_dir, None);
    }

    #[test]
    fn vm_names_match_whole_or_dash_prefixed() {
        for name in ["colima", "colima-work", "lima-rancher-desktop", "orbstack", "desktop-linux"] {
            assert!(vm_name(name), "{name}");
        }
        for name in ["", "default", "colimax", "limado", "build-01", "rootless"] {
            assert!(!vm_name(name), "{name}");
        }
    }

    #[test]
    fn podman_fast_path_per_host() {
        let fp = |info, endpoint, linux| podman_fast_path(info, endpoint, linux).unwrap();
        assert_eq!(fp(PODMAN_ROOTLESS, None, true), known(Mode::RootlessPodman, false));
        assert_eq!(fp(PODMAN_ROOTFUL_SELINUX, None, true), known(Mode::Rootful, true));
        assert_eq!(fp(PODMAN_MACHINE, None, false), known(Mode::VmBacked, true));
        // A remote service on Linux: a machine's ssh connection is a VM, a
        // local socket is the engine's own mode.
        assert_eq!(fp(PODMAN_MACHINE, Some("ssh://core@127.0.0.1:50123/run/user/501/podman/podman.sock"), true), known(Mode::VmBacked, true));
        assert_eq!(fp(PODMAN_MACHINE, None, true), known(Mode::VmBacked, true));
        assert_eq!(fp(PODMAN_MACHINE, Some("unix:///run/user/1000/podman/podman.sock"), true), known(Mode::RootlessPodman, true));
        assert_eq!(fp(PODMAN_ROOTLESS, None, false), known(Mode::VmBacked, false));
        assert!(podman_fast_path("{}", None, true).is_err());
    }

    #[test]
    fn context_keys() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| pairs.iter().find(|(key, _)| *key == k).map(|(_, v)| v.to_string())
        };
        let never = || -> Option<String> { panic!("config read with the env set") };
        let host = &[("DOCKER_HOST", "unix:///run/user/1000/docker.sock"), ("DOCKER_CONTEXT", "x")];
        assert_eq!(context_key("docker", env(host), never), "docker:host=unix:///run/user/1000/docker.sock");
        assert_eq!(context_key("docker", env(&[("DOCKER_CONTEXT", "colima")]), never), "docker:context=colima");
        assert_eq!(context_key("docker", env(&[]), || Some("orbstack".into())), "docker:context=orbstack");
        assert_eq!(context_key("docker", env(&[]), || None), "docker:context=default");
        let podman = &[("CONTAINER_HOST", "ssh://core@127.0.0.1:50123/run/podman.sock"), ("CONTAINER_CONNECTION", "m")];
        assert_eq!(context_key("podman", env(podman), never), "podman:host=ssh://core@127.0.0.1:50123/run/podman.sock");
        assert_eq!(context_key("podman", env(&[("CONTAINER_CONNECTION", "podman-machine-default")]), never), "podman:connection=podman-machine-default");
        // Docker's variables mean nothing to podman, and the reverse.
        assert_eq!(context_key("podman", env(&[("DOCKER_HOST", "tcp://x")]), never), "podman:default");
        assert_eq!(context_key("docker", env(&[("CONTAINER_HOST", "unix:///x")]), || None), "docker:context=default");
        assert_eq!(context_key("container", env(host), never), "container");
    }

    #[test]
    fn docker_current_context_from_config() {
        let config = r#"{"auths":{"ghcr.io":{}},"credsStore":"osxkeychain","currentContext":"orbstack"}"#;
        assert_eq!(docker_current_context(config).as_deref(), Some("orbstack"));
        assert_eq!(docker_current_context(r#"{"auths":{}}"#), None);
        assert_eq!(docker_current_context(r#"{"currentContext":""}"#), None);
        assert_eq!(docker_current_context("not json"), None);
    }

    #[test]
    fn probe_serde_round_trips() {
        for mode in MODES {
            let p = Probe { mode, selinux: true };
            let text = toml::to_string(&p).unwrap();
            assert!(text.contains(&format!("mode = \"{}\"", mode.as_str())), "{text}");
            assert_eq!(toml::from_str::<Probe>(&text).unwrap(), p);
        }
    }

    #[test]
    fn identity_serde_is_lowercase() {
        #[derive(Deserialize, Serialize)]
        struct T {
            identity: Identity,
        }
        let t: T = toml::from_str("identity = \"image\"").unwrap();
        assert_eq!(t.identity, Identity::Image);
        assert_eq!(toml::to_string(&T { identity: Identity::Host }).unwrap().trim(), "identity = \"host\"");
        assert!(toml::from_str::<T>("identity = \"Host\"").is_err());
    }
}
