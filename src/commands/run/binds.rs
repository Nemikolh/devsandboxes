//! What `run` refuses to bind, and the binds it warns of
//! (docs/plans/active/credential-broker.md, *What a sandbox mounts*; *Goal*
//! for sandboxes that are the host).
//!
//! Refused on every host, read-only too, with or without credential claims
//! (an edit could hand a sibling, or the next `rebuild`, claims): a bind
//! whose source covers a config root (the one running and every one an
//! instance in `state.toml` came from), covers a whole `shared-volumes/`
//! dir, or covers or lies in another instance's worktree or per-instance
//! bind dir; and a mount of another instance's per-instance named volume.
//! Paths are compared in `key_guard`'s forms ([`key_guard::source_forms`]),
//! which already refuses what it guards.
//!
//! Checked on `folder` and `folders` before anything is created
//! ([`Fence::check_folders`]: a config inside its own `folder` is refused
//! even when the instance gets a worktree, whose companion `.git` is the
//! config's), on `mounts` before their sources are created, and over the
//! final runtime argv ([`check_argv`]), the authority.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::key_guard;
use crate::state::{Instance, State};
use crate::worker_policy::{self, Arg};

use super::mounts::ResolvedFolder;

/// What a refused bind reaches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Fenced {
    ConfigRoot,
    SharedVolumes,
    /// Another instance's worktree (its name).
    Worktree(String),
    /// Another instance's per-instance bind dir or shell history.
    Volume(String),
    /// Another instance's per-instance named volume.
    NamedVolume(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Refused {
    /// What the source is (`bind mount of`, `` `folder` ``, …).
    pub what: String,
    /// The source as written.
    pub source: String,
    /// The fenced path it reaches (the volume's name for a named volume).
    pub reaches: String,
    pub fenced: Fenced,
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let (what, source, reaches) = (&self.what, &self.source, &self.reaches);
        match &self.fenced {
            Fenced::ConfigRoot => write!(
                f,
                "refused: {what} `{source}` covers config root `{reaches}`, read-only too: no sandbox may \
                 reach a config (bind the files it needs read-only, one bind each; a config inside its own \
                 `folder`, as with `folder = \".\"`, moves beside the repo)"
            ),
            Fenced::SharedVolumes => write!(
                f,
                "refused: {what} `{source}` covers `{reaches}`, a whole `shared-volumes/` dir, read-only too \
                 (bind the subdirectory the sandbox needs)"
            ),
            Fenced::Worktree(other) => write!(
                f,
                "refused: {what} `{source}` reaches `{reaches}`, instance `{other}`'s worktree, read-only too \
                 (bind only this instance's own folders)"
            ),
            Fenced::Volume(other) => write!(
                f,
                "refused: {what} `{source}` reaches `{reaches}`, instance `{other}`'s per-instance volume, \
                 read-only too (name a per-instance source with `${{instance}}`)"
            ),
            Fenced::NamedVolume(other) => write!(
                f,
                "refused: volume `{source}` is instance `{other}`'s per-instance volume (name it with \
                 `${{instance}}` so each instance gets its own)"
            ),
        }
    }
}

impl std::error::Error for Refused {}

/// A fenced path, as written (lexically cleaned) and resolved.
#[derive(Debug, Clone)]
struct Entry {
    path: PathBuf,
    resolved: PathBuf,
    fenced: Fenced,
}

impl Entry {
    fn new(path: &Path, fenced: Fenced) -> Entry {
        let path = key_guard::lexical(path);
        let resolved = worker_policy::resolve(&path).unwrap_or_else(|| path.clone());
        Entry { path, resolved, fenced }
    }

    /// Whether a source in the forms `forms` reaches it: covers it, or for
    /// another instance's paths lies in it too.
    fn reached_by(&self, forms: &[PathBuf]) -> bool {
        let inside = matches!(self.fenced, Fenced::Worktree(_) | Fenced::Volume(_));
        forms.iter().any(|f| {
            self.path.starts_with(f)
                || self.resolved.starts_with(f)
                || (inside && (f.starts_with(&self.path) || f.starts_with(&self.resolved)))
        })
    }
}

/// The fence of one instance's binds: every config root, their
/// `shared-volumes/`, the other instances' worktrees and volumes; and what
/// other instances share code through (for [`Fence::shared_writable`]).
#[derive(Debug, Clone, Default)]
pub(crate) struct Fence {
    entries: Vec<Entry>,
    /// Other instances' per-instance named volumes, with their owner.
    volumes: Vec<(String, String)>,
    /// Paths other instances run code from, with whose they are.
    shared: Vec<(Entry, String)>,
    home: Option<PathBuf>,
}

impl Fence {
    /// Over config roots `roots` and the other instances `others` (name,
    /// record); `home` expands a `~` source.
    pub(crate) fn new(roots: &[PathBuf], others: &[(&str, &Instance)], home: Option<PathBuf>) -> Fence {
        let mut roots: Vec<PathBuf> = roots.iter().map(|r| key_guard::lexical(r)).collect();
        roots.sort();
        roots.dedup();
        let mut fence = Fence { home, ..Fence::default() };
        fence.entries.extend(roots.iter().map(|r| Entry::new(r, Fenced::ConfigRoot)));
        fence.entries.extend(roots.iter().map(|r| Entry::new(&r.join("shared-volumes"), Fenced::SharedVolumes)));
        for root in &roots {
            for name in crate::config::SUPPORTED_CACHES {
                if let Some((sub, ..)) = crate::config::known_cache(name) {
                    let whose = format!("every sandbox of `{}` with the `{name}` cache", root.display());
                    fence.shared.push((Entry::new(&root.join("shared-volumes").join(sub), Fenced::ConfigRoot), whose));
                }
            }
        }
        for (name, i) in others {
            let worktree = |p: &Path| Entry::new(p, Fenced::Worktree(name.to_string()));
            let volume = |p: &Path| Entry::new(p, Fenced::Volume(name.to_string()));
            fence.entries.extend(i.worktree.as_deref().map(worktree));
            fence.entries.extend(i.folders.iter().filter_map(|f| f.worktree.as_deref()).map(worktree));
            if let Some(root) = &i.config_dir {
                let dir = root.join(".worktrees");
                fence.entries.push(worktree(&dir.join(&i.instance_id)));
                fence.entries.push(worktree(&dir.join(format!("{}.folders", i.instance_id))));
            }
            fence.entries.extend(i.bind_dirs.iter().map(|p| volume(p)));
            fence.entries.extend(i.shell_history.as_deref().and_then(Path::parent).map(volume));
            fence.volumes.extend(i.volumes.iter().map(|v| (v.clone(), name.to_string())));
            let checkout = |p: &Path| (Entry::new(p, Fenced::Worktree(name.to_string())), format!("instance `{name}`'s checkout"));
            fence.shared.push(checkout(&i.base_folder));
            if i.worktree.is_none() {
                fence.shared.push(checkout(&i.folder));
            }
            fence.shared.extend(i.folders.iter().map(|f| checkout(&f.base)));
        }
        fence
    }

    /// The fence of instance `instance_id` of the config at `dir`, from
    /// `state` and the process env.
    pub(crate) fn for_run(dir: &Path, state: &State, instance_id: &str) -> Fence {
        let current = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        let roots: Vec<PathBuf> =
            std::iter::once(current).chain(state.instances.values().filter_map(|i| i.config_dir.clone())).collect();
        let others: Vec<(&str, &Instance)> = state
            .instances
            .iter()
            .filter(|(_, i)| i.instance_id != instance_id)
            .map(|(name, i)| (name.as_str(), i))
            .collect();
        Fence::new(&roots, &others, std::env::var_os("HOME").map(PathBuf::from))
    }

    /// A host path `source` the container gets as `what`.
    pub(crate) fn check_path(&self, source: &str, what: &str) -> Result<(), Refused> {
        let forms = key_guard::source_forms(source, self.home.as_deref());
        match self.entries.iter().find(|e| e.reached_by(&forms)) {
            Some(e) => Err(Refused {
                what: what.to_string(),
                source: source.to_string(),
                reaches: e.path.display().to_string(),
                fenced: e.fenced.clone(),
            }),
            None => Ok(()),
        }
    }

    pub(crate) fn check_volume(&self, name: &str) -> Result<(), Refused> {
        match self.volumes.iter().find(|(v, _)| v == name) {
            Some((_, owner)) => Err(Refused {
                what: "volume".to_string(),
                source: name.to_string(),
                reaches: name.to_string(),
                fenced: Fenced::NamedVolume(owner.clone()),
            }),
            None => Ok(()),
        }
    }

    pub(crate) fn check_mount(&self, value: &str) -> Result<(), Refused> {
        let fields = worker_policy::mount_fields(value);
        match (fields.kind, fields.source) {
            ("bind", Some(source)) => self.check_path(source, "bind mount of"),
            ("volume", Some(name)) => self.check_volume(name),
            _ => Ok(()),
        }
    }

    /// A `-v` value, `src:dst[:opts]`.
    fn check_dash_v(&self, value: &str) -> Result<(), Refused> {
        match worker_policy::volume_source(value) {
            Some(source) => self.check_path(source, "bind mount of"),
            None => match value.split_once(':') {
                Some((name, _)) => self.check_volume(name),
                None => Ok(()),
            },
        }
    }

    /// A `run` argv's binds and volumes (`-v`, `--volume`, `--mount`), up
    /// to the image, walked as `key_guard` walks it.
    pub(crate) fn check_argv(&self, argv: &[String]) -> Result<(), Refused> {
        for arg in worker_policy::args(argv, worker_policy::RUN_SWITCHES, "runtime") {
            match arg {
                Ok(Arg::Flag("-v" | "--volume", Some(value))) => self.check_dash_v(value)?,
                Ok(Arg::Flag("--mount", Some(value))) => self.check_mount(value)?,
                Ok(Arg::Flag(..)) => {}
                Ok(Arg::Positional(_)) | Err(_) => break,
            }
        }
        Ok(())
    }

    /// The config's `folder` (`folder`, canonical) and `folders` hosts.
    pub(super) fn check_folders(&self, folder: &Path, folders: &[ResolvedFolder]) -> Result<(), Refused> {
        self.check_path(&folder.to_string_lossy(), "`folder`")?;
        for f in folders {
            self.check_path(&f.host.to_string_lossy(), &format!("`folders` entry `{}`", f.target))?;
        }
        Ok(())
    }

    /// One warning per writable bind of `binds` that other instances run
    /// code from (a shared cache, another instance's checkout or its base's
    /// `.git`). Only for an instance with credential claims: without them,
    /// one sandbox writing what another runs gains nothing the broker keeps
    /// from it.
    pub(crate) fn shared_writable(&self, instance: &str, binds: &[Bind]) -> Vec<String> {
        let mut out = Vec::new();
        for bind in binds.iter().filter(|b| !b.readonly) {
            let forms = key_guard::source_forms(&bind.source, self.home.as_deref());
            if let Some((_, whose)) = self.shared.iter().find(|(e, _)| overlaps(e, &forms)) {
                out.push(format!(
                    "writable bind `{}` is shared with {whose}, which can write code that runs in it (hooks, \
                     `core.fsmonitor`, build scripts); instance `{instance}` has credential claims, so that \
                     code runs with them",
                    bind.source
                ));
            }
        }
        out
    }
}

/// Whether a source in the forms `forms` covers `e` or lies in it.
fn overlaps(e: &Entry, forms: &[PathBuf]) -> bool {
    forms.iter().any(|f| {
        e.path.starts_with(f) || e.resolved.starts_with(f) || f.starts_with(&e.path) || f.starts_with(&e.resolved)
    })
}

/// [`Fence::check_argv`] over the current `state.toml`, before `argv` (the
/// container of instance `instance_id` of the config at `dir`) runs.
pub(super) fn check_argv(dir: &Path, argv: &[String], instance_id: &str) -> Result<()> {
    Fence::for_run(dir, &State::load()?, instance_id).check_argv(argv)?;
    Ok(())
}

/// A host path the container gets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Bind {
    pub source: String,
    pub readonly: bool,
}

/// The binds among `-v` values `volumes` and `--mount` values `mounts`.
pub(crate) fn binds_of(volumes: &[String], mounts: &[String]) -> Vec<Bind> {
    let dash_v = volumes.iter().filter_map(|v| {
        let source = worker_policy::volume_source(v)?;
        let readonly = v.splitn(3, ':').nth(2).is_some_and(|opts| opts.split(',').any(|o| o == "ro"));
        Some(Bind { source: source.to_string(), readonly })
    });
    let mount = mounts.iter().filter_map(|m| {
        let fields = worker_policy::mount_fields(m);
        let source = fields.source.filter(|_| fields.kind == "bind")?;
        let readonly = m.split(',').map(str::trim).any(|p| {
            matches!(p, "readonly" | "ro" | "readonly=true" | "ro=true" | "readonly=1" | "ro=1")
        });
        Some(Bind { source: source.to_string(), readonly })
    });
    dash_v.chain(mount).collect()
}

/// Why a sandbox can act as its host (credential-broker, *Goal*): it can
/// read the host's files and use any instance's bridge, or drive the
/// desktop and answer any instance's asks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HostClass {
    Privileged,
    RuntimeSocket(String),
    Session(String),
}

impl fmt::Display for HostClass {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            HostClass::Privileged => write!(f, "it runs privileged"),
            HostClass::RuntimeSocket(s) => write!(f, "its bind `{s}` reaches the container runtime's socket"),
            HostClass::Session(s) => write!(f, "its bind `{s}` reaches the host's display or session sockets"),
        }
    }
}

/// The host's display and session sockets: X11's dir, `$XDG_RUNTIME_DIR`
/// (Wayland, the session bus and `serve`'s socket live there), and a
/// Wayland display or D-Bus session bus named elsewhere.
pub(crate) fn session_sockets(
    xdg_runtime_dir: Option<&Path>,
    wayland_display: Option<&str>,
    dbus_address: Option<&str>,
) -> Vec<PathBuf> {
    let mut out = vec![PathBuf::from("/tmp/.X11-unix")];
    out.extend(xdg_runtime_dir.map(Path::to_path_buf));
    out.extend(wayland_display.filter(|d| d.starts_with('/')).map(PathBuf::from));
    for address in dbus_address.unwrap_or_default().split(';') {
        if let Some(params) = address.strip_prefix("unix:") {
            out.extend(params.split(',').filter_map(|p| p.strip_prefix("path=")).map(PathBuf::from));
        }
    }
    out
}

/// What makes a sandbox with `binds` (and `privileged`) the host's equal:
/// a bind covering a runtime socket of `runtime` (or named like one,
/// `worker_policy::SOCKET_NAMES`), or covering or lying in a path of
/// `session`.
pub(crate) fn host_class(
    privileged: bool,
    binds: &[Bind],
    runtime: &[PathBuf],
    session: &[PathBuf],
    home: Option<&Path>,
) -> Vec<HostClass> {
    let entries = |paths: &[PathBuf]| -> Vec<Entry> { paths.iter().map(|p| Entry::new(p, Fenced::ConfigRoot)).collect() };
    let (runtime, session) = (entries(runtime), entries(session));
    let mut out = Vec::new();
    if privileged {
        out.push(HostClass::Privileged);
    }
    for bind in binds {
        let forms = key_guard::source_forms(&bind.source, home);
        let named = forms.iter().any(|f| {
            f.file_name().and_then(|n| n.to_str()).is_some_and(|n| worker_policy::SOCKET_NAMES.contains(&n))
        });
        if named || runtime.iter().any(|e| e.reached_by(&forms)) {
            out.push(HostClass::RuntimeSocket(bind.source.clone()));
        } else if session.iter().any(|e| overlaps(e, &forms)) {
            out.push(HostClass::Session(bind.source.clone()));
        }
    }
    out
}

/// The warning for a sandbox of class `classes` (none: no warning) on a
/// host that brokers a credential (`brokers`).
pub(crate) fn host_class_warning(instance: &str, classes: &[HostClass], brokers: bool) -> Option<String> {
    if !brokers || classes.is_empty() {
        return None;
    }
    let why: Vec<String> = classes.iter().map(ToString::to_string).collect();
    Some(format!(
        "instance `{instance}` can act as this host ({}), and this host brokers credentials: the broker's \
         per-instance limits don't hold against it",
        why.join("; ")
    ))
}

/// Whether this host brokers any credential: `credentials.toml` has an
/// entry (one that won't load counts).
#[cfg(unix)]
fn brokers_credentials() -> bool {
    let Ok(data) = crate::daemon_config::data_dir_from(std::env::var_os("XDG_DATA_HOME"), std::env::var_os("HOME"))
    else {
        return false;
    };
    crate::credentials::home::load(&crate::credentials::home::path(&data)).map_or(true, |home| !home.is_empty())
}

#[cfg(not(unix))]
fn brokers_credentials() -> bool {
    false
}

/// `run`'s bind warnings for instance `instance` (id `instance_id`) of the
/// config at `dir`, over its final `binds` and `privileged`: the host-class
/// one, and with `has_claims` the shared writable binds.
pub(super) fn warnings(
    dir: &Path,
    state: &State,
    instance: &str,
    instance_id: &str,
    binds: &[Bind],
    privileged: bool,
    has_claims: bool,
) -> Vec<String> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let session = session_sockets(
        std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).as_deref(),
        std::env::var("WAYLAND_DISPLAY").ok().as_deref(),
        std::env::var("DBUS_SESSION_BUS_ADDRESS").ok().as_deref(),
    );
    let classes = host_class(privileged, binds, &worker_policy::host_sockets(), &session, home.as_deref());
    let mut out: Vec<String> =
        host_class_warning(instance, &classes, !classes.is_empty() && brokers_credentials()).into_iter().collect();
    if has_claims {
        out.extend(Fence::for_run(dir, state, instance_id).shared_writable(instance, binds));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::FolderMount;

    /// A temp base with a config root `cfg` (and its `shared-volumes/`),
    /// a repo `repo` and `outside`; canonical.
    fn fixture(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("dsv-binds-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        for d in ["cfg/shared-volumes/cargo", "cfg/shared-volumes/gh", "cfg/mcps", "repo/.git", "outside"] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
        std::fs::write(base.join("cfg/mcps/common.json"), "{}").unwrap();
        base.canonicalize().unwrap()
    }

    fn instance(id: &str, cfg: &Path, folder: &Path) -> Instance {
        let mut i: Instance = toml::from_str(&format!(
            "sandbox = \"web\"\ninstance_id = \"{id}\"\nproject = \"p\"\ncontainer = \"devsandbox-{id}\"\n\
             folder = \"{f}\"\nbase_folder = \"{f}\"\nworkspace = \"/w\"\ncreated_unix = 0\n",
            f = folder.display()
        ))
        .unwrap();
        i.config_dir = Some(cfg.to_path_buf());
        i
    }

    fn s(p: &Path) -> String {
        p.display().to_string()
    }

    #[test]
    fn config_roots_are_refused_read_only_too() {
        let base = fixture("roots");
        let cfg = base.join("cfg");
        let other = base.join("outside/other-cfg");
        std::fs::create_dir_all(&other).unwrap();
        let fence = Fence::new(&[cfg.clone(), other.clone()], &[], Some(base.clone()));
        for source in [s(&cfg), format!("{}/", s(&cfg)), s(&base), "/".into(), format!("{}/x/..", s(&cfg)), "~".into(), "~/cfg".into()] {
            for mount in [format!("type=bind,source={source},target=/c"), format!("type=bind,source={source},target=/c,readonly")] {
                let err = fence.check_mount(&mount).unwrap_err();
                assert_eq!(err.fenced, Fenced::ConfigRoot, "{mount}");
            }
        }
        // Another root State knows.
        let err = fence.check_path(&s(&base.join("outside")), "bind mount of").unwrap_err();
        assert_eq!((err.fenced, err.reaches), (Fenced::ConfigRoot, s(&other)));
        // A symlink to the root, or above it.
        let link = base.join("link");
        std::os::unix::fs::symlink(&cfg, &link).unwrap();
        let up = base.join("outside/up");
        std::os::unix::fs::symlink(&base, &up).unwrap();
        for source in [s(&link), format!("{}/", s(&link)), s(&up)] {
            assert_eq!(fence.check_path(&source, "bind mount of").unwrap_err().fenced, Fenced::ConfigRoot, "{source}");
        }
        for source in [s(&cfg.join("mcps/common.json")), s(&cfg.join("mcps")), format!("{}/mcps/common.json", s(&link))] {
            assert_eq!(fence.check_path(&source, "bind mount of"), Ok(()), "{source}");
        }
        let err = fence.check_mount(&format!("type=bind,source={},target=/c,readonly", s(&cfg))).unwrap_err();
        let msg = err.to_string();
        assert!(msg.starts_with(&format!("refused: bind mount of `{}` covers config root `{}`", s(&cfg), s(&cfg))), "{msg}");
        assert!(msg.contains("one bind each") && msg.contains("`folder = \".\"`"), "{msg}");
    }

    #[test]
    fn a_whole_shared_volumes_dir_is_refused_a_subdir_is_not() {
        let base = fixture("sv");
        let cfg = base.join("cfg");
        let fence = Fence::new(std::slice::from_ref(&cfg), &[], None);
        let sv = cfg.join("shared-volumes");
        for source in [s(&sv), format!("{}/", s(&sv)), format!("{}/gh/..", s(&sv))] {
            let err = fence.check_path(&source, "bind mount of").unwrap_err();
            assert_eq!(err.fenced, Fenced::SharedVolumes, "{source}");
            assert!(err.to_string().contains("a whole `shared-volumes/` dir"), "{err}");
        }
        for source in [s(&sv.join("gh")), s(&sv.join("cargo")), s(&sv.join("new/sub"))] {
            assert_eq!(fence.check_path(&source, "bind mount of"), Ok(()), "{source}");
        }
    }

    #[test]
    fn other_instances_worktrees_and_volumes_are_refused() {
        let base = fixture("others");
        let (cfg, repo) = (base.join("cfg"), base.join("repo"));
        let mut other = instance("web-2", &cfg, &cfg.join(".worktrees/web-2"));
        other.worktree = Some(cfg.join(".worktrees/web-2"));
        other.base_folder = repo.clone();
        other.folders.push(FolderMount {
            target: "/workspaces/lib".into(),
            base: base.join("outside"),
            worktree: Some(cfg.join(".worktrees/web-2.folders/lib-12345678")),
        });
        other.bind_dirs.push(cfg.join("shared-volumes/dispatcher/web-2"));
        other.shell_history = Some(cfg.join("shared-volumes/history/web-2/.zsh_history"));
        other.volumes.push("dind-web-2".into());
        let fence = Fence::new(std::slice::from_ref(&cfg), &[("web-2", &other)], None);
        let wt = cfg.join(".worktrees");
        for (source, fenced) in [
            (s(&wt), Fenced::Worktree("web-2".into())),
            (s(&wt.join("web-2")), Fenced::Worktree("web-2".into())),
            (s(&wt.join("web-2/src")), Fenced::Worktree("web-2".into())),
            (s(&wt.join("web-2.folders")), Fenced::Worktree("web-2".into())),
            (s(&cfg.join("shared-volumes/dispatcher")), Fenced::Volume("web-2".into())),
            (s(&cfg.join("shared-volumes/dispatcher/web-2/x")), Fenced::Volume("web-2".into())),
            (s(&cfg.join("shared-volumes/history")), Fenced::Volume("web-2".into())),
        ] {
            assert_eq!(fence.check_path(&source, "bind mount of").unwrap_err().fenced, fenced, "{source}");
        }
        // This instance's own paths, and the other's base repo.
        for source in [s(&wt.join("web")), s(&cfg.join("shared-volumes/history/web")), s(&repo)] {
            assert_eq!(fence.check_path(&source, "bind mount of"), Ok(()), "{source}");
        }
        let err = fence.check_mount("type=volume,source=dind-web-2,target=/var/lib/docker").unwrap_err();
        assert_eq!(err.fenced, Fenced::NamedVolume("web-2".into()));
        assert!(err.to_string().contains("instance `web-2`'s per-instance volume"), "{err}");
        assert_eq!(fence.check_mount("type=volume,source=dind-web,target=/var/lib/docker"), Ok(()));
    }

    #[test]
    fn the_argv_is_checked_up_to_the_image() {
        let base = fixture("argv");
        let cfg = base.join("cfg");
        let mut other = instance("b", &cfg, &base.join("repo"));
        other.volumes.push("vol-b".into());
        let fence = Fence::new(std::slice::from_ref(&cfg), &[("b", &other)], None);
        let argv = |extra: &[&str]| -> Vec<String> {
            let mut v: Vec<String> = ["run", "-d", "--name", "devsandbox-a", "-w", "/w"].map(String::from).into();
            v.extend(extra.iter().map(|s| s.to_string()));
            v.extend(["alpine", "-v", "/:/r"].map(String::from));
            v
        };
        let primary = format!("{}:/w", s(&base.join("repo")));
        assert_eq!(fence.check_argv(&argv(&["-v", &primary, "-v", "vol-a:/v"])), Ok(()));
        let bad = format!("{}:/workspaces/cfg", s(&cfg));
        assert_eq!(fence.check_argv(&argv(&["-v", &bad])).unwrap_err().fenced, Fenced::ConfigRoot);
        assert_eq!(fence.check_argv(&argv(&[&format!("--volume={bad}:ro")])).unwrap_err().fenced, Fenced::ConfigRoot);
        assert_eq!(fence.check_argv(&argv(&["-v", "vol-b:/v"])).unwrap_err().fenced, Fenced::NamedVolume("b".into()));
        let sv = format!("type=bind,source={},target=/s,readonly", s(&cfg.join("shared-volumes")));
        assert_eq!(fence.check_argv(&argv(&["--mount", &sv])).unwrap_err().fenced, Fenced::SharedVolumes);
    }

    #[test]
    fn a_config_inside_its_own_folder_is_refused() {
        let base = fixture("dot");
        let cfg = base.join("cfg");
        let fence = Fence::new(std::slice::from_ref(&cfg), &[], None);
        let err = fence.check_folders(&cfg, &[]).unwrap_err();
        assert_eq!((err.what.as_str(), err.fenced.clone()), ("`folder`", Fenced::ConfigRoot));
        assert!(err.to_string().contains("moves beside the repo"), "{err}");
        // The repo holding the config as a subdir.
        let nested = base.join("repo/.devsandboxes");
        std::fs::create_dir_all(&nested).unwrap();
        let fence = Fence::new(&[nested], &[], None);
        assert!(fence.check_folders(&base.join("repo"), &[]).is_err());
        // A `folders` entry, and a config beside the repo.
        let fence = Fence::new(std::slice::from_ref(&cfg), &[], None);
        let lib = ResolvedFolder { target: "/workspaces/.cfg".into(), host: cfg.clone(), mode: crate::config::FolderWorktree::Auto };
        let err = fence.check_folders(&base.join("repo"), &[lib]).unwrap_err();
        assert_eq!(err.what, "`folders` entry `/workspaces/.cfg`");
        assert_eq!(fence.check_folders(&base.join("repo"), &[]), Ok(()));
    }

    #[test]
    fn binds_of_reads_sources_and_read_only() {
        let binds = binds_of(
            &["/a:/a".into(), "/b:/b:ro".into(), "/c:/c:z,ro".into(), "named:/n".into()],
            &[
                "type=bind,source=/d,target=/d".into(),
                "type=bind,source=/e,target=/e,readonly".into(),
                "source=/f,target=/f,ro=true".into(),
                "type=volume,source=v,target=/v".into(),
                "type=tmpfs,target=/t".into(),
            ],
        );
        let got: Vec<(&str, bool)> = binds.iter().map(|b| (b.source.as_str(), b.readonly)).collect();
        assert_eq!(got, [("/a", false), ("/b", true), ("/c", true), ("/d", false), ("/e", true), ("/f", true)]);
    }

    #[test]
    fn host_class_detection() {
        let base = fixture("host");
        let run = base.join("run/user/1000");
        std::fs::create_dir_all(run.join("podman")).unwrap();
        let runtime = worker_policy::runtime_sockets(Some("unix:///srv/docker.socket"), None, Some(&run), None);
        let session = session_sockets(Some(&run), Some("/tmp/wl-0"), Some("unix:path=/srv/bus,guid=x;unix:abstract=/a"));
        assert_eq!(session, [PathBuf::from("/tmp/.X11-unix"), run.clone(), "/tmp/wl-0".into(), "/srv/bus".into()]);
        let bind = |source: &str| Bind { source: source.into(), readonly: true };
        let class = |binds: &[Bind], privileged| host_class(privileged, binds, &runtime, &session, None);
        assert_eq!(class(&[], true), [HostClass::Privileged]);
        assert_eq!(class(&[bind("/repo"), bind(&s(&base.join("cfg")))], false), []);
        // `$XDG_RUNTIME_DIR` itself holds a runtime socket too.
        for source in ["/var/run/docker.sock", "/var/run", "/srv/docker.socket", "/elsewhere/podman.sock", &s(&run.join("podman")), &s(&run)] {
            assert_eq!(class(&[bind(source)], false), [HostClass::RuntimeSocket(source.into())], "{source}");
        }
        for source in ["/tmp/.X11-unix", "/tmp/.X11-unix/X0", "/tmp/wl-0", "/srv/bus", &s(&run.join("wayland-0"))] {
            assert_eq!(class(&[bind(source)], false), [HostClass::Session(source.into())], "{source}");
        }
        // A symlink to the X11 dir.
        let link = base.join("x11");
        std::os::unix::fs::symlink("/tmp/.X11-unix", &link).unwrap();
        assert_eq!(class(&[bind(&s(&link))], false), [HostClass::Session(s(&link))]);

        let classes = class(&[bind("/tmp/.X11-unix")], true);
        assert_eq!(host_class_warning("web", &classes, false), None);
        assert_eq!(host_class_warning("web", &[], true), None);
        assert_eq!(
            host_class_warning("web", &classes, true).unwrap(),
            "instance `web` can act as this host (it runs privileged; its bind `/tmp/.X11-unix` reaches the \
             host's display or session sockets), and this host brokers credentials: the broker's per-instance \
             limits don't hold against it"
        );
    }

    #[test]
    fn shared_writable_binds_are_named() {
        let base = fixture("shared");
        let (cfg, repo) = (base.join("cfg"), base.join("repo"));
        let mut other = instance("web-2", &cfg, &cfg.join(".worktrees/web-2"));
        other.worktree = Some(cfg.join(".worktrees/web-2"));
        other.base_folder = repo.clone();
        let fence = Fence::new(std::slice::from_ref(&cfg), &[("web-2", &other)], None);
        let rw = |source: &Path| Bind { source: s(source), readonly: false };
        let companion = repo.join(".git");
        let warnings = fence.shared_writable(
            "web",
            &[
                rw(&cfg.join(".worktrees/web")),
                rw(&companion),
                rw(&cfg.join("shared-volumes/cargo")),
                rw(&cfg.join("shared-volumes/gh")),
                Bind { source: s(&cfg.join("shared-volumes/pnpm-store")), readonly: true },
            ],
        );
        assert_eq!(warnings.len(), 2, "{warnings:#?}");
        assert!(warnings[0].starts_with(&format!("writable bind `{}` is shared with instance `web-2`'s checkout", s(&companion))), "{}", warnings[0]);
        assert!(warnings[1].contains("with the `cargo` cache") && warnings[1].contains("instance `web` has credential claims"), "{}", warnings[1]);
    }
}
