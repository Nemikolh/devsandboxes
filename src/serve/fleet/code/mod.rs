//! Moving a config root and its repos onto a worker (docs/architecture/daemon/fleet.md,
//! *Moving code*; fleet step 3c1). The plan's `Transfer` seam, in its git +
//! tar implementation:
//!
//! - **offer** ([`offer`], on the placing host): what a sandbox needs on
//!   the worker: its config root as a tar ([`tar`], content-hashed) and,
//!   for its `folder` and each `folders` entry outside the root, one branch
//!   of the folder's own checkout ([`repo`]; no remote involved, fleet plan
//!   decision 72). Anything that can't move is [`Unprepared::NotMovable`]:
//!   the run stays where it is.
//! - **haves** ([`Transfer::haves`]): what the worker has already under its
//!   fleet root for this host, one `code.haves` ([`haves_of`]): the roots
//!   unpacked there and the tips of its cache of each repo.
//! - **send** ([`send`]): the tar unless the worker has it, then each
//!   repo's branch and, for the checked-out one, a fresh WIP snapshot
//!   ([`wip`], decision 73) as a bundle from the checkout `--not` its tips
//!   (0 bytes when it has everything), one `TRANSFER` each, through the
//!   worker's link ([`LinkTransfer`]).
//! - **receive** ([`receivers`], on the worker): `root-tar` unpacked into
//!   `<fleet root>/roots/<hash>/`; `bundle` into the repo's cache
//!   `<fleet root>/repos/<repo id>.git`, then a checkout below the root cut
//!   from it; a `bundle` with no `to` into the cache only.
//! - **mirror** ([`mirror`], on a member placing elsewhere than its hub):
//!   the same refs again into the hub's cache, best effort (decision 75).
//! - **pull** ([`Transfer::pull`], [`fetch`]): fetch-back (decision 76), a
//!   placed instance's branch and WIP bundled on the worker for the host
//!   that placed it, pulled by that host into its own checkout's hidden
//!   refs.
//!
//! The layout keeps every `folder` where the config says, relative to the
//! root: the tar's members sit under the root's last components on the hub
//! (as many as the folders climb with `..`, [`layout`]), so `../web` from
//! `/w/cfg` is `<hash>/cfg` and `<hash>/web` on the worker.
//! [`prepare`] is offer + send.

pub mod fetch;
pub mod repo;
pub mod tar;
pub mod wip;

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use self::repo::Repo;
use super::transfer::{self, Header, Received, Receivers};
use crate::serve::api::ApiError;
use crate::config::{Build, Config, MountContext, Placement, ResolvedSandbox};
use crate::worker_policy::{self, Allow, Policy, Refusal};

/// A config root's tar: [`tar`], unpacked under `roots/<hash>/`.
pub const ROOT_TAR_KIND: &str = "root-tar";
/// One branch of a folder's repo: a git bundle, landed at its header's `to`.
pub const BUNDLE_KIND: &str = "bundle";
/// The largest config root tar either end takes.
pub const ROOT_TAR_LIMIT: u64 = 256 << 20;
/// The largest bundle a worker takes.
pub const BUNDLE_LIMIT: u64 = 4 << 30;

/// Top-level entries of a config root that never travel: per-host and
/// per-instance state (worktrees, shared volumes, `worktree-link` stores).
/// `.git` is skipped at every depth.
pub const ROOT_SKIPS: [&str; 3] = [".worktrees", "shared-volumes", "shared-files"];

/// Why a sandbox doesn't go to a worker.
#[derive(Debug)]
pub enum Unprepared {
    /// It can't move (a folder that isn't a checkout, a detached `HEAD`,
    /// …): it runs where it is; the reason is for the user.
    NotMovable(String),
    /// Moving it failed.
    Failed(anyhow::Error),
}

impl std::fmt::Display for Unprepared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unprepared::NotMovable(why) => write!(f, "runs where it is: {why}"),
            Unprepared::Failed(e) => write!(f, "{e:#}"),
        }
    }
}

impl From<anyhow::Error> for Unprepared {
    fn from(e: anyhow::Error) -> Self {
        Unprepared::Failed(e)
    }
}

fn not_movable<T>(why: impl Into<String>) -> Result<T, Unprepared> {
    Err(Unprepared::NotMovable(why.into()))
}

/// The API call a peer asks a worker's haves with (docs/reference/api.md,
/// `code.haves`), answered by [`haves_call`].
pub const HAVES_METHOD: &str = "code.haves";
/// The most repos one `code.haves` asks about.
pub const HAVES_REPOS_MAX: usize = 32;
/// The most roots `code.haves` answers (by name): one past it is only sent
/// again, and kept.
pub const ROOTS_CAP: usize = 1024;
/// How long [`LinkTransfer::haves`] waits for the worker's hello, then for
/// its answer: listing a dir and each cache's tips (at most
/// `repo::HAVES_CAP`) is quick.
pub const HAVES_HELLO_WAIT: Duration = Duration::from_secs(5);
pub const HAVES_WAIT: Duration = Duration::from_secs(30);
/// How long one write of a transfer's body may block: the link's flow
/// control holds it while the worker writes the body out, never this long
/// unless the link is stuck.
pub const SEND_WRITE_WAIT: Duration = Duration::from_secs(5 * 60);
/// How long a sender waits for the worker's answer once the body is sent:
/// the worker takes a bundle into its cache and cuts the checkout before it
/// answers, each git step bounded by `repo::WHOLE`; a landing that takes
/// longer than one such step and some is treated as stuck (the next
/// `prepare` sends again).
pub const ANSWER_WAIT: Duration = Duration::from_secs(35 * 60);

/// What the receiver has already, under its fleet root for the sender: the
/// `code.haves` answer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Haves {
    /// Root hashes unpacked under `roots/` (at most [`ROOTS_CAP`]).
    #[serde(default)]
    pub roots: BTreeSet<String>,
    /// Per repo identity asked about, its cache's tips (`repo::tips`); a
    /// repo with no cache (or an empty one) is absent.
    #[serde(default)]
    pub tips: BTreeMap<String, Vec<String>>,
}

/// The seam bodies cross: production sends through a worker's link
/// ([`LinkTransfer`]); another implementation (a sync layer) replaces it
/// without changing [`prepare`]'s callers.
pub trait Transfer {
    /// What the receiver has already, with the tips of its cache of each of
    /// `repos` (repo identities). The default: nothing, so everything is
    /// sent.
    fn haves(&self, repos: &[String]) -> Result<Haves> {
        let _ = repos;
        Ok(Haves::default())
    }
    /// One transfer: `header`, then `body`; the receiver's refusal is the
    /// error.
    fn send(&self, header: &Header, body: &mut dyn Read) -> Result<()>;
    /// One pull (`transfer::pull`): the file `name` the other end made for
    /// this host, of `kind`, at most `limit` bytes, into `dest`; its size.
    /// The default pulls nothing.
    fn pull(&self, kind: &str, name: &str, limit: u64, dest: &Path) -> Result<u64> {
        let _ = (kind, limit, dest);
        bail!("this transfer pulls nothing (asked for {name})")
    }
}

/// [`Transfer`] over a worker's link: each send one `TRANSFER` stream
/// through its link's socket, else its route socket
/// (`fleet::route::connect`).
pub struct LinkTransfer {
    /// The socket dir.
    pub dir: PathBuf,
    pub worker: String,
}

impl LinkTransfer {
    /// `worker`, its sockets in the socket dir `dir`.
    pub fn for_worker(dir: &Path, worker: &str) -> LinkTransfer {
        LinkTransfer { dir: dir.to_path_buf(), worker: worker.to_string() }
    }
}

impl Transfer for LinkTransfer {
    /// `code.haves` over a fresh `API` stream of the same path (hello, then
    /// the call), each step bounded ([`HAVES_HELLO_WAIT`], [`HAVES_WAIT`]).
    fn haves(&self, repos: &[String]) -> Result<Haves> {
        use crate::serve::machines::{link, ssh::Duplex};
        let stream = super::route::connect(&self.dir, &self.worker, crate::devsbd::proto::channel::API)
            .with_context(|| format!("cannot reach the worker `{}`", self.worker))?;
        stream.set_write_timeout(Some(HAVES_WAIT)).context("cannot set a write timeout")?;
        let mut link = link::handshake(&self.worker, Duplex::socket(stream), HAVES_HELLO_WAIT, link::Over::Link)
            .map_err(|e| anyhow!("{e}"))?;
        let r = link.conn.call_within(HAVES_METHOD, json!({ "repos": repos }), HAVES_WAIT)?;
        let r = r.with_context(|| format!("no `{HAVES_METHOD}` answer within {}s", HAVES_WAIT.as_secs()))?;
        if let Some(e) = r.error {
            bail!("{HAVES_METHOD}: {}", e.message);
        }
        serde_json::from_value(r.result.unwrap_or(Value::Null)).with_context(|| format!("bad {HAVES_METHOD} answer"))
    }

    fn send(&self, header: &Header, body: &mut dyn Read) -> Result<()> {
        let stream = super::route::connect(&self.dir, &self.worker, crate::devsbd::proto::channel::TRANSFER)
            .with_context(|| format!("cannot reach the worker `{}`", self.worker))?;
        stream.set_write_timeout(Some(SEND_WRITE_WAIT)).context("cannot set a write timeout")?;
        stream.set_read_timeout(Some(ANSWER_WAIT)).context("cannot set a read timeout")?;
        transfer::send(stream, header, body)
    }

    /// A fresh `TRANSFER` stream of the same path, each read and write on
    /// it bounded by [`SEND_WRITE_WAIT`]: the worker serves a file it made
    /// already, so a stall that long is a stuck link.
    fn pull(&self, kind: &str, name: &str, limit: u64, dest: &Path) -> Result<u64> {
        let stream = super::route::connect(&self.dir, &self.worker, crate::devsbd::proto::channel::TRANSFER)
            .with_context(|| format!("cannot reach the worker `{}`", self.worker))?;
        stream.set_write_timeout(Some(SEND_WRITE_WAIT)).context("cannot set a write timeout")?;
        stream.set_read_timeout(Some(SEND_WRITE_WAIT)).context("cannot set a read timeout")?;
        transfer::pull(stream, kind, name, limit, dest)
    }
}

impl LinkTransfer {
    /// `instances.fetch {instance, haves}` on the worker ([`fetch::fetch_call`])
    /// over a fresh `API` stream of the same path: the hello within
    /// [`HAVES_HELLO_WAIT`], the answer within [`fetch::FETCH_WAIT`] (a
    /// snapshot and a bundle there).
    pub fn fetch_instance(&self, instance: &str, haves: &[String]) -> Result<fetch::Fetched> {
        use crate::serve::machines::{link, ssh::Duplex};
        let method = fetch::FETCH_METHOD;
        let stream = super::route::connect(&self.dir, &self.worker, crate::devsbd::proto::channel::API)
            .with_context(|| format!("cannot reach the worker `{}`", self.worker))?;
        stream.set_write_timeout(Some(HAVES_WAIT)).context("cannot set a write timeout")?;
        let mut link = link::handshake(&self.worker, Duplex::socket(stream), HAVES_HELLO_WAIT, link::Over::Link)
            .map_err(|e| anyhow!("{e}"))?;
        let r = link.conn.call_within(method, json!({ "instance": instance, "haves": haves }), fetch::FETCH_WAIT)?;
        let r = r.with_context(|| format!("no `{method}` answer within {}s", fetch::FETCH_WAIT.as_secs()))?;
        if let Some(e) = r.error {
            bail!("{method} on {}: {}", self.worker, e.message);
        }
        serde_json::from_value(r.result.unwrap_or(Value::Null)).with_context(|| format!("bad {method} answer"))
    }
}

/// Where things land below `roots/<hash>/` on the worker ([`layout`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// The config root.
    pub root: PathBuf,
    /// Each folder, in the order given.
    pub folders: Vec<PathBuf>,
}

/// The layout of config root `root` and `folders` outside it (absolute,
/// canonical) below the transferred tree, pure. Each folder's path relative
/// to `root` climbs `n` times (`..`) then goes down; the root sits under
/// its last `max(n)` components (at least one, its own name), so every
/// folder's relative path resolves inside the tree, with the hub's names.
/// Refused (as why): a folder at or in the root, one holding the root, two
/// folders where one holds the other, a root too shallow for its folders.
pub fn layout(root: &Path, folders: &[PathBuf]) -> Result<Layout, String> {
    let names = |p: &Path| -> Vec<String> {
        p.components().filter_map(|c| match c {
            Component::Normal(n) => Some(n.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect()
    };
    let root_names = names(root);
    let mut climbs = Vec::new();
    for f in folders {
        let f_names = names(f);
        let common = root_names.iter().zip(&f_names).take_while(|(a, b)| a == b).count();
        let ups = root_names.len() - common;
        let down = f_names[common..].to_vec();
        if ups == 0 {
            return Err(format!("folder {} is in the config root", f.display()));
        }
        if down.is_empty() {
            return Err(format!("folder {} holds the config root", f.display()));
        }
        climbs.push((ups, down));
    }
    let depth = climbs.iter().map(|(ups, _)| *ups).max().unwrap_or(0).max(1);
    if root_names.len() < depth {
        return Err(format!("config root {} is too shallow for its folders", root.display()));
    }
    let root_at: Vec<String> = root_names[root_names.len() - depth..].to_vec();
    let mut out = Vec::new();
    for (ups, down) in climbs {
        let at: PathBuf = root_at[..depth - ups].iter().chain(&down).collect();
        out.push(at);
    }
    for (i, a) in out.iter().enumerate() {
        for b in &out[i + 1..] {
            if a.starts_with(b) || b.starts_with(a) {
                return Err(format!("folders {} and {} nest", folders[i].display(), b.display()));
            }
        }
    }
    Ok(Layout { root: root_at.iter().collect(), folders: out })
}

/// One repo an [`Offer`] carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoOffer {
    /// The folder on this host: the checkout the bundle is made from.
    pub host: PathBuf,
    /// Where it lands below `roots/<hash>/`.
    pub at: PathBuf,
    /// The branch the bundle carries.
    pub branch: String,
    /// `branch` is the one checked out: [`send`] snapshots the working
    /// tree over it. A requested other branch goes as it is committed.
    pub wip: bool,
    /// The repo's identity (`repo::identity` of the branch, or of the
    /// devsandbox root of a repo with no commit yet): which of the
    /// worker's caches it goes into.
    pub id: String,
}

/// What [`send`] sends for one sandbox.
#[derive(Debug, Clone)]
pub struct Offer {
    /// The config root's tar and its hash (the sha256's first 16 hex).
    pub tar: Vec<u8>,
    pub hash: String,
    pub layout: Layout,
    pub repos: Vec<RepoOffer>,
}

/// Where a sandbox is on the worker once [`prepare`]d.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    /// The config root there (`<fleet root>/roots/<hash>/<root>`): the
    /// `root` a placed `run` names.
    pub remote_root: PathBuf,
    /// Each moved folder: its host path, its path there, the branch its
    /// bundle carried (checked out in a new checkout or one no instance
    /// mounts; one in use keeps what it has checked out and gets that
    /// branch updated, `repo::land_from`).
    pub folders: Vec<(PathBuf, PathBuf, String)>,
    /// What each repo's bundle carried, for the hub mirror ([`mirror`]).
    pub sent: Vec<Sent>,
    /// Lines for the run's output after the placement's (the hub mirror's
    /// failures); set by the caller, [`send`] leaves it empty.
    pub notes: Vec<String>,
}

/// One repo [`send`] sent: the hub mirror sends the same refs again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sent {
    /// The repo's identity.
    pub id: String,
    /// The checkout it was bundled from.
    pub host: PathBuf,
    /// The header's refs: `refs/heads/<branch>` and maybe its WIP ref, at
    /// the commits sent (the snapshot no longer named by any ref here).
    pub refs: BTreeMap<String, String>,
}

/// What sandbox `sandbox` of `config_root` needs on a worker, each repo
/// from its own checkout here (decision 72). `branch` (a `run --branch`) is
/// the `folder`'s branch when its checkout has it; else, and for `folders`
/// entries, the checkout's current branch (it may have no commit yet: its
/// identity is then that of the devsandbox root, `repo::ensure_root`, made
/// here once); a detached `HEAD` doesn't move. `folders` entries inside the
/// config root travel in its tar (their `.git` doesn't); one with a `${…}`
/// variable doesn't move; nor does a sandbox with [`ties`]. Only reads the
/// checkouts (and maybe makes that root): the snapshot is [`send`]'s.
pub fn offer(config_root: &Path, sandbox: &str, branch: Option<&str>) -> Result<Offer, Unprepared> {
    let root = config_root.canonicalize().with_context(|| format!("config root {} does not exist", config_root.display()))?;
    let config = Config::load(&root)?;
    let resolved = config.resolve_sandbox(sandbox)?;
    let props = &resolved.properties;
    let folder = props.folder.as_deref().with_context(|| format!("sandbox `{sandbox}` has no `folder`"))?;
    let resolve = |f: &str| root.join(f).canonicalize().with_context(|| format!("sandbox folder `{f}` does not exist"));
    let main = resolve(folder)?;
    if main.starts_with(&root) {
        return not_movable(format!("its folder {} is in the config root (the config root travels as a tar, without `.git`)", main.display()));
    }
    let mut outside = vec![main];
    for entry in props.folders.iter().flat_map(|f| f.values()) {
        if entry.path().contains("${") {
            return not_movable(format!("`folders` entry `{}` uses a variable", entry.path()));
        }
        let host = resolve(entry.path())?;
        if !host.starts_with(&root) && !outside.contains(&host) {
            outside.push(host);
        }
    }
    let tied = ties(&root, &config, &resolved);
    if !tied.is_empty() {
        return not_movable(tied.join(", "));
    }
    let layout = layout(&root, &outside).map_err(Unprepared::NotMovable)?;
    let mut repos_out = Vec::new();
    for (i, (host, at)) in outside.iter().zip(&layout.folders).enumerate() {
        let Some(checkout) = repo::probe(host)? else {
            return not_movable(format!("{} isn't a git checkout", host.display()));
        };
        let ask = if i == 0 { branch } else { None };
        if let Some(b) = ask {
            repo::check_branch(b)?;
        }
        let requested = match ask {
            Some(b) if checkout.branch.as_deref() != Some(b) && repo::has_branch(Repo::Checkout(host), b)? => Some(b.to_string()),
            _ => None,
        };
        let offered = if let Some(b) = requested {
            let id = repo::identity(Repo::Checkout(host), &format!("refs/heads/{b}"))?;
            RepoOffer { host: host.clone(), at: at.clone(), branch: b, wip: false, id }
        } else {
            let Some(current) = checkout.branch else {
                return not_movable(format!("{} has a detached HEAD (check out a branch first)", host.display()));
            };
            repo::check_branch(&current)?;
            let id = match repo::ensure_root(host)? {
                Some(_) => repo::identity(Repo::Checkout(host), repo::ROOT_REF)?,
                None => repo::identity(Repo::Checkout(host), "HEAD")?,
            };
            RepoOffer { host: host.clone(), at: at.clone(), branch: current, wip: true, id }
        };
        repos_out.push(offered);
    }
    let tar = pack_root(&root, &layout.root)?;
    let hash = short_hash(&tar);
    Ok(Offer { tar, hash, layout, repos: repos_out })
}

/// [`ties`] of sandbox `sandbox` of the config at `config_root`; a config
/// that doesn't load or resolve is the error (the run reports it itself).
pub fn ties_of(config_root: &Path, sandbox: &str) -> Result<Vec<String>> {
    let root = config_root.canonicalize().with_context(|| format!("config root {} does not exist", config_root.display()))?;
    let config = Config::load(&root)?;
    let resolved = config.resolve_sandbox(sandbox)?;
    Ok(ties(&root, &config, &resolved))
}

/// What ties `sandbox` of `config` (at the canonical config root `root`) to
/// this host, for the user (fleet plan decision 40): first the pin
/// (`placement = "home"`), then the inferred ties, what a
/// worker policy with no allowances would refuse, over the config root and
/// the folders that move with it (`folder`, `folders` outside the root) as
/// the only paths that travel. From the config ([`Policy::config_refusals`]:
/// `initializeCommand`, `privileged`, global services,
/// `${localEnv:…}`, …) and each `mounts` bind ([`Policy::check_bind`]:
/// sources outside those paths, `${sharedVolumes}` ones, which never travel
/// ([`ROOT_SKIPS`]), runtime sockets) and each `build` context or
/// dockerfile, the sandbox's or a service's. Any tie keeps the sandbox home,
/// whatever a worker's trust or allowances. Folders that can't move are
/// [`offer`]'s to say (they need git).
pub fn ties(root: &Path, config: &Config, sandbox: &ResolvedSandbox) -> Vec<String> {
    let props = &sandbox.properties;
    let at = |f: &str| {
        let p = root.join(f);
        worker_policy::resolve(&p).unwrap_or(p)
    };
    let main = props.folder.as_deref().map(at);
    let mut moved: Vec<PathBuf> = main.iter().cloned().collect();
    moved.extend(props.folders.iter().flat_map(|f| f.values()).filter(|e| !e.path().contains("${")).map(|e| at(e.path())));
    moved.retain(|f| !f.starts_with(root));
    // The moved folders as `binds`: paths under them travel as the root does.
    let policy = Policy {
        root: root.to_path_buf(),
        data: root.to_path_buf(),
        allow: Allow { binds: moved, ..Allow::default() },
        sockets: worker_policy::host_sockets(),
    };
    let mut out: Vec<String> = Vec::new();
    if props.placement == Some(Placement::Home) {
        out.push("`placement = \"home\"`".into());
    }
    out.extend(policy.config_refusals(root, config, sandbox).into_iter().map(tie));
    let root_s = root.to_string_lossy();
    let shared = root.join("shared-volumes");
    let shared_s = shared.to_string_lossy();
    let folder_s = main.as_deref().map(|m| m.to_string_lossy().into_owned()).unwrap_or_default();
    let basename = main.as_deref().and_then(Path::file_name).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let ctx = MountContext {
        config_dir: &root_s,
        workspace_folder: &folder_s,
        workspace_folder_basename: &basename,
        shared_volumes: &shared_s,
        instance: "instance",
        container_env: None,
    };
    for mount in props.mounts.iter().flatten() {
        // A bad mount is the run's to report.
        let Ok(m) = mount.resolve(&ctx) else { continue };
        let Some(source) = m.source.as_deref().filter(|_| m.kind == "bind") else { continue };
        // `${localEnv:…}` is counted above; what's still a variable (or
        // relative) isn't a host path to judge.
        if source.contains("${") || !Path::new(source).is_absolute() || reads_local_env(mount) {
            continue;
        }
        if let Err(r) = policy.check_bind(source) {
            out.push(tie(r));
        }
    }
    // Build paths, the sandbox's and its services' (both relative to the
    // config dir, as `services::build_args` takes them); a missing one is
    // the build's to report.
    let services: Vec<Build> = props.services.iter().flatten().filter_map(|n| config.resolve_service(n).ok()?.spec.build).collect();
    for build in props.build.iter().chain(&services) {
        let paths = [("build context", build.context.as_deref().or(Some("."))), ("dockerfile", build.dockerfile.as_deref())];
        for (what, rel) in paths {
            let Some(path) = rel.map(|r| root.join(r)).filter(|p| p.exists()) else { continue };
            if let Err(r) = policy.check_path(&path, what) {
                out.push(tie(r));
            }
        }
    }
    out.dedup();
    out
}

/// A mount whose raw source reads `${localEnv:…}`: [`ties`] counts it once,
/// as that variable.
fn reads_local_env(mount: &crate::config::Mount) -> bool {
    let raw = match mount {
        crate::config::Mount::Shorthand(s) => s.as_str(),
        crate::config::Mount::Object(o) => o.source.as_deref().unwrap_or_default(),
    };
    raw.contains("${localEnv:")
}

/// A refusal as a tie: what it names, with the policy's root called what it
/// is here.
fn tie(r: Refusal) -> String {
    r.what.replace("(outside the fleet root)", "(outside the config root)")
}

/// The first 16 hex of `bytes`' sha256.
pub fn short_hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// The config root `root` as a [`tar`] with its members under `at`: every
/// dir, regular file (exec bit kept) and symlink (stored, never followed)
/// in sorted order, but [`ROOT_SKIPS`] at the top and `.git` anywhere;
/// other kinds (sockets, fifos) are left out. A symlink pointing out of the
/// root ([`tar::check_link`] over its path in the root) or a non-UTF-8
/// name refuses the root; so does a tar over [`ROOT_TAR_LIMIT`].
pub fn pack_root(root: &Path, at: &Path) -> Result<Vec<u8>> {
    let at = at.to_str().context("the root's name isn't UTF-8")?.to_string();
    let mut w = tar::Writer::default();
    let mut prefix = String::new();
    for part in at.split('/') {
        prefix = if prefix.is_empty() { part.to_string() } else { format!("{prefix}/{part}") };
        w.dir(&prefix)?;
    }
    walk(root, "", &at, &mut w)?;
    Ok(w.finish())
}

fn walk(dir: &Path, rel: &str, at: &str, w: &mut tar::Writer) -> Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("cannot list {}", dir.display()))?
        .collect::<std::io::Result<_>>()
        .with_context(|| format!("cannot list {}", dir.display()))?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name();
        let name = name.to_str().with_context(|| format!("{} has a non-UTF-8 name", e.path().display()))?;
        if name == ".git" || (rel.is_empty() && ROOT_SKIPS.contains(&name)) {
            continue;
        }
        let rel = if rel.is_empty() { name.to_string() } else { format!("{rel}/{name}") };
        let member = format!("{at}/{rel}");
        let path = e.path();
        let meta = path.symlink_metadata().with_context(|| format!("cannot read {}", path.display()))?;
        let kind = meta.file_type();
        if kind.is_symlink() {
            let target = std::fs::read_link(&path).with_context(|| format!("cannot read {}", path.display()))?;
            let target = target.to_str().with_context(|| format!("{} points at a non-UTF-8 path", path.display()))?;
            tar::check_link(&rel, target)?;
            w.link(&member, target)?;
        } else if kind.is_dir() {
            w.dir(&member)?;
            walk(&path, &rel, at, w)?;
        } else if kind.is_file() {
            if w.len() as u64 + meta.len() > ROOT_TAR_LIMIT {
                bail!("the config root is over {ROOT_TAR_LIMIT} bytes");
            }
            let data = std::fs::read(&path).with_context(|| format!("cannot read {}", path.display()))?;
            w.file(&member, meta.permissions().mode() & 0o100 != 0, &data)?;
        }
    }
    Ok(())
}

/// Send `offer` through `t`: first its haves (one ask, every repo's
/// identity), then the root's tar unless the worker has it, then each
/// repo as a bundle `--not` the worker's tips of that repo ([`send_repo`]);
/// where it all is on the worker whose fleet root is `worker_root`.
pub fn send(offer: &Offer, worker_root: &Path, t: &dyn Transfer) -> Result<Prepared> {
    let ids: Vec<String> = offer.repos.iter().map(|r| r.id.clone()).collect::<BTreeSet<_>>().into_iter().collect();
    let haves = t.haves(&ids).context("cannot ask the worker what it has")?;
    if !haves.roots.contains(&offer.hash) {
        let (size, sha256) = transfer::digest(&offer.tar[..])?;
        let name = format!("root-{}.tar", offer.hash);
        let header = Header { kind: ROOT_TAR_KIND.into(), name, size, sha256, to: None, repo: None, refs: None };
        t.send(&header, &mut &offer.tar[..]).context("cannot send the config root")?;
    }
    let tree = worker_root.join("roots").join(&offer.hash);
    let mut folders = Vec::new();
    let mut sent = Vec::new();
    for r in &offer.repos {
        let tips = haves.tips.get(&r.id).map(Vec::as_slice).unwrap_or_default();
        let refs = send_repo(r, &offer.hash, tips, t).with_context(|| format!("cannot send {}", r.host.display()))?;
        folders.push((r.host.clone(), tree.join(&r.at), r.branch.clone()));
        sent.push(Sent { id: r.id.clone(), host: r.host.clone(), refs });
    }
    Ok(Prepared { remote_root: tree.join(&offer.layout.root), folders, sent, notes: Vec::new() })
}

/// The hub mirror (fleet plan decision 75): each of `sent` again, through
/// `t` (the hub's link), into the hub's cache of that repo only: one
/// `code.haves`, then per repo a bundle from its checkout `--not` the hub's
/// tips of the same refs at the same commits (each pointed at by a
/// transient `repo::TempRef`, so a snapshot no ref names any more still
/// goes), a header with no `to` (no checkout there, no root needed); a
/// body of 0 bytes when the hub has it all. Best effort: each repo that
/// didn't make it, with why (all of them when the hub can't be asked).
pub fn mirror(sent: &[Sent], t: &dyn Transfer) -> Vec<(PathBuf, String)> {
    let ids: Vec<String> = sent.iter().map(|s| s.id.clone()).collect::<BTreeSet<_>>().into_iter().collect();
    let haves = match t.haves(&ids) {
        Ok(h) => h,
        Err(e) => {
            let why = format!("{:#}", e.context("cannot ask the hub what it has"));
            return sent.iter().map(|s| (s.host.clone(), why.clone())).collect();
        }
    };
    let mut failed = Vec::new();
    for s in sent {
        let tips = haves.tips.get(&s.id).map(Vec::as_slice).unwrap_or_default();
        if let Err(e) = mirror_repo(s, tips, t) {
            failed.push((s.host.clone(), format!("{e:#}")));
        }
    }
    failed
}

/// One repo of [`mirror`]: its refs' commits on transient refs in its
/// checkout, bundled `--not` `tips` into a temp file in its git dir (as
/// [`send_repo`]), both gone once sent.
fn mirror_repo(s: &Sent, tips: &[String], t: &dyn Transfer) -> Result<()> {
    let checkout = Repo::Checkout(&s.host);
    let file = repo::TempPath::new(&checkout.git_dir()?, "bundle");
    let temps = s.refs.values().map(|id| repo::TempRef::at(&s.host, id)).collect::<Result<Vec<_>>>()?;
    let names: Vec<&str> = temps.iter().map(|r| r.name.as_str()).collect();
    let new = repo::bundle_refs(checkout, &names, tips, &file.0)?;
    drop(temps);
    let open = || std::fs::File::open(&file.0).with_context(|| format!("cannot read {}", file.0.display()));
    let mut body: Box<dyn Read> = if new { Box::new(open()?) } else { Box::new(std::io::empty()) };
    let (size, sha256) = if new { transfer::digest(open()?)? } else { transfer::digest(std::io::empty())? };
    let header = Header {
        kind: BUNDLE_KIND.into(),
        name: "repo.bundle".into(),
        size,
        sha256,
        to: None,
        repo: Some(s.id.clone()),
        refs: Some(s.refs.clone()),
    };
    t.send(&header, &mut body)
}

/// One repo of an offer of root `hash`, bundled from its checkout `--not`
/// `tips` (the worker's of that repo): its branch and, when it's the one
/// checked out ([`RepoOffer::wip`]), a snapshot of the working tree
/// ([`wip::snapshot`]) made just now on a transient
/// `refs/devsandbox/wip/send-<nonce>` (`repo::TempRef`), deleted once the
/// bundle is written (and on any error). A branch with no commit yet is
/// bundled as its devsandbox root ([`repo::ROOT_REF`]), so the worker's
/// `refs/heads/<branch>` is that empty commit with the snapshot over it.
/// The bundle goes to a temp file in the checkout's git dir (never its
/// working tree, where the user or a snapshot's `add -A` would see it; on
/// the objects' disk), removed after the send; a body of 0 bytes when the
/// worker has every commit. The header names the repo,
/// `refs/heads/<branch>` at the base commit and, with a snapshot,
/// `refs/devsandbox/wip/<branch>` at it; those refs are the answer.
fn send_repo(r: &RepoOffer, hash: &str, tips: &[String], t: &dyn Transfer) -> Result<BTreeMap<String, String>> {
    let full = format!("refs/heads/{}", r.branch);
    let checkout = Repo::Checkout(&r.host);
    let file = repo::TempPath::new(&checkout.git_dir()?, "bundle");
    let mut refs = BTreeMap::new();
    let new = if r.wip {
        let temp = repo::TempRef::new(&r.host);
        let snap = wip::snapshot(&r.host, &temp.name)?;
        if snap.branch.as_deref() != Some(r.branch.as_str()) {
            bail!("{} isn't on {} any more", r.host.display(), r.branch);
        }
        let base_ref = if repo::has_branch(checkout, &r.branch)? { full.as_str() } else { repo::ROOT_REF };
        refs.insert(full.clone(), snap.base);
        refs.insert(repo::wip_ref(&r.branch), snap.wip);
        let new = repo::bundle_refs(checkout, &[base_ref, &temp.name], tips, &file.0)?;
        drop(temp); // the ref is gone before anything is sent
        new
    } else {
        let commit = repo::commit_at(checkout, &full)?.with_context(|| format!("no {full}"))?;
        refs.insert(full.clone(), commit);
        repo::bundle_refs(checkout, &[&full], tips, &file.0)?
    };
    let open = || std::fs::File::open(&file.0).with_context(|| format!("cannot read {}", file.0.display()));
    let mut body: Box<dyn Read> = if new { Box::new(open()?) } else { Box::new(std::io::empty()) };
    let (size, sha256) = if new { transfer::digest(open()?)? } else { transfer::digest(std::io::empty())? };
    let to = r.at.to_str().context("a folder's name isn't UTF-8")?;
    let header = Header {
        kind: BUNDLE_KIND.into(),
        name: "repo.bundle".into(),
        size,
        sha256,
        to: Some(format!("{hash}/{to}")),
        repo: Some(r.id.clone()),
        refs: Some(refs.clone()),
    };
    t.send(&header, &mut body)?;
    Ok(refs)
}

/// `code.haves {repos}` from a peer whose fleet root here is `fleet_root`
/// (the daemon's `Table::Peer`): [`haves_params`], then [`haves_of`].
pub fn haves_call(fleet_root: &Path, params: &Value) -> Result<Value, ApiError> {
    let repos = haves_params(params)?;
    let haves = haves_of(fleet_root, &repos).map_err(ApiError::internal)?;
    serde_json::to_value(haves).map_err(|e| ApiError::internal(e.into()))
}

/// `code.haves`' `repos`: at most [`HAVES_REPOS_MAX`] repo identities
/// (`repo::check_id`).
pub fn haves_params(params: &Value) -> Result<Vec<String>, ApiError> {
    let Some(list) = params.get("repos").and_then(Value::as_array) else {
        return Err(ApiError::invalid(format!("`{HAVES_METHOD}` needs `repos`, a list of repo identities")));
    };
    if list.len() > HAVES_REPOS_MAX {
        return Err(ApiError::invalid(format!("`{HAVES_METHOD}` asks about at most {HAVES_REPOS_MAX} repos")));
    }
    list.iter()
        .map(|v| {
            let id = v.as_str().ok_or_else(|| ApiError::invalid("`repos` holds repo identities (strings)"))?;
            repo::check_id(id).map_err(|e| ApiError::invalid(format!("{e:#}")))?;
            Ok(id.to_string())
        })
        .collect()
}

/// What fleet root `fleet_root` has: the root hashes unpacked under
/// `roots/` (real dirs named as one, sorted, at most [`ROOTS_CAP`]) and,
/// for each of `repos` with a cache under `repos/`, its tips.
pub fn haves_of(fleet_root: &Path, repos: &[String]) -> Result<Haves> {
    let mut out = Haves::default();
    let roots = fleet_root.join("roots");
    match std::fs::read_dir(&roots) {
        Ok(entries) => {
            let mut names: Vec<String> = entries
                .flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| is_hash(n))
                .collect();
            names.sort_unstable();
            names.truncate(ROOTS_CAP);
            out.roots = names.into_iter().collect();
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(anyhow!(e).context(format!("cannot list {}", roots.display()))),
    }
    for id in repos {
        repo::check_id(id)?;
        let cache = cache_path(fleet_root, id);
        if cache.symlink_metadata().is_ok_and(|m| m.is_dir()) && cache.join("HEAD").is_file() {
            let tips = repo::tips(Repo::Bare(&cache))?;
            if !tips.is_empty() {
                out.tips.insert(id.clone(), tips);
            }
        }
    }
    Ok(out)
}

/// Fleet root `fleet_root`'s cache of repo `id`: `repos/<id>.git`.
pub fn cache_path(fleet_root: &Path, id: &str) -> PathBuf {
    fleet_root.join("repos").join(format!("{id}.git"))
}

/// [`cache_path`], made a bare repo if it isn't one: `repos/` and the cache
/// real dirs, never through a symlink.
fn make_cache(fleet_root: &Path, id: &str) -> Result<PathBuf> {
    repo::check_id(id)?;
    let cache = cache_path(fleet_root, id);
    real_dir(cache.parent().context("a cache has a parent")?)?;
    real_dir(&cache)?;
    repo::ensure_bare(&cache)?;
    Ok(cache)
}

/// `dir` made if missing (its parent must exist); refused unless it's a
/// real dir (a symlink is refused, whatever it points at).
fn real_dir(dir: &Path) -> Result<()> {
    match std::fs::create_dir(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(anyhow!(e).context(format!("cannot create {}", dir.display()))),
    }
    if !dir.symlink_metadata().is_ok_and(|m| m.is_dir()) {
        bail!("{} isn't a directory", dir.display());
    }
    Ok(())
}

/// 16 lowercase hex: a root hash's (and a repo identity's) shape.
fn is_hash(s: &str) -> bool {
    s.len() == 16 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Put sandbox `sandbox` of `config_root` on the worker whose fleet root is
/// `worker_root`, through `t` ([`offer`], then [`send`]). The `run` verb's
/// step before it runs there.
pub fn prepare(t: &dyn Transfer, worker_root: &Path, config_root: &Path, sandbox: &str, branch: Option<&str>) -> Result<Prepared, Unprepared> {
    let offer = offer(config_root, sandbox, branch)?;
    Ok(send(&offer, worker_root, t)?)
}

fn next_seq() -> u32 {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

/// The worker's receivers for code, into fleet root `fleet_root`, added to
/// `receivers`:
///
/// - `root-tar` named `root-<hash>.tar`, `<hash>` its sha256's first 16
///   hex: unpacked ([`tar::extract`]) into a temp dir renamed to
///   `roots/<hash>/`; a hash already there is kept.
/// - `bundle` with `to` = `<hash>/<path>`, `<hash>` a root unpacked here,
///   `repo` its repo identity and `refs` one `refs/heads/<branch>` and at
///   most its WIP ref (`refs/devsandbox/wip/<branch>`, [`bundle_refs_of`]):
///   its objects into the cache `repos/<repo>.git` (made, bare, if missing;
///   a 0-byte body brings none), the refs set there (each a commit the
///   cache now has, `repo::set_refs`), then the checkout at
///   `roots/<hash>/<path>` cut from the cache ([`repo::land_from`]: a new
///   one, or one `in_use` says no instance mounts, is cut to what was sent,
///   the WIP applied; one in use keeps its working tree, a line logged
///   when its branch stays), every dir on the way a real one (no symlink).
/// - `bundle` with no `to` (the hub mirror, [`mirror`]): cache-only, the
///   same checks of `repo` and `refs`, into `repos/<repo>.git` with its
///   refs set, no checkout and no root needed.
///
/// Bodies wait in `roots/.in/` (`repos/.in/` for a cache-only bundle); the
/// unpacking's error is the transfer's answer, so the sender hears it.
/// It also serves the peer's fetch-back pulls ([`fetch::serves`]: the
/// bundles `instances.fetch` made under `out/`).
pub fn receivers(fleet_root: &Path, receivers: Receivers, in_use: InUse) -> Receivers {
    let receivers = fetch::serves(fleet_root, receivers);
    let log = receivers.log();
    let roots = fleet_root.join("roots");
    let (tar_in, tar_roots, bundle_in, bundle_roots) = (roots.clone(), roots.clone(), roots.clone(), roots);
    let cache_in = fleet_root.join("repos").join(".in");
    let fleet_root = fleet_root.to_path_buf();
    receivers
        .with(
            ROOT_TAR_KIND,
            ROOT_TAR_LIMIT,
            move |h| {
                let want = format!("root-{}.tar", &h.sha256[..16]);
                if h.name != want || h.to.is_some() {
                    bail!("a root tar is named {want}, not {}", h.name);
                }
                Ok(tar_in.join(".in").join(&h.name))
            },
            |_| {},
        )
        .then(ROOT_TAR_KIND, move |r| {
            let unpacked = unpack_root(&r.path, &tar_roots, &r.header.sha256[..16]);
            let _ = std::fs::remove_file(&r.path);
            unpacked
        })
        .with(
            BUNDLE_KIND,
            BUNDLE_LIMIT,
            move |h| {
                // Unique: two empty bodies share their sha256.
                let name = format!("{}-{}.bundle", h.sha256, repo::nonce());
                if h.to.is_none() {
                    let (id, _, _) = bundle_refs_of(h)?;
                    repo::check_id(id)?;
                    return Ok(cache_in.join(name));
                }
                let (hash, _) = bundle_target(h)?;
                bundle_refs_of(h)?;
                if !bundle_in.join(hash).is_dir() {
                    bail!("root {hash} isn't here (its tar goes first)");
                }
                Ok(bundle_in.join(".in").join(name))
            },
            |_| {},
        )
        .then(BUNDLE_KIND, move |r: &Received| {
            let landed = (|| {
                let (id, branch, wip) = bundle_refs_of(&r.header)?;
                let refs = r.header.refs.as_ref().context("a bundle names its refs")?;
                if r.header.to.is_none() {
                    let cache = make_cache(&fleet_root, id)?;
                    if r.header.size > 0 {
                        repo::unbundle(&cache, &r.path)?;
                    }
                    return repo::set_refs(&cache, refs);
                }
                let (hash, rest) = bundle_target(&r.header)?;
                let tree = bundle_roots.join(hash);
                let mut at = tree.clone();
                for part in rest.split('/') {
                    at.push(part);
                    if at.symlink_metadata().is_ok_and(|m| !m.is_dir()) {
                        bail!("{} isn't a directory", at.display());
                    }
                }
                let cache = make_cache(&fleet_root, id)?;
                if r.header.size > 0 {
                    repo::unbundle(&cache, &r.path)?;
                }
                repo::set_refs(&cache, refs)?;
                let busy = at.join(".git").exists() && in_use(&at);
                if repo::land_from(&cache, &at, branch, wip, busy)? == repo::Landed::Kept {
                    log(&format!(
                        "fleet: {} is in use (dirty, or not behind the sent tip): {branch} stays where it is there, the sent tip is in the repo cache",
                        at.display()
                    ));
                }
                Ok(())
            })();
            let _ = std::fs::remove_file(&r.path);
            landed
        })
}

/// Whether an instance on this host mounts a checkout ([`mounted`] over
/// `state.toml`, the daemon's), as [`receivers`] asks before landing in an
/// existing one.
pub type InUse = std::sync::Arc<dyn Fn(&Path) -> bool + Send + Sync>;

/// Whether an instance in `state` mounts `checkout` directly: its `folder`,
/// or a `folders` entry bind-mounted as is (no worktree), is the checkout,
/// inside it or holds it. A worktree instance whose base is the checkout
/// runs in its own working tree: it doesn't count. Paths compare
/// canonicalized where they exist.
pub fn mounted(state: &crate::state::State, checkout: &Path) -> bool {
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let checkout = canon(checkout);
    state.instances.values().any(|i| {
        let direct = i.folders.iter().filter(|f| f.worktree.is_none()).map(|f| &f.base);
        std::iter::once(&i.folder).chain(direct).filter(|p| p.is_absolute()).map(|p| canon(p)).any(|p| p.starts_with(&checkout) || checkout.starts_with(&p))
    })
}

/// A bundle header's `to`, as `(root hash, path below it)`.
fn bundle_target(h: &Header) -> Result<(&str, &str)> {
    let to = h.to.as_deref().context("a bundle names where it goes (`to`)")?;
    let (hash, rest) = to.split_once('/').with_context(|| format!("a bundle goes below a root, not to {to}"))?;
    if !is_hash(hash) {
        bail!("`{hash}` isn't a root hash");
    }
    Ok((hash, rest))
}

/// A bundle header's `repo` and `refs` (both checked by
/// `transfer::check_header`), as `(repo id, branch, its WIP commit)`:
/// exactly one `refs/heads/<branch>`, besides it at most
/// `refs/devsandbox/wip/<branch>` (the same branch's), nothing else.
fn bundle_refs_of(h: &Header) -> Result<(&str, &str, Option<&str>)> {
    let id = h.repo.as_deref().context("a bundle names its repo (`repo`)")?;
    let refs = h.refs.as_ref().context("a bundle names its refs (`refs`)")?;
    let mut branches = refs.keys().filter_map(|n| n.strip_prefix("refs/heads/"));
    let (Some(branch), None) = (branches.next(), branches.next()) else {
        bail!("a bundle carries one branch (`refs/heads/…`), not {}", refs.keys().cloned().collect::<Vec<_>>().join(", "));
    };
    let wip = repo::wip_ref(branch);
    if let Some(other) = refs.keys().find(|n| !n.starts_with("refs/heads/") && **n != wip) {
        bail!("a bundle carries its branch and that branch's WIP ref ({wip}), not {other}");
    }
    Ok((id, branch, refs.get(&wip).map(String::as_str)))
}

/// The root tar at `tar` into `roots/<hash>/` (kept if there).
fn unpack_root(tar: &Path, roots: &Path, hash: &str) -> Result<()> {
    let dest = roots.join(hash);
    if dest.is_dir() {
        return Ok(());
    }
    let bytes = std::fs::read(tar).with_context(|| format!("cannot read {}", tar.display()))?;
    if short_hash(&bytes) != hash {
        bail!("the tar isn't root {hash}");
    }
    let temp = roots.join(format!(".{hash}.{}-{}.part", std::process::id(), next_seq()));
    let unpacked = tar::extract(&bytes, &temp).and_then(|()| match std::fs::rename(&temp, &dest) {
        Ok(()) => Ok(()),
        // Another transfer of the same root got there first.
        Err(_) if dest.is_dir() => Ok(()),
        Err(e) => Err(anyhow!(e).context(format!("cannot move the root to {}", dest.display()))),
    });
    let _ = std::fs::remove_dir_all(&temp);
    unpacked
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixStream;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::repo::tests::{git, repo_with_branches, scratch};
    use super::*;
    use crate::serve::endpoint;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn layout_keeps_each_folders_offset() {
        let l = layout(&p("/w/.devsandboxes"), &[p("/w/devsandboxes"), p("/w/stackblitz/bolt")]).unwrap();
        assert_eq!(l, Layout { root: p(".devsandboxes"), folders: vec![p("devsandboxes"), p("stackblitz/bolt")] });
        let l = layout(&p("/home/u/src/cfg"), &[p("/home/u/src/web"), p("/home/u/lib")]).unwrap();
        assert_eq!(l, Layout { root: p("src/cfg"), folders: vec![p("src/web"), p("lib")] });
        // No folder outside: the root under its own name.
        assert_eq!(layout(&p("/w/cfg"), &[]).unwrap(), Layout { root: p("cfg"), folders: vec![] });
        let err = |root: &str, f: &[&str]| layout(&p(root), &f.iter().map(|s| p(s)).collect::<Vec<_>>()).unwrap_err();
        assert!(err("/w/cfg", &["/w/cfg"]).contains("is in the config root"));
        assert!(err("/w/cfg", &["/w/cfg/app"]).contains("is in the config root"));
        assert!(err("/w/cfg", &["/w"]).contains("holds the config root"));
        assert!(err("/w/cfg", &["/w/a", "/w/a/b"]).contains("nest"));
        assert!(err("/", &[]).contains("too shallow"));
        // A folder elsewhere entirely climbs to the top: the root keeps all its names.
        assert_eq!(layout(&p("/w/cfg"), &[p("/x/y")]).unwrap(), Layout { root: p("w/cfg"), folders: vec![p("x/y")] });
    }

    /// `<dir>/cfg` (the config root, sandbox `web` with `folder = "../web"`,
    /// plus what must not travel) and `<dir>/web`, a checkout with no
    /// remote on `main` with a local `feature` ([`repo_with_branches`]).
    fn fixture(dir: &Path) -> PathBuf {
        repo_with_branches(dir, "web");
        let cfg = dir.join("cfg");
        for d in ["dockerfiles", ".worktrees/web-2", "shared-volumes/h", "shared-files/x", ".git/objects", "features/f/.git"] {
            std::fs::create_dir_all(cfg.join(d)).unwrap();
        }
        std::fs::write(cfg.join("devsandboxes.toml"), "[sandbox.web]\nfolder = \"../web\"\nimage = \"img\"\n").unwrap();
        std::fs::write(cfg.join("dockerfiles/web.Dockerfile"), "FROM x\n").unwrap();
        std::fs::write(cfg.join("features/f/install.sh"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(cfg.join("features/f/install.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(cfg.join(".worktrees/web-2/x"), "no").unwrap();
        std::fs::write(cfg.join(".git/HEAD"), "no").unwrap();
        std::fs::write(cfg.join("features/f/.git/HEAD"), "no").unwrap();
        std::os::unix::fs::symlink("dockerfiles/web.Dockerfile", cfg.join("Dockerfile")).unwrap();
        cfg
    }

    fn member_paths(tar: &[u8]) -> Vec<String> {
        tar::read(tar).unwrap().into_iter().map(|m| m.path).collect()
    }

    #[test]
    fn the_root_tar_skips_state_and_git_and_hashes_by_content() {
        let dir = scratch("pack");
        let cfg = fixture(&dir);
        let tar = pack_root(&cfg, &p("cfg")).unwrap();
        assert_eq!(
            member_paths(&tar),
            [
                "cfg",
                "cfg/Dockerfile",
                "cfg/devsandboxes.toml",
                "cfg/dockerfiles",
                "cfg/dockerfiles/web.Dockerfile",
                "cfg/features",
                "cfg/features/f",
                "cfg/features/f/install.sh",
            ]
        );
        // Stable: same content, same bytes, whatever the mtimes.
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(cfg.join("dockerfiles/web.Dockerfile"), "FROM x\n").unwrap();
        assert_eq!(short_hash(&pack_root(&cfg, &p("cfg")).unwrap()), short_hash(&tar));
        // The layout is in the bytes too.
        assert_ne!(short_hash(&pack_root(&cfg, &p("w/cfg")).unwrap()), short_hash(&tar));
        std::fs::write(cfg.join("dockerfiles/web.Dockerfile"), "FROM y\n").unwrap();
        assert_ne!(short_hash(&pack_root(&cfg, &p("cfg")).unwrap()), short_hash(&tar));
        // A symlink out of the root refuses it.
        std::os::unix::fs::symlink("../web", cfg.join("out")).unwrap();
        let err = format!("{:#}", pack_root(&cfg, &p("cfg")).unwrap_err());
        assert!(err.contains("symlink out points outside the config root"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `receivers(root)` taking one `send` over a socket pair.
    fn deliver(r: &Arc<Receivers>, header: &Header, body: &[u8]) -> Result<()> {
        transfer::send(r.accept().unwrap(), header, body)
    }

    fn tar_header(tar: &[u8]) -> Header {
        let (size, sha256) = transfer::digest(tar).unwrap();
        Header { kind: ROOT_TAR_KIND.into(), name: format!("root-{}.tar", short_hash(tar)), size, sha256, to: None, repo: None, refs: None }
    }

    /// A `bundle` header for `body` into `to`, of repo `repo` with `refs`.
    fn bundle_header(body: &[u8], to: &str, repo: &str, refs: &[(&str, &str)]) -> Header {
        let (size, sha256) = transfer::digest(body).unwrap();
        Header {
            kind: BUNDLE_KIND.into(),
            name: "repo.bundle".into(),
            size,
            sha256,
            to: Some(to.into()),
            repo: Some(repo.into()),
            refs: Some(refs.iter().map(|(n, id)| (n.to_string(), id.to_string())).collect()),
        }
    }

    #[test]
    fn the_worker_unpacks_a_root_once_and_refuses_bad_ones() {
        let dir = scratch("recv-root");
        let cfg = fixture(&dir);
        let fleet = dir.join("fleet");
        let r = Arc::new(receivers(&fleet, Receivers::new(|_| {}), free()));
        let tar = pack_root(&cfg, &p("cfg")).unwrap();
        let h = tar_header(&tar);
        deliver(&r, &h, &tar).unwrap();
        let tree = fleet.join("roots").join(short_hash(&tar));
        assert_eq!(std::fs::read_to_string(tree.join("cfg/devsandboxes.toml")).unwrap(), "[sandbox.web]\nfolder = \"../web\"\nimage = \"img\"\n");
        assert_eq!(std::fs::read_link(tree.join("cfg/Dockerfile")).unwrap(), p("dockerfiles/web.Dockerfile"));
        assert!(std::fs::metadata(tree.join("cfg/features/f/install.sh")).unwrap().permissions().mode() & 0o100 != 0);
        // Again: kept as is.
        std::fs::write(tree.join("cfg/marker"), "").unwrap();
        deliver(&r, &h, &tar).unwrap();
        assert!(tree.join("cfg/marker").exists());
        let leftovers: Vec<_> = std::fs::read_dir(fleet.join("roots")).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(leftovers.len(), 2, "the tree and `.in`: {leftovers:?}");
        assert_eq!(std::fs::read_dir(fleet.join("roots/.in")).unwrap().count(), 0);

        // A name that isn't its hash; a member escaping.
        let err = format!("{:#}", deliver(&r, &Header { name: "root-0000000000000000.tar".into(), ..h.clone() }, &tar).unwrap_err());
        assert!(err.contains("a root tar is named"), "{err}");
        let mut evil = tar::Writer::default();
        evil.dir("cfg").unwrap();
        let mut bytes = evil.finish();
        bytes.truncate(bytes.len() - 1024);
        let mut raw = [0u8; 512];
        raw[..9].copy_from_slice(b"../escape");
        raw[100..108].copy_from_slice(b"0000644\0");
        raw[124..136].copy_from_slice(b"00000000000\0");
        raw[156] = b'0';
        raw[148..156].copy_from_slice(b"        ");
        let sum: u32 = raw.iter().map(|&b| u32::from(b)).sum();
        raw[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        bytes.extend_from_slice(&raw);
        bytes.extend_from_slice(&[0; 1024]);
        let err = format!("{:#}", deliver(&r, &tar_header(&bytes), &bytes).unwrap_err());
        assert!(err.contains("unsafe path \"../escape\""), "{err}");
        assert!(!dir.join("fleet/roots/../escape").exists() && !fleet.join("escape").exists());
        assert!(!fleet.join("roots").join(short_hash(&bytes)).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bundle_needs_its_root_and_a_plain_target() {
        let dir = scratch("recv-bundle");
        let fleet = dir.join("fleet");
        let r = Arc::new(receivers(&fleet, Receivers::new(|_| {}), free()));
        let oid = "a".repeat(40);
        let h = bundle_header(b"x", "0123456789abcdef/web", "fedcba9876543210", &[("refs/heads/main", &oid)]);
        let err = format!("{:#}", deliver(&r, &h, b"x").unwrap_err());
        assert!(err.contains("root 0123456789abcdef isn't here"), "{err}");
        // Its repo and refs: one branch, at most that branch's WIP ref.
        let err = format!("{:#}", deliver(&r, &Header { repo: None, ..h.clone() }, b"x").unwrap_err());
        assert!(err.contains("names its repo"), "{err}");
        let err = format!("{:#}", deliver(&r, &Header { refs: None, ..h.clone() }, b"x").unwrap_err());
        assert!(err.contains("names its refs"), "{err}");
        for refs in [
            vec![("refs/heads/main", oid.as_str()), ("refs/heads/b", oid.as_str())],
            vec![("refs/devsandbox/wip/main", oid.as_str())],
        ] {
            let bad = bundle_header(b"x", "0123456789abcdef/web", "fedcba9876543210", &refs);
            let err = format!("{:#}", deliver(&r, &bad, b"x").unwrap_err());
            assert!(err.contains("carries one branch"), "{refs:?}: {err}");
        }
        for other in ["refs/devsandbox/wip/b", "refs/tags/v1", "refs/devsandbox/root"] {
            let bad = bundle_header(b"x", "0123456789abcdef/web", "fedcba9876543210", &[("refs/heads/main", &oid), (other, &oid)]);
            let err = format!("{:#}", deliver(&r, &bad, b"x").unwrap_err());
            assert!(err.contains(&format!("not {other}")), "{other}: {err}");
        }
        let err = format!("{:#}", deliver(&r, &Header { to: Some("web".into()), ..h.clone() }, b"x").unwrap_err());
        assert!(err.contains("below a root"), "{err}");
        // Through a symlink the root's tar made: refused.
        let tree = fleet.join("roots/0123456789abcdef");
        std::fs::create_dir_all(&tree).unwrap();
        std::os::unix::fs::symlink("/tmp", tree.join("web")).unwrap();
        let err = format!("{:#}", deliver(&r, &h, b"x").unwrap_err());
        assert!(err.contains("isn't a directory"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn what_cant_move_says_why() {
        let dir = scratch("not-movable");
        let cfg = fixture(&dir);
        let why = |toml: &str, branch: Option<&str>| {
            std::fs::write(cfg.join("devsandboxes.toml"), toml).unwrap();
            match offer(&cfg, "web", branch) {
                Err(Unprepared::NotMovable(why)) => why,
                other => panic!("{other:?}"),
            }
        };
        std::fs::create_dir_all(dir.join("plain")).unwrap();
        assert!(why("[sandbox.web]\nfolder = \"../plain\"\nimage = \"i\"\n", None).contains("isn't a git checkout"));
        let inside = why("[sandbox.web]\nfolder = \"dockerfiles\"\nimage = \"i\"\n", None);
        assert!(inside.contains("is in the config root (the config root travels as a tar"), "{inside}");
        let web = "[sandbox.web]\nfolder = \"../web\"\nimage = \"i\"\n";
        git(&dir.join("web"), &["checkout", "-q", "--detach"]);
        assert!(why(web, None).contains("web has a detached HEAD (check out a branch first)"));
        assert!(why(web, Some("nope")).contains("detached HEAD"), "a missing branch falls back to the current one");
        // A detached checkout's requested, existing branch moves as committed.
        let got = offer(&cfg, "web", Some("feature")).unwrap();
        assert_eq!((got.repos[0].branch.as_str(), got.repos[0].wip), ("feature", false));
        git(&dir.join("web"), &["checkout", "-q", "main"]);
        assert!(why("[sandbox.web]\nfolder = \"../web\"\nimage = \"i\"\nfolders = { \"/w/x\" = \"../${localEnv:X}\" }\n", None)
            .contains("uses a variable"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ties_name_what_keeps_a_sandbox_home() {
        let dir = scratch("ties");
        let cfg = dir.join("cfg");
        for d in ["cfg/data", "web/sub", "lib", "elsewhere"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        let ties_of_toml = |extra: &str| {
            let text = format!(
                "[services.g]\nimage = \"redis\"\nscope = \"global\"\n[services.i]\nimage = \"redis\"\n\
                 [sandbox.s]\nfolder = \"../web\"\nimage = \"alpine\"\nfolders = {{ \"/w/lib\" = \"../lib\" }}\n{extra}\n"
            );
            let config = Config::parse(&text).unwrap();
            let s = config.resolve_sandbox("s").unwrap();
            ties(&cfg, &config, &s)
        };
        // What travels (the root, the moved folders) and what isn't a bind.
        let web = dir.join("web").display().to_string();
        let lib = dir.join("lib").display().to_string();
        let clean = format!(
            "services = [\"i\"]\nmounts = [\"source=${{configDir}}/data,target=/d,type=bind\", \
             \"source=${{localWorkspaceFolder}}/sub,target=/s\", \"source={web}/sub,target=/w\", \
             \"source={lib},target=/l\", \"source=v-${{instance}},target=/v,type=volume\", \
             {{ type = \"tmpfs\", target = \"/t\" }}]"
        );
        assert_eq!(ties_of_toml(&clean), Vec::<String>::new());
        // A build inside the root isn't a tie; one whose context climbs out is.
        std::fs::write(cfg.join("Dockerfile"), "FROM alpine\n").unwrap();
        assert_eq!(ties_of_toml("build = { dockerfile = \"Dockerfile\" }"), Vec::<String>::new());
        assert_eq!(
            ties_of_toml("build = { dockerfile = \"Dockerfile\", context = \"../elsewhere\" }"),
            [format!("build context `{}` (outside the config root)", cfg.join("../elsewhere").display())]
        );
        // The pin: a tie on its own, and first among others.
        assert_eq!(ties_of_toml("placement = \"home\""), ["`placement = \"home\"`"]);
        assert_eq!(ties_of_toml("privileged = true\nplacement = \"home\""), ["`placement = \"home\"`", "privileged"]);
        assert_eq!(ties_of_toml(""), Vec::<String>::new());
        assert_eq!(ties_of_toml("caches = [\"cargo\"]"), Vec::<String>::new());
        let out = dir.join("elsewhere").display().to_string();
        for (extra, want) in [
            (format!("mounts = [\"source={out},target=/o\"]"), format!("bind mount of `{out}` (outside the config root)")),
            (
                "mounts = [{ source = \"${sharedVolumes}/x\", target = \"/x\" }]".into(),
                format!("shared-volumes mount `{}`", cfg.join("shared-volumes/x").display()),
            ),
            (
                "mounts = [\"source=/var/run/docker.sock,target=/s\"]".into(),
                "runtime socket mount `/var/run/docker.sock`".into(),
            ),
            ("containerEnv = { T = \"${localEnv:X}\" }".into(), "`${localEnv:X}` (this host's env)".into()),
            ("privileged = true".into(), "privileged".into()),
            ("initializeCommand = \"true\"".into(), "`initializeCommand`".into()),
            ("services = [\"i\", \"g\"]".into(), "global service `g`".into()),
        ] {
            assert_eq!(ties_of_toml(&extra), [want], "{extra}");
        }
        // All of them, a `${localEnv:…}` mount counted once, as its variable.
        let all = ties_of_toml(&format!(
            "privileged = true\nmounts = [\"source=${{localEnv:HOME}}/.gh,target=/g\", \"source={out},target=/o\"]"
        ));
        assert_eq!(
            all,
            [
                "privileged".to_string(),
                "`${localEnv:HOME}` (this host's env)".into(),
                format!("bind mount of `{out}` (outside the config root)"),
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn offer_refuses_a_tied_sandbox() {
        let dir = scratch("offer-tied");
        let cfg = fixture(&dir);
        std::fs::write(cfg.join("devsandboxes.toml"), "[sandbox.web]\nfolder = \"../web\"\nimage = \"i\"\ninitializeCommand = \"true\"\nprivileged = true\n")
            .unwrap();
        match offer(&cfg, "web", None) {
            Err(Unprepared::NotMovable(why)) => assert_eq!(why, "`initializeCommand`, privileged"),
            other => panic!("{other:?}"),
        }
        assert_eq!(ties_of(&cfg, "web").unwrap(), ["`initializeCommand`", "privileged"]);
        assert!(ties_of(&cfg, "nope").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The machine's end of one `API` stream, as the daemon answers a peer
    /// whose fleet root is `fleet`: a hello, then `code.haves`
    /// ([`haves_call`]) and `instances.fetch` ([`fetch::fetch_call`], over
    /// the `state.toml` next to `fleet`'s parent); anything else unknown.
    /// Reads bounded.
    fn serve_api(stream: UnixStream, fleet: &Path) {
        use std::io::{BufRead, BufReader, Write};

        use crate::serve::proto::{HelloResult, PROTOCOL, Request, Response};
        stream.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut out = stream;
        let mut line = String::new();
        while reader.read_line(&mut line).is_ok_and(|n| n > 0) {
            let req: Request = serde_json::from_str(&line).unwrap();
            let resp = match req.method.as_str() {
                "hello" => {
                    let hello = HelloResult { version: "0.0.0".into(), build: 0, protocol: PROTOCOL, handoff: false };
                    Response::ok(req.id, serde_json::to_value(hello).unwrap())
                }
                HAVES_METHOD => match haves_call(fleet, &req.params) {
                    Ok(v) => Response::ok(req.id, v),
                    Err(e) => Response::err(req.id, e.code, e.message),
                },
                fetch::FETCH_METHOD => {
                    let state = crate::state::State::load_at(&fleet.parent().unwrap().join("state.toml")).unwrap();
                    match fetch::fetch_call(&state, fleet, &req.params) {
                        Ok(v) => Response::ok(req.id, v),
                        Err(e) => Response::err(req.id, e.code, e.message),
                    }
                }
                m => Response::err(req.id, "unknown-method", m.to_string()),
            };
            if writeln!(out, "{}", serde_json::to_string(&resp).unwrap()).is_err() {
                return;
            }
            line.clear();
        }
    }

    /// No checkout is in use.
    fn free() -> InUse {
        Arc::new(|_| false)
    }

    /// [`link_with`], no checkout in use.
    fn link(dir: &Path, fleet: &Path) -> PathBuf {
        link_with(dir, fleet, free())
    }

    /// A link's two ends in-process (this host the opener, the machine the
    /// answerer), the machine's receivers [`receivers`] into `fleet` (with
    /// `in_use`) and its `API` [`serve_api`]; this host's link socket.
    fn link_with(dir: &Path, fleet: &Path, in_use: InUse) -> PathBuf {
        use crate::devsbd::proto::channel;
        use crate::serve::fleet::link::{Endpoint, Serves, answerer_with, opener_with};
        let hub_dir = dir.join("hub");
        std::fs::create_dir_all(&hub_dir).unwrap();
        let socket = endpoint::fleet_socket(&hub_dir, "w");
        let ours = Serves::default().transfers(Arc::new(Receivers::new(|_| {})));
        let api_fleet = fleet.to_path_buf();
        let api = move || {
            let (mine, theirs) = UnixStream::pair().ok()?;
            let fleet = api_fleet.clone();
            std::thread::spawn(move || serve_api(mine, &fleet));
            Some(theirs.into())
        };
        let theirs = Serves::default().with(channel::API, api).transfers(Arc::new(receivers(fleet, Receivers::new(|_| {}), in_use)));
        let (a_r, o_w) = std::io::pipe().unwrap();
        let (o_r, a_w) = std::io::pipe().unwrap();
        let path = socket.clone();
        std::thread::spawn(move || {
            opener_with(&super::super::LINK, o_r, o_w, Endpoint { socket: Some(&path), serves: ours, outlet: None, auth: None }, |_| Ok(()))
        });
        std::thread::spawn(move || {
            answerer_with(&super::super::LINK, a_r, a_w, Endpoint { serves: theirs, ..Endpoint::default() }, |_| Ok(()), || {})
        });
        let start = std::time::Instant::now();
        while UnixStream::connect(&socket).is_err() {
            assert!(start.elapsed() < Duration::from_secs(5), "the link socket accepts");
            std::thread::sleep(Duration::from_millis(10));
        }
        socket
    }

    /// What sending must leave alone in a source checkout: its index file
    /// (hashed), status, what `HEAD` names, the branch tips, the stash and
    /// devsandbox's refs.
    fn source_state(repo: &Path) -> Vec<String> {
        vec![
            std::fs::read(repo.join(".git/index")).map(|b| short_hash(&b)).unwrap_or_default(),
            git(repo, &["status", "--porcelain=v1", "-uall"]),
            git(repo, &["symbolic-ref", "HEAD"]),
            git(repo, &["for-each-ref", "refs/heads"]),
            git(repo, &["stash", "list"]),
            git(repo, &["for-each-ref", "refs/devsandbox"]),
        ]
    }

    /// No transient ref and no temp file of a send left in `repo`.
    fn assert_no_leftovers(repo: &Path) {
        assert!(!git(repo, &["for-each-ref", "--format=%(refname)", "refs/devsandbox"]).contains("/wip/send-"));
        let temps: Vec<_> = std::fs::read_dir(repo.join(".git"))
            .unwrap()
            .filter_map(|e| e.unwrap().file_name().into_string().ok())
            .filter(|n| n.starts_with("devsandbox-"))
            .collect();
        assert_eq!(temps, Vec::<String>::new());
    }

    #[test]
    fn prepare_moves_a_sandbox_onto_a_worker_over_its_link() {
        let dir = scratch("prepare");
        let cfg = fixture(&dir);
        std::fs::write(
            cfg.join("devsandboxes.toml"),
            "[sandbox.web]\nfolder = \"../web\"\nimage = \"img\"\nfolders = { \"/workspaces/lib\" = \"../lib\", \"/workspaces/cfg\" = \".\" }\n",
        )
        .unwrap();
        // `../lib`: a second repo, on `feature`; `../web` has uncommitted work on `main`.
        let (web, lib) = (dir.join("web"), repo_with_branches(&dir, "lib"));
        git(&lib, &["checkout", "-q", "feature"]);
        std::fs::write(web.join("README"), "dirty\n").unwrap();
        let before = (source_state(&web), source_state(&lib));
        let fleet = dir.join("worker/fleet/hub");
        let t = LinkTransfer::for_worker(link(&dir, &fleet).parent().unwrap(), "w");

        // `feature` isn't checked out in `web`: it moves as committed, no WIP.
        let got = prepare(&t, &fleet, &cfg, "web", Some("feature")).unwrap();
        let tree = got.remote_root.parent().unwrap().to_path_buf();
        assert_eq!(got.remote_root, tree.join("cfg"));
        assert_eq!(tree.parent().unwrap(), fleet.join("roots"));
        assert_eq!(got.folders, [(web.clone(), tree.join("web"), "feature".to_string()), (lib.clone(), tree.join("lib"), "feature".to_string())]);
        // `folder = "../web"` resolves from the moved root to the moved checkout.
        let moved = got.remote_root.join("../web").canonicalize().unwrap();
        assert_eq!(moved, tree.join("web").canonicalize().unwrap());
        assert_eq!(std::fs::read_to_string(moved.join("F")).unwrap(), "feature\n");
        assert_eq!(std::fs::read_to_string(moved.join("README")).unwrap(), "main\n");
        assert_eq!(git(&moved, &["symbolic-ref", "--short", "HEAD"]), "feature");
        assert_eq!(git(&moved, &["status", "--porcelain=v1", "-uall"]), "");
        assert_eq!(git(&moved, &["remote"]), "");
        assert!(got.remote_root.join("dockerfiles/web.Dockerfile").exists());
        assert!(!got.remote_root.join(".worktrees").exists() && !got.remote_root.join(".git").exists());

        // No `--branch`: the checkout's own (`main`); same root, kept; the
        // existing checkout, which no instance mounts, is recut onto `main`
        // with its WIP.
        let again = prepare(&t, &fleet, &cfg, "web", None).unwrap();
        assert_eq!(again.remote_root, got.remote_root);
        assert_eq!(again.folders[0].2, "main");
        assert_eq!(git(&moved, &["rev-parse", "main"]), git(&web, &["rev-parse", "main"]));
        assert_eq!(git(&moved, &["symbolic-ref", "--short", "HEAD"]), "main");
        assert_eq!(std::fs::read_to_string(moved.join("README")).unwrap(), "dirty\n");
        // A requested branch the checkout lacks: its own, which a new branch starts at.
        let new = prepare(&t, &fleet, &cfg, "web", Some("new-one")).unwrap();
        assert_eq!(new.folders[0].2, "main");
        let ask = crate::serve::fleet::placed::Ask { branch: Some("new-one"), base: None };
        assert_eq!(crate::serve::fleet::placed::start_point(&ask, &new).as_deref(), Some("main"));
        assert_eq!((source_state(&web), source_state(&lib)), before, "the sources are untouched");
        assert_no_leftovers(&web);
        assert_no_leftovers(&lib);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_checkout_moves_with_its_unpushed_and_uncommitted_work() {
        let dir = scratch("prepare-wip");
        let cfg = fixture(&dir);
        let web = dir.join("web");
        // A local-only branch with a commit no remote has (there is none).
        git(&web, &["checkout", "-q", "-b", "local-only"]);
        for (f, body) in [("keep", "k\n"), ("edit", "e\n"), ("gone", "g\n"), ("staged", "s\n"), (".gitignore", "*.log\n")] {
            std::fs::write(web.join(f), body).unwrap();
        }
        git(&web, &["add", "-A"]);
        git(&web, &["commit", "-q", "-m", "local"]);
        // A stash, then work: modified, deleted, staged, untracked, ignored.
        std::fs::write(web.join("keep"), "stashed\n").unwrap();
        git(&web, &["stash", "-q"]);
        std::fs::write(web.join("edit"), "e2\n").unwrap();
        std::fs::remove_file(web.join("gone")).unwrap();
        std::fs::write(web.join("staged"), "s2\n").unwrap();
        git(&web, &["add", "staged"]);
        std::fs::create_dir_all(web.join("sub")).unwrap();
        std::fs::write(web.join("sub/new"), "n\n").unwrap();
        std::fs::write(web.join("x.log"), "ignored\n").unwrap();
        assert_eq!(git(&web, &["remote"]), "", "no origin");
        let before = source_state(&web);
        let fleet = dir.join("worker/fleet/hub");
        let t = LinkTransfer::for_worker(link(&dir, &fleet).parent().unwrap(), "w");

        let got = prepare(&t, &fleet, &cfg, "web", None).unwrap();
        let moved = got.folders[0].1.clone();
        assert_eq!(got.folders[0].2, "local-only");
        assert_eq!(git(&moved, &["symbolic-ref", "--short", "HEAD"]), "local-only");
        assert_eq!(git(&moved, &["rev-parse", "HEAD"]), git(&web, &["rev-parse", "local-only"]));
        assert_eq!(git(&moved, &["status", "--porcelain=v1", "-uall"]), "M edit\n D gone\n M staged\n?? sub/new");
        for f in ["keep", "edit", "staged", "sub/new", ".gitignore"] {
            assert_eq!(std::fs::read(web.join(f)).unwrap(), std::fs::read(moved.join(f)).unwrap(), "{f}");
        }
        assert!(!moved.join("gone").exists() && !moved.join("x.log").exists(), "deleted stays deleted, ignored doesn't travel");
        assert_eq!(git(&moved, &["for-each-ref", "refs/devsandbox"]), "");
        assert_eq!(source_state(&web), before, "the source is untouched");
        assert_no_leftovers(&web);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unborn_repo_moves_its_untracked_files() {
        let dir = scratch("prepare-unborn");
        let cfg = fixture(&dir);
        let web = dir.join("fresh");
        std::fs::create_dir_all(web.join("sub")).unwrap();
        git(&web, &["init", "-q", "-b", "main"]);
        std::fs::write(web.join("a"), "a\n").unwrap();
        std::fs::write(web.join("sub/b"), "b\n").unwrap();
        std::fs::write(cfg.join("devsandboxes.toml"), "[sandbox.web]\nfolder = \"../fresh\"\nimage = \"img\"\n").unwrap();
        let fleet = dir.join("worker/fleet/hub");
        let t = LinkTransfer::for_worker(link(&dir, &fleet).parent().unwrap(), "w");

        let got = prepare(&t, &fleet, &cfg, "web", None).unwrap();
        let moved = got.folders[0].1.clone();
        let root = git(&web, &["rev-parse", repo::ROOT_REF]);
        assert_eq!(git(&web, &["for-each-ref", "--format=%(refname)", "refs/devsandbox"]), repo::ROOT_REF, "the one ref kept");
        assert_eq!(git(&web, &["status", "--porcelain=v1", "-uall"]), "?? a\n?? sub/b", "still unborn");
        assert_eq!(git(&web, &["symbolic-ref", "HEAD"]), "refs/heads/main");
        assert_eq!(git(&moved, &["rev-parse", "HEAD"]), root, "the worker's main is the devsandbox root");
        assert_eq!(git(&moved, &["status", "--porcelain=v1", "-uall"]), "?? a\n?? sub/b");
        assert_eq!(std::fs::read_to_string(moved.join("sub/b")).unwrap(), "b\n");
        // Again: the same root, made once.
        prepare(&t, &fleet, &cfg, "web", None).unwrap();
        assert_eq!(git(&web, &["rev-parse", repo::ROOT_REF]), root);
        assert_no_leftovers(&web);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_refused_transfer_fails_prepare() {
        /// Refuses every send of a kind in its list.
        struct Refuse(&'static [&'static str], Mutex<Vec<String>>);
        impl Transfer for Refuse {
            fn send(&self, header: &Header, body: &mut dyn Read) -> Result<()> {
                std::io::copy(body, &mut std::io::sink())?;
                self.1.lock().unwrap().push(header.kind.clone());
                if self.0.contains(&header.kind.as_str()) {
                    bail!("transfer refused: full")
                }
                Ok(())
            }
        }
        let dir = scratch("refused");
        let cfg = fixture(&dir);
        let web = dir.join("web");
        std::fs::write(web.join("README"), "dirty\n").unwrap();
        let before = source_state(&web);
        let t = Refuse(&[ROOT_TAR_KIND], Mutex::default());
        match prepare(&t, &dir.join("fleet"), &cfg, "web", None) {
            Err(Unprepared::Failed(e)) => assert!(format!("{e:#}").contains("cannot send the config root: transfer refused: full")),
            other => panic!("{other:?}"),
        }
        assert_eq!(*t.1.lock().unwrap(), ["root-tar"]);
        // The bundle refused: the snapshot's ref and the bundle are gone all the same.
        let t = Refuse(&[BUNDLE_KIND], Mutex::default());
        match prepare(&t, &dir.join("fleet"), &cfg, "web", None) {
            Err(Unprepared::Failed(e)) => {
                assert!(format!("{e:#}").contains(&format!("cannot send {}: transfer refused: full", web.display())), "{e:#}")
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(*t.1.lock().unwrap(), ["root-tar", "bundle"]);
        assert_eq!(source_state(&web), before);
        assert_no_leftovers(&web);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A [`Transfer`] recording each send's kind and size, passing it on.
    struct Recorded<'a> {
        inner: &'a dyn Transfer,
        sent: Mutex<Vec<(String, u64)>>,
    }

    impl Transfer for Recorded<'_> {
        fn haves(&self, repos: &[String]) -> Result<Haves> {
            self.inner.haves(repos)
        }
        fn send(&self, header: &Header, body: &mut dyn Read) -> Result<()> {
            self.sent.lock().unwrap().push((header.kind.clone(), header.size));
            self.inner.send(header, body)
        }
    }

    impl Recorded<'_> {
        fn take(&self) -> Vec<(String, u64)> {
            std::mem::take(&mut *self.sent.lock().unwrap())
        }
    }

    #[test]
    fn repeat_sends_carry_only_what_the_worker_lacks() {
        let dir = scratch("repeat");
        let cfg = fixture(&dir);
        let web = dir.join("web");
        // Some bulk, so a delta is visibly smaller than the first send.
        let bulk: String = (0..20_000).map(|i| format!("line {i}\n")).collect();
        std::fs::write(web.join("bulk"), bulk).unwrap();
        git(&web, &["add", "bulk"]);
        git(&web, &["commit", "-q", "-m", "bulk"]);
        let fleet = dir.join("worker/fleet/hub");
        let link = LinkTransfer::for_worker(link(&dir, &fleet).parent().unwrap(), "w");
        let t = Recorded { inner: &link, sent: Mutex::default() };
        let id = repo::identity(Repo::Checkout(&web), "HEAD").unwrap();
        // Nothing there yet: no roots, no tips.
        assert_eq!(link.haves(std::slice::from_ref(&id)).unwrap(), Haves::default());

        let got = prepare(&t, &fleet, &cfg, "web", None).unwrap();
        let first = t.take();
        assert_eq!(first.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(), ["root-tar", "bundle"]);
        let checkout = got.folders[0].1.clone();
        assert_eq!(git(&checkout, &["symbolic-ref", "--short", "HEAD"]), "main");
        assert_eq!(git(&checkout, &["remote"]), "", "no remote on the worker");
        let cache = cache_path(&fleet, &id);
        let main = git(&web, &["rev-parse", "main"]);
        assert_eq!(git(&cache, &["rev-parse", "refs/heads/main"]), main);
        assert_eq!(git(&cache, &["rev-parse", &format!("{}^", repo::wip_ref("main"))]), main, "the snapshot sits on main");
        // The worker answers its root and the cache's tips, over the link.
        let haves = link.haves(std::slice::from_ref(&id)).unwrap();
        let hash = got.remote_root.parent().unwrap().file_name().unwrap().to_str().unwrap().to_string();
        assert_eq!(haves.roots, [hash].into());
        assert!(haves.tips[&id].contains(&main), "{haves:?}");

        // A new commit, then new uncommitted work: only the bundle goes,
        // only the delta; the existing checkout, which no instance mounts,
        // is recut to it, the new snapshot applied.
        std::fs::write(web.join("README"), "two\n").unwrap();
        git(&web, &["commit", "-q", "-am", "two"]);
        std::fs::write(web.join("later"), "wip\n").unwrap();
        prepare(&t, &fleet, &cfg, "web", None).unwrap();
        let second = t.take();
        assert_eq!(second.len(), 1, "the root isn't sent again: {second:?}");
        assert!(second[0].1 > 0 && second[0].1 < first[1].1, "a delta: {second:?} after {first:?}");
        assert_eq!(std::fs::read_to_string(checkout.join("README")).unwrap(), "two\n");
        assert_eq!(std::fs::read_to_string(checkout.join("later")).unwrap(), "wip\n", "the new WIP applied");
        let wip = git(&cache, &["rev-parse", &repo::wip_ref("main")]);
        assert_eq!(git(&cache, &["show", &format!("{wip}:later")]), "wip");

        // A committed branch the worker has: an empty bundle, still answered `ok`.
        prepare(&t, &fleet, &cfg, "web", Some("feature")).unwrap();
        t.take();
        prepare(&t, &fleet, &cfg, "web", Some("feature")).unwrap();
        assert_eq!(t.take(), [("bundle".to_string(), 0)]);
        assert_eq!(git(&checkout, &["rev-parse", "feature"]), git(&web, &["rev-parse", "feature"]));
        assert_eq!(std::fs::read_dir(fleet.join("roots/.in")).unwrap().count(), 0, "no body left behind");
        assert_no_leftovers(&web);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_hub_mirror_fills_the_hubs_cache_only() {
        let dir = scratch("mirror");
        let cfg = fixture(&dir);
        let web = dir.join("web");
        let bulk: String = (0..20_000).map(|i| format!("line {i}\n")).collect();
        std::fs::write(web.join("bulk"), bulk).unwrap();
        git(&web, &["add", "bulk"]);
        git(&web, &["commit", "-q", "-m", "bulk"]);
        std::fs::write(web.join("README"), "dirty\n").unwrap();
        let before = source_state(&web);
        let (worker, hub) = (dir.join("worker/fleet/m"), dir.join("hub/fleet/m"));
        let to_worker = LinkTransfer::for_worker(link(&dir.join("w"), &worker).parent().unwrap(), "w");
        let to_hub = LinkTransfer::for_worker(link(&dir.join("k"), &hub).parent().unwrap(), "w");
        let t = Recorded { inner: &to_hub, sent: Mutex::default() };
        let none = Vec::<(PathBuf, String)>::new();

        let got = prepare(&to_worker, &worker, &cfg, "web", None).unwrap();
        assert_eq!(got.notes, Vec::<String>::new());
        let [sent] = got.sent.as_slice() else { panic!("{:?}", got.sent) };
        assert_eq!(sent.host, web);
        assert_eq!(mirror(&got.sent, &t), none);
        let first = t.take();
        assert!(matches!(first.as_slice(), [(k, n)] if k == "bundle" && *n > 0), "{first:?}");
        // The same refs at the same commits as the worker's cache, nothing else.
        let (hub_cache, worker_cache) = (cache_path(&hub, &sent.id), cache_path(&worker, &sent.id));
        let refs = |c: &Path| git(c, &["for-each-ref", "--format=%(refname) %(objectname)"]);
        assert_eq!(refs(&hub_cache), refs(&worker_cache));
        assert_eq!(git(&hub_cache, &["rev-parse", "refs/heads/main"]), git(&web, &["rev-parse", "main"]));
        let wip = git(&hub_cache, &["rev-parse", &repo::wip_ref("main")]);
        assert_eq!(git(&hub_cache, &["show", &format!("{wip}:README")]), "dirty");
        assert!(!hub.join("roots").exists(), "no root, no checkout");
        let mut entries: Vec<String> = std::fs::read_dir(hub.join("repos")).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        entries.sort();
        assert_eq!(entries, [".in".to_string(), format!("{}.git", sent.id)]);
        assert_eq!(std::fs::read_dir(hub.join("repos/.in")).unwrap().count(), 0, "no body left behind");

        // Nothing new: a 0-byte body, still ok.
        assert_eq!(mirror(&got.sent, &t), none);
        assert_eq!(t.take(), [("bundle".to_string(), 0)]);
        assert_eq!(source_state(&web), before, "the source is untouched");
        assert_no_leftovers(&web);
        // A new commit: only the delta.
        std::fs::write(web.join("two"), "two\n").unwrap();
        git(&web, &["add", "two"]);
        git(&web, &["commit", "-q", "-m", "two"]);
        let again = prepare(&to_worker, &worker, &cfg, "web", None).unwrap();
        assert_eq!(mirror(&again.sent, &t), none);
        let second = t.take();
        assert!(second.len() == 1 && second[0].1 > 0 && second[0].1 < first[0].1, "a delta: {second:?} after {first:?}");
        assert_eq!(refs(&hub_cache), refs(&worker_cache));
        assert_no_leftovers(&web);

        // A commit the checkout doesn't have: that repo's failure, no ref left.
        let gone = Sent { refs: [("refs/heads/main".to_string(), "1".repeat(40))].into(), ..sent.clone() };
        let failed = mirror(std::slice::from_ref(&gone), &t);
        assert!(matches!(failed.as_slice(), [(h, why)] if *h == web && why.contains("cannot point a ref at")), "{failed:?}");
        assert_no_leftovers(&web);
        // A hub that can't be asked: every repo's failure.
        struct NoHaves;
        impl Transfer for NoHaves {
            fn haves(&self, _: &[String]) -> Result<Haves> {
                bail!("unreachable")
            }
            fn send(&self, _: &Header, _: &mut dyn Read) -> Result<()> {
                unreachable!()
            }
        }
        assert_eq!(mirror(&got.sent, &NoHaves), [(web.clone(), "cannot ask the hub what it has: unreachable".to_string())]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_placed_instance_is_fetched_back_over_the_link_into_hidden_refs() {
        let dir = scratch("fetch-back-link");
        let cfg = fixture(&dir);
        let web = dir.join("web");
        std::fs::write(web.join("README"), "dirty\n").unwrap();
        let fleet = dir.join("worker/fleet/hub");
        let t = LinkTransfer::for_worker(link(&dir, &fleet).parent().unwrap(), "w");
        let got = prepare(&t, &fleet, &cfg, "web", None).unwrap();
        let moved = crate::serve::fleet::placed::MovedFolder::of(&got).unwrap();
        assert_eq!((moved.local.as_path(), moved.branch.as_str()), (web.as_path(), "main"));
        // On the worker: an instance on that checkout commits, then leaves work uncommitted.
        let there = moved.remote.clone();
        let instance = format!(
            "[instance.web-2]\nsandbox = \"web\"\ninstance_id = \"web-2\"\ncontainer = \"c\"\nworkspace = \"/w\"\ncreated_unix = 0\nfolder = {:?}\n",
            there.to_str().unwrap()
        );
        std::fs::write(fleet.parent().unwrap().join("state.toml"), instance).unwrap();
        git(&there, &["commit", "-q", "-am", "on the worker"]);
        std::fs::write(there.join("later"), "wip\n").unwrap();
        let before = source_state(&web);
        let entry = crate::serve::fleet::placements::Entry {
            machine: "w".into(),
            instance: "web-2".into(),
            sandbox: "web".into(),
            root: cfg.clone(),
            folder: web.clone(),
            remote: there.clone(),
            repo: moved.repo.clone(),
            branch: moved.branch.clone(),
        };
        let back = fetch::fetch_back(&entry, |haves| t.fetch_instance("web-2", haves), &t).unwrap();
        assert!(back.fetched > 0);
        let heads = "refs/devsandbox/w/web-2/heads/main";
        assert_eq!(git(&web, &["rev-parse", heads]), git(&there, &["rev-parse", "main"]));
        assert_eq!(git(&web, &["show", &format!("{heads}:README")]), "dirty", "the worker's commit");
        assert_eq!(git(&web, &["show", "refs/devsandbox/w/web-2/wip:later"]), "wip");
        let after = source_state(&web);
        assert_eq!(after[..5], before[..5], "the user's index, status, HEAD, branches and stash untouched");
        assert_eq!(git(&there, &["rev-parse", "refs/devsandbox/wip/web-2"]), back.refs["refs/devsandbox/w/web-2/wip"], "the WIP stays there");
        assert_eq!(std::fs::read_dir(fleet.join(fetch::OUT_DIR)).unwrap().count(), 0, "pulled, so removed");
        assert_no_leftovers(&web);
        // Nothing new: no bundle.
        let again = fetch::fetch_back(&entry, |haves| t.fetch_instance("web-2", haves), &t).unwrap();
        assert_eq!((again.fetched, again.refs), (0, back.refs));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cache_only_bundle_keeps_every_check() {
        let dir = scratch("recv-cache-only");
        let fleet = dir.join("fleet");
        let r = Arc::new(receivers(&fleet, Receivers::new(|_| {}), free()));
        let oid = "a".repeat(40);
        let cache_only = |refs: &[(&str, &str)]| Header { to: None, ..bundle_header(b"", "x/y", "fedcba9876543210", refs) };
        let err = format!("{:#}", deliver(&r, &cache_only(&[("refs/heads/main", &oid)]), b"").unwrap_err());
        assert!(err.contains("is neither in the bundle nor already here"), "{err}");
        let err = format!("{:#}", deliver(&r, &cache_only(&[("refs/heads/main", &oid), ("refs/tags/v1", &oid)]), b"").unwrap_err());
        assert!(err.contains("not refs/tags/v1"), "{err}");
        let err = format!("{:#}", deliver(&r, &cache_only(&[("refs/devsandbox/wip/main", &oid)]), b"").unwrap_err());
        assert!(err.contains("carries one branch"), "{err}");
        let err = format!("{:#}", deliver(&r, &Header { repo: None, ..cache_only(&[("refs/heads/main", &oid)]) }, b"").unwrap_err());
        assert!(err.contains("names its repo"), "{err}");
        assert!(!fleet.join("roots").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_wip_snapshot_lands_in_a_new_checkout_only() {
        let dir = scratch("recv-wip");
        let fleet = dir.join("fleet");
        std::fs::create_dir_all(fleet.join("roots/0123456789abcdef")).unwrap();
        let r = Arc::new(receivers(&fleet, Receivers::new(|_| {}), Arc::new(|_| true)));
        // The sender's checkout: a commit, then uncommitted work.
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        git(&src, &["init", "-q", "-b", "main"]);
        std::fs::write(src.join("f"), "one\n").unwrap();
        git(&src, &["add", "f"]);
        git(&src, &["commit", "-q", "-m", "one"]);
        std::fs::write(src.join("f"), "changed\n").unwrap();
        std::fs::write(src.join("u"), "untracked\n").unwrap();
        let id = repo::identity(Repo::Checkout(&src), "HEAD").unwrap();
        let wip_ref = repo::wip_ref("main");
        let send = |name: &str, haves: &[String]| {
            let snap = wip::snapshot(&src, &wip_ref).unwrap();
            let file = dir.join(name);
            let body = if repo::bundle_refs(Repo::Checkout(&src), &["refs/heads/main", &wip_ref], haves, &file).unwrap() {
                std::fs::read(&file).unwrap()
            } else {
                Vec::new()
            };
            let h = bundle_header(&body, "0123456789abcdef/a", &id, &[("refs/heads/main", &snap.base), (&wip_ref, &snap.wip)]);
            deliver(&r, &h, &body).unwrap();
            snap
        };
        let first = send("1.bundle", &[]);
        let at = fleet.join("roots/0123456789abcdef/a");
        assert_eq!(git(&at, &["rev-parse", "HEAD"]), first.base);
        assert_eq!(git(&at, &["symbolic-ref", "--short", "HEAD"]), "main");
        assert_eq!(std::fs::read_to_string(at.join("f")).unwrap(), "changed\n");
        assert_eq!(git(&at, &["status", "--porcelain=v1", "-uall"]), "M f\n?? u");
        assert_eq!(git(&at, &["for-each-ref", "refs/devsandbox"]), "", "the WIP ref isn't kept in the checkout");
        let cache = cache_path(&fleet, &id);
        assert_eq!(git(&cache, &["rev-parse", &wip_ref]), first.wip);

        // Again, more work: the cache gets the new snapshot, the existing
        // checkout (in use, dirty) is left alone, and the transfer is ok.
        std::fs::write(src.join("f"), "changed again\n").unwrap();
        let tips = repo::tips(Repo::Bare(&cache)).unwrap();
        let second = send("2.bundle", &tips);
        assert_ne!(second.wip, first.wip);
        assert_eq!(git(&cache, &["rev-parse", &wip_ref]), second.wip);
        assert_eq!(std::fs::read_to_string(at.join("f")).unwrap(), "changed\n");

        // A ref naming a commit neither the body nor the cache has.
        let missing = "1".repeat(40);
        let h = bundle_header(b"", "0123456789abcdef/a", &id, &[("refs/heads/main", &missing)]);
        let err = format!("{:#}", deliver(&r, &h, b"").unwrap_err());
        assert!(err.contains("is neither in the bundle nor already here"), "{err}");
        assert_eq!(git(&cache, &["rev-parse", "refs/heads/main"]), second.base, "no ref moved");
        // The cache is never reached through a symlink.
        let other = dir.join("elsewhere");
        std::fs::create_dir_all(&other).unwrap();
        std::os::unix::fs::symlink(&other, cache_path(&fleet, "fedcba9876543210")).unwrap();
        let h = bundle_header(b"", "0123456789abcdef/b", "fedcba9876543210", &[("refs/heads/main", &second.base)]);
        let err = format!("{:#}", deliver(&r, &h, b"").unwrap_err());
        assert!(err.contains("isn't a directory"), "{err}");
        assert_eq!(std::fs::read_dir(&other).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `fixture`'s `web` with `.gitignore` (`*.log`) committed on `main`, a
    /// placement of it (WIP-1: `README` changed, `U` untracked), then a
    /// commit of `README` and WIP-2 (`README` changed again, no `U`) at the
    /// source, and `x.log` made in the worker's checkout; `in_use` the
    /// receivers'. The checkout, its first `HEAD`, the second placement's
    /// result, the repo cache and `web`.
    fn place_twice(tag: &str, in_use: bool) -> (PathBuf, PathBuf, String, Result<Prepared, Unprepared>, PathBuf, PathBuf) {
        let dir = scratch(tag);
        let cfg = fixture(&dir);
        let web = dir.join("web");
        std::fs::write(web.join(".gitignore"), "*.log\n").unwrap();
        git(&web, &["add", ".gitignore"]);
        git(&web, &["commit", "-q", "-m", "ignore"]);
        let fleet = dir.join("worker/fleet/hub");
        let t = LinkTransfer::for_worker(link_with(&dir, &fleet, Arc::new(move |_| in_use)).parent().unwrap(), "w");
        std::fs::write(web.join("README"), "wip 1\n").unwrap();
        std::fs::write(web.join("U"), "u\n").unwrap();
        let got = prepare(&t, &fleet, &cfg, "web", None).unwrap();
        let checkout = got.folders[0].1.clone();
        assert_eq!(std::fs::read_to_string(checkout.join("README")).unwrap(), "wip 1\n");
        let first = git(&checkout, &["rev-parse", "HEAD"]);
        std::fs::write(checkout.join("x.log"), "built\n").unwrap();
        std::fs::remove_file(web.join("U")).unwrap();
        std::fs::write(web.join("README"), "two\n").unwrap();
        git(&web, &["commit", "-q", "-am", "two"]);
        std::fs::write(web.join("README"), "wip 2\n").unwrap();
        let again = prepare(&t, &fleet, &cfg, "web", None);
        let cache = cache_path(&fleet, &repo::identity(Repo::Checkout(&web), "HEAD").unwrap());
        (dir, checkout, first, again, cache, web)
    }

    #[test]
    fn a_checkout_no_instance_mounts_is_recut_to_what_was_sent() {
        let (dir, checkout, _, again, _, web) = place_twice("recut", false);
        again.unwrap();
        assert_eq!(git(&checkout, &["rev-parse", "HEAD"]), git(&web, &["rev-parse", "main"]));
        assert_eq!(git(&checkout, &["symbolic-ref", "--short", "HEAD"]), "main");
        assert_eq!(std::fs::read_to_string(checkout.join("README")).unwrap(), "wip 2\n");
        assert!(!checkout.join("U").exists(), "WIP-1's untracked file is gone");
        assert_eq!(std::fs::read_to_string(checkout.join("x.log")).unwrap(), "built\n", "ignored files stay");
        assert_eq!(git(&checkout, &["status", "--porcelain=v1", "-uall"]), "M README");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_checkout_an_instance_mounts_keeps_its_tree_unless_it_can_fast_forward() {
        let (dir, checkout, first, again, cache, web) = place_twice("in-use", true);
        // README changed both in the WIP there and in the new commit: the
        // fast-forward would clash, so the branch stays; still ok.
        again.unwrap();
        assert_eq!(git(&checkout, &["rev-parse", "HEAD"]), first);
        assert_eq!(std::fs::read_to_string(checkout.join("README")).unwrap(), "wip 1\n");
        assert!(checkout.join("U").exists());
        assert_eq!(git(&cache, &["rev-parse", "refs/heads/main"]), git(&web, &["rev-parse", "main"]), "the new tip is in the cache");

        // Clean there, a descendant sent: fast-forwarded.
        git(&checkout, &["checkout", "-q", "--", "README"]);
        std::fs::remove_file(checkout.join("U")).unwrap();
        std::fs::write(web.join("README"), "three\n").unwrap();
        git(&web, &["commit", "-q", "-am", "three"]);
        let cfg = dir.join("cfg");
        let fleet = dir.join("worker/fleet/hub");
        let t = LinkTransfer::for_worker(&dir.join("hub"), "w");
        prepare(&t, &fleet, &cfg, "web", None).unwrap();
        assert_eq!(git(&checkout, &["rev-parse", "HEAD"]), git(&web, &["rev-parse", "main"]));
        assert_eq!(std::fs::read_to_string(checkout.join("README")).unwrap(), "three\n");
        assert_eq!(git(&checkout, &["status", "--porcelain=v1", "-uall"]), "", "no WIP applied");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_checkout_is_in_use_when_an_instance_mounts_it_directly() {
        let dir = scratch("mounted");
        let (at, other) = (dir.join("roots/h/web"), dir.join("roots/h/lib"));
        std::fs::create_dir_all(&at).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let state = |body: &str| -> crate::state::State {
            toml::from_str(&format!("[instance.a]\nsandbox = \"web\"\ncontainer = \"c\"\nworkspace = \"/w\"\ncreated_unix = 0\n{body}")).unwrap()
        };
        let q = |p: &Path| p.display().to_string();
        assert!(!mounted(&crate::state::State::default(), &at));
        assert!(mounted(&state(&format!("folder = \"{}\"\n", q(&at))), &at));
        // Through a symlink: compared canonicalized.
        std::os::unix::fs::symlink(&at, dir.join("link")).unwrap();
        assert!(mounted(&state(&format!("folder = \"{}\"\n", q(&dir.join("link")))), &at));
        assert!(!mounted(&state(&format!("folder = \"{}\"\n", q(&other))), &at));
        // A worktree instance based on it runs elsewhere.
        let wt = format!("folder = \"{}\"\nbase_folder = \"{}\"\nworktree = \"{0}\"\n", q(&dir.join("wt")), q(&at));
        assert!(!mounted(&state(&wt), &at));
        // A `folders` entry: direct counts, a worktree of it doesn't.
        let entry = |worktree: &str| format!("folder = \"{}\"\n[[instance.a.folders]]\ntarget = \"/w/web\"\nbase = \"{}\"\n{worktree}", q(&other), q(&at));
        assert!(mounted(&state(&entry("")), &at));
        assert!(!mounted(&state(&entry(&format!("worktree = \"{}\"\n", q(&dir.join("wt2"))))), &at));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn haves_answer_the_fleet_roots_roots_and_caches() {
        let dir = scratch("haves");
        let fleet = dir.join("fleet");
        assert_eq!(haves_of(&fleet, &[]).unwrap(), Haves::default(), "no fleet root yet");
        for d in ["roots/0123456789abcdef", "roots/.in", "roots/.0123456789abcdef.1-2.part", "roots/not-a-hash"] {
            std::fs::create_dir_all(fleet.join(d)).unwrap();
        }
        std::fs::write(fleet.join("roots/fedcba9876543210"), "a file").unwrap();
        let cache = cache_path(&fleet, "00000000000000aa");
        std::fs::create_dir_all(&cache).unwrap();
        git(&cache, &["init", "-q", "--bare"]);
        let ids = ["00000000000000aa".to_string(), "00000000000000bb".to_string()];
        let got = haves_of(&fleet, &ids).unwrap();
        assert_eq!(got, Haves { roots: ["0123456789abcdef".to_string()].into(), tips: BTreeMap::new() }, "an empty cache has no tips");
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        git(&src, &["init", "-q", "-b", "main"]);
        git(&src, &["commit", "-q", "--allow-empty", "-m", "x"]);
        git(&src, &["push", "-q", cache.to_str().unwrap(), "main"]);
        let got = haves_call(&fleet, &json!({ "repos": ids })).unwrap();
        assert_eq!(got, json!({"roots": ["0123456789abcdef"], "tips": {"00000000000000aa": [git(&src, &["rev-parse", "HEAD"])]}}));
        crate::serve::dts::assert_matches("CodeHaves", &got);

        let code = |params: Value| haves_call(&fleet, &params).unwrap_err().code;
        assert_eq!(code(json!({})), "invalid");
        assert_eq!(code(json!({"repos": "x"})), "invalid");
        assert_eq!(code(json!({"repos": [1]})), "invalid");
        assert_eq!(code(json!({"repos": ["../../etc"]})), "invalid");
        assert_eq!(code(json!({"repos": vec!["0123456789abcdef"; HAVES_REPOS_MAX + 1]})), "invalid");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
