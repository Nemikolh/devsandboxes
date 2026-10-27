//! The worker policy (docs/architecture/daemon/fleet.md, *Worker policy*):
//! what a config under a fleet root (`<data>/devsandbox/fleet/<hub>/`, the
//! root a hub reaches on a lending host) may ask of this host, beyond
//! containers that see only what was lent. Denied by default; each item has
//! an allowance in `lend.toml`'s `[allow]` table ([`Allow`]).
//!
//! A config is under the policy by its path, whatever process runs it (a
//! lending daemon's verb subprocess, a local CLI pointed at the dir): every
//! entry point calls [`Policy::for_dir`] itself, so there is no flag to
//! forget. The authority is the check of the final runtime argv
//! ([`Policy::check_run`], [`Policy::check_build`]) right before it runs, so
//! nothing the pipeline adds after the config (features, identity, caches)
//! escapes it: a flag that isn't on its allow list is refused. The config
//! checks ([`Policy::check_config`], [`Policy::check_path`]) refuse earlier,
//! before anything is done on the host, and cover what never reaches a
//! container's argv (`initializeCommand`, `dispatcher`, `inbox`,
//! `${localEnv:…}`, which `config::resolve_var` also never expands for a
//! name [`local_env_allowed`] refuses).
//!
//! Cross-platform (no `serve`): `lend.toml` is read here for its `[allow]`
//! table only.

use std::fmt;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{Config, ResolvedSandbox, ResolvedService, ServiceScope};
use crate::runtime::NAME_PREFIX;

/// `lend.toml`'s `[allow]`: what the worker policy permits beyond its
/// defaults. Every allowance is off (empty) unless set. Unknown keys are
/// ignored: an allowance a newer binary knows is one an older one doesn't
/// grant.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Allow {
    /// `--privileged`: the sandbox's `privileged` or a feature's.
    #[serde(skip_serializing_if = "is_false")]
    pub privileged: bool,
    /// A bind of a container runtime's socket (or of a dir holding it).
    #[serde(skip_serializing_if = "is_false")]
    pub runtime_socket: bool,
    /// `--network host` (`run` or `build`).
    #[serde(skip_serializing_if = "is_false")]
    pub host_network: bool,
    /// `caches`.
    #[serde(skip_serializing_if = "is_false")]
    pub caches: bool,
    /// Binds under a config root's `shared-volumes/` (`${sharedVolumes}`
    /// mounts, `persist-shell-history`).
    #[serde(skip_serializing_if = "is_false")]
    pub shared_volumes: bool,
    /// `worktree-link`.
    #[serde(skip_serializing_if = "is_false")]
    pub worktree_link: bool,
    /// `scope = "global"` services.
    #[serde(skip_serializing_if = "is_false")]
    pub global_services: bool,
    /// `dispatcher`: control ops that create children on this host.
    #[serde(skip_serializing_if = "is_false")]
    pub dispatcher: bool,
    /// `inbox = true`: Inbox threads on this host.
    #[serde(skip_serializing_if = "is_false")]
    pub inbox: bool,
    /// `initializeCommand`, which runs on this host, outside any container.
    #[serde(skip_serializing_if = "is_false")]
    pub initialize_command: bool,
    /// A published port (`-p`) on more than loopback.
    #[serde(skip_serializing_if = "is_false")]
    pub publish: bool,
    /// Names of this host's env vars a config may read with
    /// `${localEnv:NAME}` (exact names; the hub writes the config, so any
    /// other would hand it this host's env).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub local_env: Vec<String>,
    /// Host paths outside the fleet root that binds, build contexts,
    /// `folder`s and local features may use (absolute; a path covers what's
    /// under it).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub binds: Vec<PathBuf>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl Allow {
    /// `binds` entries must be absolute.
    pub fn check(&self) -> Result<()> {
        if let Some(bad) = self.binds.iter().find(|b| !b.is_absolute()) {
            bail!("`allow.binds`: `{}` is not an absolute path", bad.display());
        }
        Ok(())
    }
}

/// Why the policy refused: what was asked and the allowance that permits it
/// (`None`: none does).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub what: String,
    pub allow: Option<String>,
}

impl Refusal {
    pub fn new(what: impl Into<String>, allow: impl Into<String>) -> Self {
        Refusal { what: what.into(), allow: Some(allow.into()) }
    }

    fn always(what: impl Into<String>) -> Self {
        Refusal { what: what.into(), allow: None }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "refused by this machine's lending policy: {}", self.what)?;
        match &self.allow {
            Some(allow) => write!(f, " (allow with {allow} under [allow] in lend.toml)"),
            None => write!(f, " (no lend.toml allowance permits it)"),
        }
    }
}

impl std::error::Error for Refusal {}

/// The policy over one fleet root.
#[derive(Debug, Clone)]
pub struct Policy {
    /// The canonical fleet root (`<data>/devsandbox/fleet/<hub>`), or the
    /// fleet dir itself for a config placed directly in it.
    pub root: PathBuf,
    /// The canonical `<data>/devsandbox`: where devsandbox's own seccomp
    /// profiles live.
    pub data: PathBuf,
    pub allow: Allow,
    /// The runtimes' socket paths a bind must not reach ([`runtime_sockets`]).
    pub sockets: Vec<PathBuf>,
}

/// Socket file names refused wherever they are.
const SOCKET_NAMES: [&str; 3] = ["docker.sock", "podman.sock", "containerd.sock"];

/// The runtime socket paths of this host: the usual ones, `DOCKER_HOST`'s
/// and `CONTAINER_HOST`'s unix paths, and the per-user ones under
/// `$XDG_RUNTIME_DIR` and `$HOME`.
pub fn runtime_sockets(
    docker_host: Option<&str>,
    container_host: Option<&str>,
    xdg_runtime_dir: Option<&Path>,
    home: Option<&Path>,
) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> =
        ["/var/run/docker.sock", "/run/docker.sock", "/run/podman/podman.sock", "/var/run/podman/podman.sock"]
            .into_iter()
            .map(PathBuf::from)
            .collect();
    for host in [docker_host, container_host].into_iter().flatten() {
        if let Some(path) = host.strip_prefix("unix://") {
            out.push(PathBuf::from(path));
        }
    }
    if let Some(run) = xdg_runtime_dir {
        out.extend([run.join("docker.sock"), run.join("podman/podman.sock")]);
    }
    if let Some(home) = home {
        out.extend([home.join(".docker/run/docker.sock"), home.join(".colima/default/docker.sock")]);
    }
    out
}

/// The fleet root `dir` is under, from the canonical `<data>/devsandbox`
/// (`data`): `<data>/fleet/<first component of dir's path below it>`, or
/// the fleet dir itself for `dir` = it. `None` when `dir` (canonical) isn't
/// under it; symlinks count where they point.
/// A `dir` that doesn't exist (nothing runs from it) is placed by its path
/// as written, made absolute; one with a `..` there is an error.
pub fn fleet_root_of(data: &Path, dir: &Path) -> Result<Option<PathBuf>> {
    let fleet = data.join("fleet");
    let (dir, fleet) = match dir.canonicalize() {
        Ok(dir) => match fleet.canonicalize() {
            Ok(fleet) => (dir, fleet),
            Err(_) => return Ok(None), // no fleet dir: nothing can be in it.
        },
        Err(_) => {
            let abs = std::path::absolute(dir).with_context(|| format!("cannot resolve {}", dir.display()))?;
            if abs.components().any(|c| c == Component::ParentDir) {
                bail!("cannot resolve {}", dir.display());
            }
            (abs, fleet.canonicalize().unwrap_or(fleet))
        }
    };
    let Ok(rel) = dir.strip_prefix(&fleet) else {
        return Ok(None);
    };
    Ok(Some(match rel.components().next() {
        Some(first) => fleet.join(first),
        None => fleet,
    }))
}

/// `[allow]` of the `lend.toml` at `path`; missing is the defaults. Only that
/// table is read (the rest is `serve::fleet::lend`'s), so a file that's
/// otherwise invalid still yields its allowances; one whose `[allow]` doesn't
/// parse is an error, never the defaults' silent stand-in.
pub fn load_allow(path: &Path) -> Result<Allow> {
    #[derive(Deserialize)]
    struct File {
        #[serde(default)]
        allow: Allow,
    }
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let file: File = toml::from_str(&text).with_context(|| format!("invalid {}", path.display()))?;
            file.allow.check().with_context(|| format!("invalid {}", path.display()))?;
            Ok(file.allow)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Allow::default()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

impl Policy {
    /// The policy for config dir `dir` over the process env (`$XDG_DATA_HOME`
    /// / `$HOME`, the runtime env vars); `None` when `dir` is under no fleet
    /// root.
    pub fn for_dir(dir: &Path) -> Result<Option<Policy>> {
        let data = crate::daemon_config::data_dir_from(std::env::var_os("XDG_DATA_HOME"), std::env::var_os("HOME"))?;
        let var = |k: &str| std::env::var(k).ok();
        let os = |k: &str| std::env::var_os(k).map(PathBuf::from);
        let sockets = runtime_sockets(
            var("DOCKER_HOST").as_deref(),
            var("CONTAINER_HOST").as_deref(),
            os("XDG_RUNTIME_DIR").as_deref(),
            os("HOME").as_deref(),
        );
        Self::detect(&data, dir, sockets)
    }

    /// [`Self::for_dir`] over a given `<data>/devsandbox` and socket list.
    pub fn detect(data: &Path, dir: &Path, sockets: Vec<PathBuf>) -> Result<Option<Policy>> {
        let data = data.canonicalize().unwrap_or_else(|_| data.to_path_buf());
        let Some(root) = fleet_root_of(&data, dir)? else {
            return Ok(None);
        };
        let allow = load_allow(&data.join("lend.toml"))?;
        Ok(Some(Policy { root, data, allow, sockets }))
    }

    /// What the resolved sandbox `sandbox` of `config` (at `dir`) declares
    /// that never shows in a container's argv or is done on the host before
    /// one: `initializeCommand`, `dispatcher`, `inbox`, `caches`,
    /// `persist-shell-history`, `worktree-link`, `privileged`, `global`
    /// services, local features outside the fleet root (or with a file
    /// resolving outside it), and `${localEnv:NAME}` in the merged sandbox
    /// or its services for a name `local-env` doesn't list.
    pub fn check_config(&self, dir: &Path, config: &Config, sandbox: &ResolvedSandbox) -> Result<(), Refusal> {
        let props = &sandbox.properties;
        let a = &self.allow;
        let flags = [
            (props.initialize_command.is_some(), a.initialize_command, "`initializeCommand`", "initialize-command"),
            (props.dispatcher.is_some(), a.dispatcher, "`dispatcher`", "dispatcher"),
            (props.inbox == Some(true), a.inbox, "`inbox`", "inbox"),
            (props.caches.as_ref().is_some_and(|c| !c.is_empty()), a.caches, "`caches`", "caches"),
            (props.persist_shell_history == Some(true), a.shared_volumes, "`persist-shell-history`", "shared-volumes"),
            (props.worktree_link.as_ref().is_some_and(|l| !l.is_empty()), a.worktree_link, "`worktree-link`", "worktree-link"),
            (props.privileged == Some(true), a.privileged, "privileged", "privileged"),
        ];
        for (asked, allowed, what, key) in flags {
            if asked && !allowed {
                return Err(Refusal::new(what, format!("{key} = true")));
            }
        }
        // `${localEnv:…}` anywhere in the merged sandbox or its services,
        // before anything is substituted.
        let mut texts = Vec::new();
        let merged = config.resolved_table(&sandbox.name).map(|(t, _)| t).unwrap_or_default();
        table_strings(&merged, &mut texts);
        for name in props.services.iter().flatten() {
            if let Some(table) = config.services.get(name) {
                table_strings(table, &mut texts);
            }
        }
        for name in texts.iter().flat_map(|t| local_env_names(t)) {
            if !self.allows_local_env(name) {
                return Err(Refusal::new(
                    format!("`${{localEnv:{name}}}` (this host's env)"),
                    format!("local-env = [\"{name}\"]"),
                ));
            }
        }
        for name in props.services.iter().flatten() {
            // An unknown service fails later with its own error.
            if let Ok(service) = config.resolve_service(name) {
                self.check_service(&service)?;
            }
        }
        for reference in props.features.iter().flat_map(|f| f.keys()) {
            let Ok(source) = crate::features::FeatureSource::parse(reference) else { continue };
            if let Some(local) = source.local_dir(dir) {
                self.check_tree(&local, &format!("local feature `{reference}`"))?;
            }
        }
        Ok(())
    }

    /// Whether `allow.local-env` lists `var`'s name (up to a `:`).
    pub fn allows_local_env(&self, var: &str) -> bool {
        let name = var.split(':').next().unwrap_or_default();
        self.allow.local_env.iter().any(|n| n == name)
    }

    /// A service's `scope = "global"`.
    pub fn check_service(&self, service: &ResolvedService) -> Result<(), Refusal> {
        if service.spec.scope == ServiceScope::Global && !self.allow.global_services {
            return Err(Refusal::new(format!("global service `{}`", service.name), "global-services = true"));
        }
        Ok(())
    }

    /// A host path the host reads or a container sees (`what` names it):
    /// resolved (symlinks, `..`) under the fleet root or an `allow.binds`
    /// entry.
    pub fn check_path(&self, path: &Path, what: &str) -> Result<(), Refusal> {
        let resolved = resolve(path).ok_or_else(|| Refusal::always(format!("{what} `{}` (cannot resolve)", path.display())))?;
        if resolved.starts_with(&self.root) || self.in_binds(&resolved) {
            return Ok(());
        }
        Err(Refusal::new(
            format!("{what} `{}` (outside the fleet root)", path.display()),
            format!("binds = [\"{}\"]", resolved.display()),
        ))
    }

    /// [`Self::check_path`] over `dir` and everything under it, each entry
    /// resolved where it points (a symlink out of the tree is refused). Real
    /// subdirs are walked; a missing `dir` is left to its user to report.
    fn check_tree(&self, dir: &Path, what: &str) -> Result<(), Refusal> {
        self.check_path(dir, what)?;
        let Ok(entries) = std::fs::read_dir(dir) else { return Ok(()) };
        for entry in entries.flatten() {
            let path = entry.path();
            self.check_path(&path, what)?;
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                self.check_tree(&path, what)?;
            }
        }
        Ok(())
    }

    fn in_binds(&self, resolved: &Path) -> bool {
        self.allow.binds.iter().filter_map(|b| resolve(b)).any(|b| resolved.starts_with(b))
    }

    /// A bind's host source: a runtime socket (by name, as one of
    /// [`Self::sockets`], or a dir holding one) needs `runtime-socket`;
    /// under the fleet root, a `shared-volumes` dir needs `caches` (a cache's
    /// dir) or `shared-volumes`, a `shared-files` dir `worktree-link`;
    /// anything else must pass [`Self::check_path`].
    pub fn check_bind(&self, source: &str) -> Result<(), Refusal> {
        let path = Path::new(source);
        let what = "bind mount of";
        let resolved =
            resolve(path).ok_or_else(|| Refusal::always(format!("{what} `{source}` (cannot resolve)")))?;
        let named = |p: &Path| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| SOCKET_NAMES.contains(&n));
        let socket = named(path)
            || named(&resolved)
            || self.sockets.iter().any(|s| {
                let s = resolve(s).unwrap_or_else(|| s.clone());
                s.starts_with(&resolved)
            });
        if socket && !self.allow.runtime_socket {
            return Err(Refusal::new(format!("runtime socket mount `{source}`"), "runtime-socket = true"));
        }
        if let Ok(rel) = resolved.strip_prefix(&self.root) {
            let comps: Vec<&str> = rel.components().filter_map(|c| c.as_os_str().to_str()).collect();
            if let Some(i) = comps.iter().position(|c| *c == "shared-volumes") {
                let cache = comps.get(i + 1).is_some_and(|sub| {
                    crate::config::SUPPORTED_CACHES
                        .iter()
                        .any(|c| crate::config::known_cache(c).is_some_and(|(dir, ..)| dir == *sub))
                });
                if cache && !self.allow.caches && !self.allow.shared_volumes {
                    return Err(Refusal::new(format!("cache mount `{source}` (`caches`)"), "caches = true"));
                }
                if !cache && !self.allow.shared_volumes {
                    return Err(Refusal::new(format!("shared-volumes mount `{source}`"), "shared-volumes = true"));
                }
            }
            if comps.contains(&"shared-files") && !self.allow.worktree_link {
                return Err(Refusal::new(format!("`worktree-link` store mount `{source}`"), "worktree-link = true"));
            }
        }
        self.check_path(path, what)
    }

    /// A `--mount` value: `type` `bind` (the default here, stricter than
    /// docker's), `volume` (named or anonymous) or `tmpfs`, with only the
    /// keys devsandbox writes; a bind's source passes [`Self::check_bind`].
    /// Driver options (`volume-opt`, which can bind host paths) are refused.
    pub fn check_mount(&self, value: &str) -> Result<(), Refusal> {
        let (mut kind, mut source) = (None, None);
        for part in value.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let (key, v) = part.split_once('=').map_or((part, None), |(k, v)| (k.trim(), Some(v.trim())));
            match key {
                "type" => kind = v,
                "source" | "src" => source = v,
                "target" | "destination" | "dst" | "readonly" | "ro" | "consistency" => {}
                "relabel" if v == Some("shared") => {}
                _ => return Err(Refusal::always(format!("mount option `{part}` in `{value}`"))),
            }
        }
        match kind.unwrap_or("bind") {
            "bind" => match source {
                Some(source) => self.check_bind(source),
                None => Err(Refusal::always(format!("bind mount without a source `{value}`"))),
            },
            "volume" | "tmpfs" => Ok(()),
            other => Err(Refusal::always(format!("mount type `{other}` in `{value}`"))),
        }
    }

    /// A `-v` value, `src:dst[:opts]`: an absolute (or `.`-relative) `src`
    /// is a bind ([`Self::check_bind`]), any other a named volume; options
    /// only `ro`, `rw`, `z`, `Z`.
    pub fn check_volume(&self, value: &str) -> Result<(), Refusal> {
        let mut parts = value.splitn(3, ':');
        let src = parts.next().unwrap_or_default();
        let _dst = parts.next();
        if let Some(opts) = parts.next() {
            if let Some(bad) = opts.split(',').find(|o| !matches!(*o, "ro" | "rw" | "z" | "Z")) {
                return Err(Refusal::always(format!("volume option `{bad}` in `{value}`")));
            }
        }
        if src.starts_with('/') || src.starts_with('.') || src.starts_with('~') {
            return self.check_bind(src);
        }
        Ok(())
    }

    /// The final `run` argv of a container (sandbox or service), before it
    /// reaches the runtime. Flags devsandbox writes, and only those, pass,
    /// with their values checked: `--privileged`, binds (`-v`, `--mount`),
    /// `--network` (devsandbox's networks; `host` needs `host-network`),
    /// `--userns` (`keep-id…`), `--security-opt` (devsandbox's own seccomp
    /// profile). Every other flag (`--cap-add`, `--device`, `--pid`,
    /// `--ipc`, …) is refused. The first positional is the image; what
    /// follows is its command.
    pub fn check_run(&self, argv: &[String]) -> Result<(), Refusal> {
        let mut it = argv.iter();
        match it.next().map(String::as_str) {
            Some("run") => {}
            other => return Err(Refusal::always(format!("runtime command `{}`", other.unwrap_or_default()))),
        }
        while let Some(arg) = it.next() {
            if !arg.starts_with('-') {
                return Ok(()); // the image
            }
            let (flag, inline) = split_flag(arg);
            let mut value = || {
                inline
                    .map(str::to_string)
                    .or_else(|| it.next().cloned())
                    .ok_or_else(|| Refusal::always(format!("runtime flag `{flag}` without a value")))
            };
            match flag {
                "-d" | "--detach" | "--init" if inline.is_none() => {}
                "--privileged" if inline.is_none() => {
                    if !self.allow.privileged {
                        return Err(Refusal::new("privileged", "privileged = true"));
                    }
                }
                "--name" | "--label" | "-w" | "--workdir" | "-e" | "--env" | "--user" | "-u" | "--restart"
                | "--network-alias" => {
                    value()?;
                }
                "-p" | "--publish" => {
                    let v = value()?;
                    if !loopback_bound(&v) && !self.allow.publish {
                        return Err(Refusal::new(format!("published port `{v}` (beyond loopback)"), "publish = true"));
                    }
                }
                "--userns" => {
                    let v = value()?;
                    if !v.starts_with("keep-id") {
                        return Err(Refusal::always(format!("`--userns {v}`")));
                    }
                }
                "--security-opt" => {
                    let v = value()?;
                    if !self.own_seccomp(&v) {
                        return Err(Refusal::always(format!("`--security-opt {v}`")));
                    }
                }
                "--network" | "--net" => {
                    let v = value()?;
                    self.check_network(&v)?;
                }
                "-v" | "--volume" => self.check_volume(&value()?)?,
                "--mount" => self.check_mount(&value()?)?,
                _ => return Err(Refusal::always(format!("runtime flag `{flag}`"))),
            }
        }
        Err(Refusal::always("a `run` without an image"))
    }

    /// The `build` argv of a sandbox's or service's `build` (the feature
    /// image's own builds use a context devsandbox generates): the
    /// dockerfile and the context pass [`Self::check_path`]; `--network host`
    /// needs `host-network`; flags beyond `-t`, `-f`, `--build-arg`,
    /// `--target`, `--cache-from`, `--platform`, `--progress`, `--pull`,
    /// `--no-cache`, `-q` (what `build.options` might add: `--secret`,
    /// `--ssh`, `--build-context`, `--allow`, …) are refused.
    pub fn check_build(&self, argv: &[String]) -> Result<(), Refusal> {
        let mut it = argv.iter();
        match it.next().map(String::as_str) {
            Some("build") => {}
            other => return Err(Refusal::always(format!("runtime command `{}`", other.unwrap_or_default()))),
        }
        let mut context = None;
        while let Some(arg) = it.next() {
            if !arg.starts_with('-') || arg == "-" {
                if context.replace(arg).is_some() {
                    return Err(Refusal::always(format!("a second build context `{arg}`")));
                }
                continue;
            }
            let (flag, inline) = split_flag(arg);
            let mut value = || {
                inline
                    .map(str::to_string)
                    .or_else(|| it.next().cloned())
                    .ok_or_else(|| Refusal::always(format!("build flag `{flag}` without a value")))
            };
            match flag {
                "--pull" | "--no-cache" | "-q" | "--quiet" if inline.is_none() => {}
                "-t" | "--tag" | "--build-arg" | "--target" | "--cache-from" | "--platform" | "--progress" => {
                    value()?;
                }
                "-f" | "--file" => self.check_path(Path::new(&value()?), "build file")?,
                "--network" => match value()?.as_str() {
                    "default" | "none" | "bridge" => {}
                    "host" if self.allow.host_network => {}
                    "host" => return Err(Refusal::new("`--network host` (build)", "host-network = true")),
                    other => return Err(Refusal::always(format!("build `--network {other}`"))),
                },
                _ => return Err(Refusal::always(format!("build flag `{flag}`"))),
            }
        }
        match context.map(String::as_str) {
            Some("-") => Err(Refusal::always("a build context from stdin")),
            Some(context) => self.check_path(Path::new(context), "build context"),
            None => Err(Refusal::always("a build without a context")),
        }
    }

    fn check_network(&self, network: &str) -> Result<(), Refusal> {
        if network.starts_with(&format!("{NAME_PREFIX}net-")) {
            return Ok(());
        }
        if network == "host" {
            if self.allow.host_network {
                return Ok(());
            }
            return Err(Refusal::new("`--network host`", "host-network = true"));
        }
        Err(Refusal::always(format!("`--network {network}`")))
    }

    /// `seccomp=<path>` of a profile devsandbox writes: `seccomp-*.json`
    /// directly in the data dir (`runtime::seccomp`).
    fn own_seccomp(&self, opt: &str) -> bool {
        let Some(path) = opt.strip_prefix("seccomp=").map(Path::new) else { return false };
        let named = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("seccomp-") && n.ends_with(".json"));
        named && resolve(path).and_then(|p| p.parent().map(|d| d == self.data)).unwrap_or(false)
    }
}

/// `--flag=value` as `("--flag", Some("value"))`; anything else as is.
fn split_flag(arg: &str) -> (&str, Option<&str>) {
    match arg.split_once('=') {
        Some((flag, value)) if arg.starts_with("--") => (flag, Some(value)),
        _ => (arg, None),
    }
}

/// `path` as the host resolves it: made absolute, its deepest existing
/// ancestor canonicalized (symlinks and `..` resolved), the rest appended.
/// `None` for a `..` in the part that doesn't exist yet, which can't be
/// resolved without it.
fn resolve(path: &Path) -> Option<PathBuf> {
    let path = std::path::absolute(path).ok()?;
    let mut existing = path.as_path();
    let mut rest: Vec<&std::ffi::OsStr> = Vec::new();
    loop {
        if let Ok(canonical) = existing.canonicalize() {
            let mut out = canonical;
            for name in rest.iter().rev() {
                out.push(name);
            }
            return Some(out);
        }
        match existing.components().next_back() {
            Some(Component::Normal(name)) => rest.push(name),
            Some(Component::CurDir) => {}
            _ => return None,
        }
        existing = existing.parent()?;
    }
}

/// Whether `${localEnv:<var>}` may expand for the config at `dir`: always
/// outside a fleet root; under one only for a name (`var` up to a `:`) in
/// `allow.local-env`. An error finding the policy is a no.
pub fn local_env_allowed(dir: &Path, var: &str) -> bool {
    match Policy::for_dir(dir) {
        Ok(None) => true,
        Ok(Some(policy)) => policy.allows_local_env(var),
        Err(_) => false,
    }
}

/// The `${localEnv:…}` names `text` references (`NAME` of
/// `${localEnv:NAME}` and `${localEnv:NAME:default}`).
fn local_env_names(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("${localEnv:") {
        let after = &rest[start + "${localEnv:".len()..];
        let end = after.find('}').unwrap_or(after.len());
        out.push(after[..end].split(':').next().unwrap_or_default());
        rest = &after[end..];
    }
    out
}

/// Every string in `value`, keys included.
fn strings<'a>(value: &'a toml::Value, out: &mut Vec<&'a str>) {
    match value {
        toml::Value::String(s) => out.push(s),
        toml::Value::Array(items) => items.iter().for_each(|v| strings(v, out)),
        toml::Value::Table(table) => table_strings(table, out),
        _ => {}
    }
}

fn table_strings<'a>(table: &'a toml::Table, out: &mut Vec<&'a str>) {
    for (key, value) in table {
        out.push(key);
        strings(value, out);
    }
}

/// A service spec's `-p` with no host address bound to loopback:
/// `5432` → `127.0.0.1::5432`, `8080:80` → `127.0.0.1:8080:80`; one with an
/// address is kept (and [`Policy::check_run`] judges it).
pub fn loopback_publish(spec: &str) -> String {
    if spec.starts_with('[') || spec.matches(':').count() >= 2 {
        return spec.to_string();
    }
    match spec.contains(':') {
        true => format!("127.0.0.1:{spec}"),
        false => format!("127.0.0.1::{spec}"),
    }
}

/// Whether a `-p` value's host side is bound to a loopback address.
fn loopback_bound(spec: &str) -> bool {
    spec.starts_with("127.0.0.1:") || spec.starts_with("[::1]:")
}

/// [`Policy::for_dir`] over `dir`, then `f` on the policy when there is one.
pub fn enforce(dir: &Path, f: impl FnOnce(&Policy) -> Result<(), Refusal>) -> Result<()> {
    if let Some(policy) = Policy::for_dir(dir)? {
        f(&policy)?;
    }
    Ok(())
}

/// [`enforce`] of [`Policy::check_run`] / [`Policy::check_build`] over an
/// argv about to be run for the config at `dir`.
pub fn enforce_argv(dir: &Path, argv: &[String]) -> Result<()> {
    enforce(dir, |p| match argv.first().map(String::as_str) {
        Some("build") => p.check_build(argv),
        _ => p.check_run(argv),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temp `<data>/devsandbox` with `fleet/me@hub/cfg` and an `outside`
    /// dir next to it. Returns (data, fleet root, outside).
    fn fixture(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!("dsv-policy-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let data = base.join("data/devsandbox");
        let root = data.join("fleet/me@hub");
        std::fs::create_dir_all(root.join("cfg/repo")).unwrap();
        std::fs::create_dir_all(base.join("outside")).unwrap();
        (data.canonicalize().unwrap(), root.canonicalize().unwrap(), base.join("outside").canonicalize().unwrap())
    }

    fn policy(data: &Path, root: &Path, allow: Allow) -> Policy {
        Policy { root: root.to_path_buf(), data: data.to_path_buf(), allow, sockets: runtime_sockets(None, None, None, None) }
    }

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    fn run_with(extra: &[&str]) -> Vec<String> {
        let mut v = argv(&["run", "-d", "--name", "devsandbox-x", "--label", "a=b", "-w", "/w"]);
        v.extend(extra.iter().map(|s| s.to_string()));
        v.extend(argv(&["alpine", "sh", "-c", "--privileged"]));
        v
    }

    #[test]
    fn detects_fleet_roots_by_path() {
        let (data, root, outside) = fixture("detect");
        let cfg = root.join("cfg");
        assert_eq!(fleet_root_of(&data, &cfg).unwrap(), Some(root.clone()));
        assert_eq!(fleet_root_of(&data, &root).unwrap(), Some(root.clone()));
        assert_eq!(fleet_root_of(&data, &data.join("fleet")).unwrap(), Some(data.join("fleet")));
        assert_eq!(fleet_root_of(&data, &outside).unwrap(), None);
        assert_eq!(fleet_root_of(&data, &data).unwrap(), None);
        // A symlink into the fleet dir is under it; `..` out of it isn't.
        let link = outside.join("link");
        std::os::unix::fs::symlink(&cfg, &link).unwrap();
        assert_eq!(fleet_root_of(&data, &link).unwrap(), Some(root.clone()));
        assert_eq!(fleet_root_of(&data, &cfg.join("../../../..")).unwrap(), None);
        // A dir that doesn't exist is placed as written.
        assert_eq!(fleet_root_of(&data, &root.join("gone/x")).unwrap(), Some(root.clone()));
        assert_eq!(fleet_root_of(&data, &outside.join("gone")).unwrap(), None);
        assert!(fleet_root_of(&data, &root.join("gone/../x")).is_err());
        // No fleet dir: nothing is under it.
        let (data2, _, outside2) = fixture("detect-none");
        std::fs::remove_dir_all(data2.join("fleet")).unwrap();
        assert_eq!(fleet_root_of(&data2, &outside2).unwrap(), None);
        // `detect` reads `[allow]` off lend.toml next to it.
        std::fs::write(data.join("lend.toml"), "state = \"on\"\nhub = \"me@hub\"\n[allow]\nprivileged = true\n").unwrap();
        let p = Policy::detect(&data, &cfg, Vec::new()).unwrap().unwrap();
        assert!(p.allow.privileged && !p.allow.host_network);
        assert!(Policy::detect(&data, &outside, Vec::new()).unwrap().is_none());
        std::fs::write(data.join("lend.toml"), "[allow]\nprivileged = 1\n").unwrap();
        assert!(Policy::detect(&data, &cfg, Vec::new()).is_err(), "a bad [allow] fails, not the defaults");
        std::fs::write(data.join("lend.toml"), "[allow]\nbinds = [\"rel\"]\n").unwrap();
        assert!(Policy::detect(&data, &cfg, Vec::new()).is_err());
    }

    #[test]
    fn local_env_names_and_loopback_publish() {
        assert_eq!(local_env_names("a${localEnv:A}b${localEnv:B:def}${containerEnv:C}${localEnv:D"), ["A", "B", "D"]);
        assert!(local_env_names("${localenv:A} $localEnv:B").is_empty());
        for (spec, want) in [
            ("5432", "127.0.0.1::5432"),
            ("5432/udp", "127.0.0.1::5432/udp"),
            ("8080:80", "127.0.0.1:8080:80"),
            ("8000-8010:8000-8010", "127.0.0.1:8000-8010:8000-8010"),
            ("127.0.0.1:8080:80", "127.0.0.1:8080:80"),
            ("0.0.0.0:8080:80", "0.0.0.0:8080:80"),
            ("[::1]:8080:80", "[::1]:8080:80"),
        ] {
            assert_eq!(loopback_publish(spec), want, "{spec}");
        }
        assert!(loopback_bound("127.0.0.1::5432") && loopback_bound("[::1]:1:1"));
        assert!(!loopback_bound("127.0.0.10:1:1") && !loopback_bound("5432") && !loopback_bound("[::]:1:1"));
    }

    #[test]
    fn the_refusal_names_the_item_and_its_allowance() {
        let r = Refusal::new("privileged", "privileged = true");
        assert_eq!(
            r.to_string(),
            "refused by this machine's lending policy: privileged (allow with privileged = true under [allow] in lend.toml)"
        );
        assert!(Refusal::always("runtime flag `--cap-add`").to_string().ends_with("(no lend.toml allowance permits it)"));
    }

    #[test]
    fn run_argv_items_are_refused_and_each_allowance_permits_its_own() {
        let (data, root, outside) = fixture("run");
        let inside = root.join("cfg/repo");
        let sv = root.join("cfg/shared-volumes");
        let sock = outside.join("docker.sock");
        let inside_s = inside.display().to_string();
        let outside_s = outside.display().to_string();
        let ws = format!("{inside_s}:/w");
        let seccomp = format!("seccomp={}", data.join("seccomp-0123.json").display());
        // Pass with no allowance.
        let fine: Vec<Vec<String>> = vec![
            vec!["-v".into(), ws.clone()],
            vec!["-v".into(), format!("{ws}:ro,z")],
            vec!["-v".into(), "named:/data".into()],
            vec!["--mount".into(), format!("type=bind,source={inside_s},target=/x,readonly")],
            vec!["--mount".into(), format!("type=bind,source={inside_s}/new/dir,target=/x,relabel=shared")],
            vec!["--mount".into(), "type=volume,source=devsandbox-x-cache,target=/c".into()],
            vec!["--mount".into(), "type=volume,target=/anon".into()],
            vec!["--mount".into(), "type=tmpfs,target=/t".into()],
            vec!["--network".into(), "devsandbox-net-abc-x".into()],
            vec!["--network=devsandbox-net-abc".into()],
            vec!["--userns".into(), "keep-id:uid=1000,gid=1000".into()],
            vec!["--security-opt".into(), seccomp.clone()],
            vec!["--init".into(), "--restart".into(), "unless-stopped".into(), "-e".into(), "A=--privileged".into()],
            vec!["--user".into(), "root".into(), "-p".into(), "127.0.0.1::5432".into(), "--network-alias".into(), "db".into()],
            vec!["-p".into(), "127.0.0.1:5432:5432".into(), "--publish=[::1]:8080:80".into()],
        ];
        let none = policy(&data, &root, Allow::default());
        for extra in &fine {
            let extra: Vec<&str> = extra.iter().map(String::as_str).collect();
            assert_eq!(none.check_run(&run_with(&extra)), Ok(()), "{extra:?}");
        }
        // (args, the allowance that permits them; None: nothing does).
        let all_but = |f: &dyn Fn(&mut Allow)| {
            let mut a = Allow {
                privileged: true,
                runtime_socket: true,
                host_network: true,
                caches: true,
                shared_volumes: true,
                worktree_link: true,
                global_services: true,
                dispatcher: true,
                inbox: true,
                initialize_command: true,
                publish: true,
                local_env: vec!["X".into()],
                binds: vec![outside.clone()],
            };
            f(&mut a);
            a
        };
        type Set = fn(&mut Allow);
        let denied: Vec<(Vec<String>, &str, Option<(Set, Set)>)> = vec![
            (argv(&["--privileged"]), "privileged = true", Some((|a| a.privileged = true, |a| a.privileged = false))),
            (
                vec!["-v".into(), format!("{outside_s}:/o")],
                "binds = [",
                Some((|a| a.binds = vec![std::env::temp_dir()], |a| a.binds.clear())),
            ),
            (
                vec!["--mount".into(), format!("type=bind,source={},target=/s", sock.display())],
                "runtime-socket = true",
                Some((
                    |a| {
                        a.runtime_socket = true;
                        a.binds = vec![std::env::temp_dir()];
                    },
                    |a| a.runtime_socket = false,
                )),
            ),
            (
                vec!["--mount".into(), "type=bind,source=/var/run/docker.sock,target=/s".into()],
                "runtime-socket = true",
                None,
            ),
            (argv(&["--network", "host"]), "host-network = true", Some((|a| a.host_network = true, |a| a.host_network = false))),
            (argv(&["-p", "5432"]), "publish = true", Some((|a| a.publish = true, |a| a.publish = false))),
            (argv(&["-p", "8080:80"]), "publish = true", None),
            (argv(&["--publish=0.0.0.0:8080:80"]), "publish = true", None),
            (argv(&["-p", "[::]:8080:80"]), "publish = true", None),
            (argv(&["-p", "192.168.1.2:8080:80"]), "publish = true", None),
            (
                vec!["--mount".into(), format!("type=bind,source={},target=/c", sv.join("cargo").display())],
                "caches = true",
                Some((|a| a.caches = true, |a| {
                    a.caches = false;
                    a.shared_volumes = false;
                })),
            ),
            (
                vec!["--mount".into(), format!("type=bind,source={},target=/h", sv.join("history/x").display())],
                "shared-volumes = true",
                Some((|a| a.shared_volumes = true, |a| a.shared_volumes = false)),
            ),
            (
                vec!["--mount".into(), format!("type=bind,source={},target=/l", root.join("cfg/shared-files/r-1").display())],
                "worktree-link = true",
                Some((|a| a.worktree_link = true, |a| a.worktree_link = false)),
            ),
            // `..` and symlink escapes, nothing but `binds` permits them.
            (vec!["-v".into(), format!("{inside_s}/../../../../../../outside:/o")], "binds = [", None),
            (vec!["-v".into(), format!("{inside_s}/missing/../../x:/o")], "", None),
        ];
        for (extra, allowance, toggles) in &denied {
            let extra: Vec<&str> = extra.iter().map(String::as_str).collect();
            let err = none.check_run(&run_with(&extra)).unwrap_err();
            assert!(err.to_string().contains(allowance), "{extra:?}: {err}");
            if let Some((grant, revoke)) = toggles {
                // Its own allowance alone permits it …
                let mut only = Allow::default();
                grant(&mut only);
                assert_eq!(policy(&data, &root, only).check_run(&run_with(&extra)), Ok(()), "{extra:?}");
                // … and every other one doesn't.
                let rest = all_but(&|a| revoke(a));
                assert!(policy(&data, &root, rest).check_run(&run_with(&extra)).is_err(), "{extra:?}");
            }
        }
        // A symlink inside the root pointing out is outside.
        let link = inside.join("escape");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let err = none.check_run(&run_with(&["-v", &format!("{}:/e", link.display())])).unwrap_err();
        assert!(err.to_string().contains("outside the fleet root"), "{err}");
        // A dir holding a runtime socket needs `runtime-socket` even when bound.
        let p = Policy { sockets: vec![sock.clone()], ..policy(&data, &root, Allow { binds: vec![outside.clone()], ..Default::default() }) };
        assert!(p.check_run(&run_with(&["-v", &format!("{outside_s}:/o")])).unwrap_err().to_string().contains("runtime-socket"));
        // Flags devsandbox never writes: refused, no allowance.
        let all = policy(&data, &root, all_but(&|_| {}));
        for extra in [
            &["--cap-add", "SYS_ADMIN"][..],
            &["--device", "/dev/kvm"],
            &["--pid", "host"],
            &["--ipc=host"],
            &["--userns", "host"],
            &["--security-opt", "seccomp=unconfined"],
            &["--security-opt", "apparmor=unconfined"],
            &["--security-opt", "seccomp=/tmp/seccomp-1.json"],
            &["--volumes-from", "other"],
            &["--network", "container:other"],
            &["--mount", "type=volume,source=v,target=/v,volume-opt=device=/"],
            &["--mount", "type=image,source=x,target=/i"],
            &["-v", "/x:/y:rshared"],
            &["-it"],
        ] {
            let err = all.check_run(&run_with(extra)).unwrap_err();
            assert_eq!(err.allow, None, "{extra:?}: {err}");
        }
        assert!(none.check_run(&argv(&["create", "alpine"])).is_err());
        assert!(none.check_run(&argv(&["run", "-d"])).is_err());
    }

    #[test]
    fn build_argv_paths_and_flags() {
        let (data, root, outside) = fixture("build");
        let none = policy(&data, &root, Allow::default());
        let cfg = root.join("cfg");
        let df = cfg.join("Dockerfile").display().to_string();
        let ctx = cfg.display().to_string();
        let ok = argv(&["build", "-t", "t", "-f", &df, "--build-arg", "A=1", "--target", "x", "--cache-from", "i", "--pull", &ctx]);
        assert_eq!(none.check_build(&ok), Ok(()));
        let out = outside.display().to_string();
        for (bad, allowance) in [
            (argv(&["build", "-t", "t", "-f", &df, &out]), Some("binds = [")),
            (argv(&["build", "-t", "t", "-f", &format!("{out}/Dockerfile"), &ctx]), Some("binds = [")),
            (argv(&["build", "-t", "t", "-f", &df, &format!("{ctx}/../../../../../outside")]), Some("binds = [")),
            (argv(&["build", "-t", "t", "--network", "host", "-f", &df, &ctx]), Some("host-network = true")),
            (argv(&["build", "-t", "t", "--build-context", &format!("x={out}"), "-f", &df, &ctx]), None),
            (argv(&["build", "-t", "t", "--secret", "id=a,src=/etc/shadow", "-f", &df, &ctx]), None),
            (argv(&["build", "-t", "t", "--ssh", "default", "-f", &df, &ctx]), None),
            (argv(&["build", "-t", "t", "-f", &df, &ctx, &ctx]), None),
            (argv(&["build", "-t", "t", "-f", &df, "-"]), None),
        ] {
            let err = none.check_build(&bad).unwrap_err();
            assert_eq!(err.allow.as_deref().map(|a| a.split('"').next().unwrap()), allowance, "{bad:?}: {err}");
        }
        let net = policy(&data, &root, Allow { host_network: true, ..Default::default() });
        assert_eq!(net.check_build(&argv(&["build", "--network=host", "-f", &df, &ctx])), Ok(()));
        let bound = policy(&data, &root, Allow { binds: vec![outside.clone()], ..Default::default() });
        assert_eq!(bound.check_build(&argv(&["build", "-f", &df, &out])), Ok(()));
    }

    #[test]
    fn config_items_are_refused_and_each_allowance_permits_its_own() {
        let (data, root, outside) = fixture("config");
        let cfg = root.join("cfg");
        std::fs::create_dir_all(cfg.join("features/ok")).unwrap();
        std::fs::write(cfg.join("features/ok/install.sh"), "").unwrap();
        std::fs::create_dir_all(cfg.join("features/leak")).unwrap();
        std::fs::write(outside.join("secret"), "").unwrap();
        std::os::unix::fs::symlink(outside.join("secret"), cfg.join("features/leak/install.sh")).unwrap();
        let rel_out = "../../../../../outside";
        let sandbox = |extra: &str| {
            let text = format!(
                "[services.g]\nimage = \"redis\"\nscope = \"global\"\n[services.i]\nimage = \"redis\"\n\
                 [sandbox.s]\nfolder = \"repo\"\nimage = \"alpine\"\n{extra}\n"
            );
            let config = Config::parse(&text).unwrap();
            let s = config.resolve_sandbox("s").unwrap();
            (config, s)
        };
        let none = policy(&data, &root, Allow::default());
        let check = |p: &Policy, extra: &str| {
            let (config, s) = sandbox(extra);
            p.check_config(&cfg, &config, &s)
        };
        for fine in [
            "",
            "services = [\"i\"]",
            "privileged = false",
            "features = { \"./features/ok\" = {} }",
            "features = { \"ghcr.io/devcontainers/features/node:1\" = {} }",
            "mounts = [\"source=/etc,target=/e\"]", // a bind: the mounts' own check
        ] {
            assert_eq!(check(&none, fine), Ok(()), "{fine}");
        }
        let items: [(&str, &str, fn(&mut Allow)); 9] = [
            ("initializeCommand = \"true\"", "initialize-command = true", |a| a.initialize_command = true),
            ("dispatcher = {}", "dispatcher = true", |a| a.dispatcher = true),
            ("inbox = true", "inbox = true", |a| a.inbox = true),
            ("caches = [\"cargo\"]", "caches = true", |a| a.caches = true),
            ("persist-shell-history = true", "shared-volumes = true", |a| a.shared_volumes = true),
            ("worktree-link = [\".env\"]", "worktree-link = true", |a| a.worktree_link = true),
            ("privileged = true", "privileged = true", |a| a.privileged = true),
            ("services = [\"i\", \"g\"]", "global-services = true", |a| a.global_services = true),
            ("features = { \"./features/leak\" = {} }", "binds = [", |a| a.binds = vec![std::env::temp_dir()]),
        ];
        for (i, (extra, allowance, grant)) in items.iter().enumerate() {
            let err = check(&none, extra).unwrap_err();
            assert!(err.to_string().contains(allowance), "{extra}: {err}");
            let mut own = Allow::default();
            grant(&mut own);
            assert_eq!(check(&policy(&data, &root, own), extra), Ok(()), "{extra}");
            for (j, (_, _, other)) in items.iter().enumerate() {
                if i != j {
                    let mut a = Allow::default();
                    other(&mut a);
                    assert!(check(&policy(&data, &root, a), extra).is_err(), "{extra} permitted by item {j}");
                }
            }
        }
        // `${localEnv:…}` anywhere (a template, a key, a service), by name.
        let gh = Allow { local_env: vec!["GH".into()], ..Default::default() };
        for extra in [
            "containerEnv = { T = \"${localEnv:GH}\" }",
            "containerEnv = { T = \"${localEnv:GH:fallback}\" }",
            "mounts = [\"source=${localEnv:GH}/x,target=/x\"]",
            "folders = { \"/x-${localEnv:GH}\" = \"repo\" }",
            "worktree-branch = \"b-${localEnv:GH}\"",
            "services = [\"e\"]",
            "extends = \"t\"",
        ] {
            let text = format!(
                "[template.t]\nremoteEnv = {{ A = \"${{localEnv:GH}}\" }}\n\
                 [services.e]\nimage = \"redis\"\nenv = {{ P = \"${{localEnv:GH}}\" }}\n\
                 [sandbox.s]\nfolder = \"repo\"\nimage = \"alpine\"\n{extra}\n"
            );
            let config = Config::parse(&text).unwrap();
            let s = config.resolve_sandbox("s").unwrap();
            let err = none.check_config(&cfg, &config, &s).unwrap_err();
            assert_eq!(err.allow.as_deref(), Some("local-env = [\"GH\"]"), "{extra}: {err}");
            assert!(err.to_string().contains("`${localEnv:GH}`"), "{err}");
            assert_eq!(policy(&data, &root, gh.clone()).check_config(&cfg, &config, &s), Ok(()), "{extra}");
            let other = Allow { local_env: vec!["GH2".into(), "G".into()], ..Default::default() };
            assert!(policy(&data, &root, other).check_config(&cfg, &config, &s).is_err(), "{extra}");
        }
        // An unrelated sandbox's reference doesn't count.
        let config = Config::parse(
            "[sandbox.s]\nfolder = \"repo\"\nimage = \"alpine\"\n[sandbox.o]\nfolder = \"repo\"\nimage = \"a\"\ncontainerEnv = { T = \"${localEnv:GH}\" }\n",
        )
        .unwrap();
        assert_eq!(none.check_config(&cfg, &config, &config.resolve_sandbox("s").unwrap()), Ok(()));
        let p = policy(&data, &root, gh);
        assert!(p.allows_local_env("GH") && p.allows_local_env("GH:default") && !p.allows_local_env("GHX"));
        // Outside a fleet root nothing changes.
        assert!(local_env_allowed(&outside, "ANY"));
        let err = check(&none, &format!("features = {{ \"{rel_out}\" = {{}} }}")).unwrap_err();
        assert!(err.to_string().contains("outside the fleet root"), "{err}");
        // `check_path`: the folder rule.
        assert_eq!(none.check_path(&cfg.join("repo"), "folder"), Ok(()));
        assert!(none.check_path(&cfg.join(rel_out), "folder").is_err());
    }
}
