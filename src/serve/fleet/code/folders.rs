//! A signed config's folders on a member (docs/architecture/daemon/fleet.md,
//! *Config versions*; fleet-driver plan decision 3): a member's copy of a
//! signed config root (`mesh::tree_path`, `<data>/devsandbox/mesh/tree/<name>/`)
//! carries no code, so a relative `folder = "../x"` resolves to
//! `mesh/tree/x`, which the authority's run fills from its own checkout.
//!
//! - **ask** ([`folders_call`], `mesh.folders {root, sandbox}`): each
//!   `folder` / `folders` entry of a sandbox of an installed copy, its path
//!   relative to `mesh/tree/` and its state ([`classify`]): `missing`,
//!   `landed` (a checkout devsandbox landed: [`MARKER`] in its `.git/`),
//!   `user` (anything else there: never touched) or `unsupported`.
//! - **send** ([`send`], on the authority): one branch of the folder's
//!   checkout (and its WIP) as a [`FOLDER_KIND`] transfer, `to` relative to
//!   `mesh/tree/`, `--not` the member's tips of that repo (`code.haves`
//!   about the authority's fleet root there, where its cache is).
//! - **bring** ([`bring`], the authority's `run` aimed at a member's copy,
//!   from `daemon::verb`): ask, [`plan`] (pure: what to send, what to say),
//!   map each folder to this host's checkout under the root bound to the
//!   copy's name, send.
//! - **receive** ([`receivers`], on the member): only from the log's
//!   authority by its proven key; the objects into the authority's repo
//!   cache (`<data>/fleet/<authority>/repos/<id>.git`), then a missing
//!   folder cut fresh (WIP applied, the marker written, renamed into
//!   place) or a landed one's refs moved as `repo::land_from` does for a
//!   checkout in use (never recut); a `user` one is refused.
//! - **missing** ([`missing_in`]): a `run` of a copy whose folder is
//!   missing says where, and how to get it there.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::repo;
use super::transfer::{Header, Received, Receivers};
use super::{RepoOffer, Transfer};
use crate::config::Config;
use crate::serve::api::ApiError;
use crate::serve::fleet::identity::PublicKey;
use crate::serve::fleet::mesh;

/// The API call an authority asks a member's copy's folders with
/// (docs/reference/api.md, `mesh.folders`), answered by [`folders_call`].
pub const FOLDERS_METHOD: &str = "mesh.folders";
/// One branch of a signed copy's folder: a git bundle, landed at its
/// header's `to` below `mesh/tree/`.
pub const FOLDER_KIND: &str = "mesh-folder";
/// The file in a landed checkout's `.git/` (never in its working tree)
/// that marks it devsandbox's: the repo identity it was landed as, one
/// line. A checkout without it is the user's.
pub const MARKER: &str = "devsandbox-landed";

/// A folder's state on the member ([`classify`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FolderState {
    /// Nothing there: the authority lands it.
    Missing,
    /// A checkout devsandbox landed ([`MARKER`]): the authority sends the
    /// delta and its refs move.
    Landed,
    /// Something there devsandbox didn't land (a hand clone, a symlink, a
    /// file): never touched.
    User,
    /// Not a folder devsandbox lands: `why` says.
    Unsupported,
}

/// One entry of `mesh.folders`' answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Folder {
    /// The entry as the config writes it (`folder`, or a `folders` value).
    pub config: String,
    /// Where it resolves, relative to `mesh/tree/`, normalized; `null`
    /// when it resolves nowhere there (a variable, an absolute path, out
    /// of `mesh/tree/`).
    pub path: Option<String>,
    pub state: FolderState,
    /// A `landed` one's repo identity (its [`MARKER`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Why it's `unsupported`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

/// `mesh.folders`' answer: the sandbox's `folder` first, then its
/// `folders` values in key order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Folders {
    pub folders: Vec<Folder>,
}

/// What is at a folder's path under `mesh/tree/` ([`probe`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    Missing,
    /// A real dir whose real `.git/` holds [`MARKER`] naming this repo.
    Landed(String),
    User,
    /// Missing, but not landable there: a parent on the way is a symlink,
    /// a file, or a checkout.
    Blocked(String),
}

/// Where config entry `config` of copy `copy` resolves, relative to
/// `mesh/tree/` (pure): `copy/<config>` normalized lexically. Refused (as
/// why): a `${…}` variable, an absolute path, one out of `mesh/tree/` or
/// `mesh/tree/` itself, one in the copy (`.`) or in another installed copy
/// (`copies`, every installed name), a name a transfer's `to` can't carry.
pub fn target(copy: &str, config: &str, copies: &BTreeSet<String>) -> Result<String, String> {
    if config.contains("${") {
        return Err("it uses a variable".into());
    }
    if Path::new(config).is_absolute() {
        return Err("it is an absolute path: only a folder relative to the config is landed".into());
    }
    let mut parts: Vec<&str> = vec![copy];
    for c in Path::new(config).components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if parts.pop().is_none() {
                    return Err("it resolves outside `mesh/tree/`".into());
                }
            }
            Component::Normal(n) => parts.push(n.to_str().ok_or("it isn't UTF-8")?),
            Component::RootDir | Component::Prefix(_) => return Err("it is an absolute path".into()),
        }
    }
    let Some(first) = parts.first() else {
        return Err("it resolves to `mesh/tree/` itself".into());
    };
    if *first == copy {
        return Err(format!("it is inside the signed config's copy `{copy}`, which carries no `.git`"));
    }
    if copies.contains(*first) {
        return Err(format!("it is inside the copy of signed config `{first}`"));
    }
    let rel = parts.join("/");
    check_to(&rel, copies)?;
    Ok(rel)
}

/// A [`FOLDER_KIND`] transfer's `to` (pure): relative to `mesh/tree/`,
/// plain `/`-separated components (no `.`, `..` or empty one, no `\`,
/// NUL or newline, at most 1024 bytes, as `transfer::check_header`), its
/// first not an installed copy's name (`copies`).
pub fn check_to(to: &str, copies: &BTreeSet<String>) -> Result<(), String> {
    let plain = |c: &str| !c.is_empty() && c != "." && c != ".." && !c.contains(['\0', '\n', '\\']);
    if to.is_empty() || to.len() > 1024 || !to.split('/').all(plain) {
        return Err(format!("`{to}` isn't a plain path below `mesh/tree/`"));
    }
    let first = to.split('/').next().unwrap_or_default();
    if copies.contains(first) {
        return Err(format!("`{to}` is inside the copy of signed config `{first}`"));
    }
    Ok(())
}

/// Each of `entries` (config strings, `folder` first) of copy `copy`
/// classified (pure but for `probe`, asked once per resolved path):
/// [`target`]'s refusals and entries whose paths nest (one inside another,
/// not the same) are `unsupported`; the others by what `probe` finds.
pub fn classify(copy: &str, entries: &[String], copies: &BTreeSet<String>, probe: impl Fn(&str) -> Probe) -> Vec<Folder> {
    let targets: Vec<Result<String, String>> = entries.iter().map(|e| target(copy, e, copies)).collect();
    let mut out = Vec::with_capacity(entries.len());
    for (i, (config, t)) in entries.iter().zip(&targets).enumerate() {
        let unsupported = |path: Option<String>, why: String| Folder { config: config.clone(), path, state: FolderState::Unsupported, repo: None, why: Some(why) };
        let rel = match t {
            Err(why) => {
                out.push(unsupported(None, why.clone()));
                continue;
            }
            Ok(rel) => rel,
        };
        let nests = targets.iter().enumerate().find(|(j, o)| {
            *j != i && o.as_ref().is_ok_and(|o| o != rel && (Path::new(o).starts_with(rel) || Path::new(rel).starts_with(o)))
        });
        if let Some((j, _)) = nests {
            out.push(unsupported(Some(rel.clone()), format!("it nests with `{}`", entries[j])));
            continue;
        }
        let folder = |state, repo| Folder { config: config.clone(), path: Some(rel.clone()), state, repo, why: None };
        out.push(match probe(rel) {
            Probe::Missing => folder(FolderState::Missing, None),
            Probe::Landed(id) => folder(FolderState::Landed, Some(id)),
            Probe::User => folder(FolderState::User, None),
            Probe::Blocked(why) => unsupported(Some(rel.clone()), why),
        });
    }
    out
}

/// What is at `rel` (a [`check_to`] path) under `tree` (`mesh/tree/`): each
/// parent a real dir that isn't a checkout (else [`Probe::Blocked`] while
/// nothing resolves there, [`Probe::User`] once something does); then the
/// path itself: missing, a real dir with a real `.git/` holding a
/// [`MARKER`] with a repo identity ([`Probe::Landed`]), anything else the
/// user's.
pub fn probe(tree: &Path, rel: &str) -> Probe {
    let parts: Vec<&str> = rel.split('/').collect();
    let mut at = tree.to_path_buf();
    for (n, p) in parts[..parts.len() - 1].iter().enumerate() {
        at.push(p);
        let shown = parts[..=n].join("/");
        let blocked = match at.symlink_metadata() {
            Ok(m) if m.is_dir() && at.join(".git").symlink_metadata().is_ok() => Some(format!("it is inside the checkout `{shown}`")),
            Ok(m) if m.is_dir() => None,
            Ok(_) => Some(format!("`{shown}` isn't a directory (a symlink or a file)")),
            Err(_) => return Probe::Missing,
        };
        if let Some(why) = blocked {
            return if tree.join(rel).exists() { Probe::User } else { Probe::Blocked(why) };
        }
    }
    let path = tree.join(rel);
    match path.symlink_metadata() {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Probe::Missing,
        Ok(m) if m.is_dir() => landed_id(&path).map_or(Probe::User, Probe::Landed),
        _ => Probe::User,
    }
}

/// The repo identity a checkout's [`MARKER`] names: `.git` a real dir, the
/// marker a regular file holding one identity.
fn landed_id(checkout: &Path) -> Option<String> {
    let git = checkout.join(".git");
    if !git.symlink_metadata().is_ok_and(|m| m.is_dir()) {
        return None;
    }
    let marker = git.join(MARKER);
    if !marker.symlink_metadata().is_ok_and(|m| m.is_file()) {
        return None;
    }
    let id = std::fs::read_to_string(&marker).ok()?.trim().to_string();
    repo::check_id(&id).ok().map(|()| id)
}

/// `mesh.folders {root, sandbox}` against data dir `data`
/// (`<data>/devsandbox/`): `root` exactly an installed copy
/// ([`mesh::copy_of`], its name in `installed.toml`), else `denied`; its
/// config loaded there and `sandbox` resolved (`failed`, `not-found`);
/// each entry [`classify`]d against `mesh/tree/` ([`probe`]).
pub fn folders_call(data: &Path, params: &Value) -> Result<Value, ApiError> {
    let text = |key: &str| {
        params.get(key).and_then(Value::as_str).ok_or_else(|| ApiError::invalid(format!("`{FOLDERS_METHOD}` needs `{key}`, a string")))
    };
    let (root, sandbox) = (Path::new(text("root")?), text("sandbox")?);
    if !root.is_absolute() {
        return Err(ApiError::invalid(format!("`root` {} must be absolute", root.display())));
    }
    let copies = mesh::copy_names(data).map_err(ApiError::internal)?;
    let name = match mesh::copy_of(data, root) {
        Some((name, true)) if copies.contains(&name) => name,
        _ => return Err(ApiError::denied(format!("{} isn't a signed config's copy here (`mesh/tree/<name>`)", root.display()))),
    };
    let config = Config::load(&mesh::tree_path(data, &name)).map_err(|e| ApiError::failed(format!("config root `{name}`: {e:#}")))?;
    let resolved = config.resolve_sandbox(sandbox).map_err(|e| ApiError::not_found(format!("{e:#}")))?;
    let props = &resolved.properties;
    let entries: Vec<String> =
        props.folder.iter().cloned().chain(props.folders.iter().flat_map(|f| f.values()).map(|e| e.path().to_string())).collect();
    let tree = data.join("mesh").join("tree");
    let folders = classify(&name, &entries, &copies, |rel| probe(&tree, rel));
    serde_json::to_value(Folders { folders }).map_err(|e| ApiError::internal(e.into()))
}

/// Send repo `r` (its `at` unused) to a member's `mesh/tree/<to>` through
/// `t` (its link) as a [`FOLDER_KIND`] transfer, bundled `--not` `tips`
/// (the member's of that repo, `code.haves`): `code::send_repo`'s bundle
/// and header, this kind and `to`. The refs sent.
pub fn send(r: &RepoOffer, to: &str, tips: &[String], t: &dyn Transfer) -> Result<BTreeMap<String, String>> {
    check_to(to, &BTreeSet::new()).map_err(|e| anyhow!(e))?;
    super::send_repo_as(r, FOLDER_KIND, to, tips, t)
}

/// One folder [`bring`] sends ([`plan`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wanted {
    /// The entry as the config writes it.
    pub config: String,
    /// Where it lands, relative to `mesh/tree/` on the member.
    pub to: String,
    /// This host's checkout: the bound root joined with `config`, not
    /// resolved yet.
    pub host: PathBuf,
    /// A `landed` one's repo identity there: this checkout's must be it.
    pub landed: Option<String>,
    /// The `run --branch` asked, for the first entry (the sandbox's
    /// `folder`) only, as `code::offer`.
    pub branch: Option<String>,
}

/// What [`bring`] does with a `mesh.folders` answer ([`plan`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub wanted: Vec<Wanted>,
    /// The run's `note:` lines for what isn't sent.
    pub notes: Vec<String>,
}

/// What the authority sends of `folders` (machine `machine`'s answer)
/// from config root `bound` (pure): each `missing` or `landed` path once
/// (the first entry naming it); a `user` one left alone and an
/// `unsupported` one not sent, each with a note (the member's run says
/// so itself if that folder is missing there). `branch` goes with the
/// first entry.
pub fn plan(folders: &[Folder], bound: &Path, machine: &str, branch: Option<&str>) -> Plan {
    let mut out = Plan::default();
    for (i, f) in folders.iter().enumerate() {
        let config = &f.config;
        match (f.state, &f.path) {
            (FolderState::Missing | FolderState::Landed, Some(to)) => {
                if out.wanted.iter().any(|w| &w.to == to) {
                    continue;
                }
                let landed = if f.state == FolderState::Landed { f.repo.clone() } else { None };
                let branch = if i == 0 { branch.map(str::to_string) } else { None };
                out.wanted.push(Wanted { config: config.clone(), to: to.clone(), host: bound.join(config), landed, branch });
            }
            (FolderState::User, _) => {
                out.notes.push(format!("note: `{config}` on {machine} is a checkout devsandbox didn't land: left as it is"))
            }
            (FolderState::Unsupported, _) => {
                let why = f.why.as_deref().unwrap_or("unsupported");
                out.notes.push(format!("note: `{config}` isn't sent to {machine}: {why}"));
            }
            (_, None) => out.notes.push(format!("note: `{config}` isn't sent to {machine}: it resolves nowhere below `mesh/tree/` there")),
        }
    }
    out
}

/// What [`bring`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Brought {
    /// The machine answered `mesh.folders` with an error (`root` isn't a
    /// copy there, no such sandbox, a config that doesn't load there):
    /// nothing to bring, the run says the rest itself. Why, for the log.
    Nothing(String),
    /// The `note:` lines for the run's output: what was sent, what wasn't.
    Notes(Vec<String>),
}

/// The authority's `run` of sandbox `sandbox` of machine `machine`'s copy
/// at `remote_root`, whose name is bound here to config root `bound`
/// (fleet-driver decision 3): `mesh.folders` asked there over `t`, then
/// [`plan`], then [`send_planned`]. `Err` is the run's: the machine can't
/// be asked, or a folder can't be sent.
pub fn bring(t: &super::LinkTransfer, remote_root: &Path, sandbox: &str, bound: &Path, branch: Option<&str>) -> Result<Brought, String> {
    let machine = t.worker.as_str();
    let answer = t
        .folders(remote_root, sandbox)
        .map_err(|e| format!("cannot ask {machine} which folders of `{sandbox}` it has: {e:#}"))?;
    let folders = match answer {
        Ok(f) => f,
        Err((code, message)) => return Ok(Brought::Nothing(format!("{FOLDERS_METHOD}: {code}: {message}"))),
    };
    let plan = plan(&folders.folders, bound, machine, branch);
    let mut notes = plan.notes.clone();
    notes.extend(send_planned(&plan.wanted, machine, t)?);
    Ok(Brought::Notes(notes))
}

/// Each of `wanted` sent to `machine` through `t`: its checkout resolved
/// (canonical, a checkout's root: `code::repo_offer`, its branch or
/// `--branch`, its WIP), a `landed` one's identity checked against it, one
/// `code.haves` for every repo, then each [`send`] `--not` the member's
/// tips. The notes saying what went; the first failure, naming its
/// folder and the machine, stops it.
pub fn send_planned(wanted: &[Wanted], machine: &str, t: &dyn Transfer) -> Result<Vec<String>, String> {
    let fail = |w: &Wanted, why: String| format!("cannot bring `{}` to {machine}: {why}", w.config);
    let mut offers = Vec::with_capacity(wanted.len());
    for w in wanted {
        let host = std::fs::canonicalize(&w.host).map_err(|e| fail(w, format!("{} doesn't resolve here: {e}", w.host.display())))?;
        let offer = super::repo_offer(&host, Path::new(""), w.branch.as_deref()).map_err(|e| {
            fail(
                w,
                match e {
                    super::Unprepared::NotMovable(why) => why,
                    super::Unprepared::Failed(e) => format!("{e:#}"),
                },
            )
        })?;
        if let Some(there) = w.landed.as_deref().filter(|there| *there != offer.id) {
            return Err(fail(
                w,
                format!("mesh/tree/{} there was landed as repo {there}, and {} here is repo {}", w.to, host.display(), offer.id),
            ));
        }
        offers.push(offer);
    }
    if offers.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<String> = offers.iter().map(|o| o.id.clone()).collect::<BTreeSet<_>>().into_iter().collect();
    let haves = t.haves(&ids).map_err(|e| format!("cannot ask {machine} which commits it has: {e:#}"))?;
    let mut notes = Vec::new();
    for (w, o) in wanted.iter().zip(&offers) {
        let tips = haves.tips.get(&o.id).map(Vec::as_slice).unwrap_or_default();
        send(o, &w.to, tips, t).map_err(|e| fail(w, format!("{e:#}")))?;
        notes.push(format!("note: sent `{}` ({}) to {machine}", w.config, o.branch));
    }
    Ok(notes)
}

/// Whether peer `peer` (key `key`, the one its link proved) is the
/// authority of the membership log next to data dir `data`'s
/// `member.toml`, read now: its name and key both the log's authority's.
fn check_authority(data: &Path, peer: &str, key: Option<&PublicKey>) -> Result<()> {
    let stored = crate::serve::fleet::membership_api::load(&data.join("member.toml")).map_err(|e| anyhow!("{}", e.message))?;
    let membership = stored.as_ref().and_then(|s| s.log.membership());
    match (membership, key) {
        (Some(m), Some(key)) if m.authority.0 == peer && m.is_authority(key) => Ok(()),
        (Some(m), _) => bail!("a signed config's folders come from the fleet's authority `{}`, not `{peer}`", m.authority.0),
        (None, _) => bail!("this host is in no fleet: no signed config's folders land here"),
    }
}

/// `mesh/tree/` under data dir `data`, `mesh` and it real dirs (a copy
/// installed made them).
fn tree_of(data: &Path) -> Result<PathBuf> {
    let mesh = data.join("mesh");
    let tree = mesh.join("tree");
    for d in [&mesh, &tree] {
        if !d.symlink_metadata().is_ok_and(|m| m.is_dir()) {
            bail!("no signed config is installed here ({} isn't a directory)", d.display());
        }
    }
    Ok(tree)
}

/// Where `h` lands, checked against what's there now: the `to` path
/// ([`check_to`]) and what [`probe`] finds, `missing` or `landed` as this
/// repo; a `user` path, another repo's or a blocked one is refused.
fn check_target(tree: &Path, copies: &BTreeSet<String>, h: &Header, id: &str) -> Result<(String, Probe)> {
    let to = h.to.as_deref().context("a mesh folder names where it goes (`to`, below `mesh/tree/`)")?;
    check_to(to, copies).map_err(|e| anyhow!(e))?;
    match probe(tree, to) {
        p @ Probe::Missing => Ok((to.to_string(), p)),
        Probe::Landed(have) if have == id => Ok((to.to_string(), Probe::Landed(have))),
        Probe::Landed(have) => bail!("mesh/tree/{to} is a checkout devsandbox landed as repo {have}, not {id}: left as it is"),
        Probe::User => bail!("mesh/tree/{to} exists and devsandbox didn't land it (no .git/{MARKER}): a checkout of the user's is never touched"),
        Probe::Blocked(why) => bail!("mesh/tree/{to}: {why}"),
    }
}

/// The member's [`FOLDER_KIND`] receiver, added to `receivers`, for peer
/// `peer` (key `key`, the one its link proved) whose fleet root here is
/// `fleet_root`, under data dir `data` (`<data>/devsandbox/`):
///
/// - taken only while `peer` is the log's authority ([`check_authority`],
///   read per transfer), with `repo` and `refs` as a `bundle`'s
///   (`code::bundle_refs_of`: one branch, at most its WIP ref) and `to`
///   landable ([`check_target`]: under `mesh/tree/`, outside every
///   installed copy, each parent a real dir and no checkout, the path
///   missing or landed as this repo), checked before the body comes and
///   again once it's in;
/// - the objects into the authority's repo cache (`code::make_cache`:
///   `<fleet root>/repos/<repo>.git`, so its `code.haves` stays the
///   delta's base), its refs set (`repo::set_refs`);
/// - a missing path: its parents made (real dirs), then a checkout cut
///   fresh from the cache next to it (`.<name>.landing-<nonce>`,
///   `repo::land_from`: the branch checked out, the WIP applied), the
///   [`MARKER`] written, renamed into place (removed on any error);
/// - a landed one: `repo::land_from` as for a checkout in use, never a
///   recut: another branch's ref moved, the checked-out one fast-forwarded
///   on a clean tree only, else kept (logged); the WIP stays in the cache.
pub fn receivers(data: &Path, fleet_root: &Path, peer: &str, key: Option<PublicKey>, receivers: Receivers) -> Receivers {
    let log = receivers.log();
    let in_dir = fleet_root.join("repos").join(".in");
    let (data_in, peer_in) = (data.to_path_buf(), peer.to_string());
    let (data, fleet_root) = (data.to_path_buf(), fleet_root.to_path_buf());
    receivers
        .with(
            FOLDER_KIND,
            super::BUNDLE_LIMIT,
            move |h| {
                check_authority(&data_in, &peer_in, key.as_ref())?;
                let (id, _, _) = super::bundle_refs_of(h)?;
                repo::check_id(id)?;
                let copies = mesh::copy_names(&data_in)?;
                check_target(&tree_of(&data_in)?, &copies, h, id)?;
                // Unique: two empty bodies share their sha256.
                Ok(in_dir.join(format!("{}-{}.bundle", h.sha256, repo::nonce())))
            },
            |_| {},
        )
        .then(FOLDER_KIND, move |r: &Received| {
            let landed = (|| {
                let (id, branch, wip) = super::bundle_refs_of(&r.header)?;
                let refs = r.header.refs.as_ref().context("a bundle names its refs")?;
                let tree = tree_of(&data)?;
                let copies = mesh::copy_names(&data)?;
                check_target(&tree, &copies, &r.header, id)?;
                let cache = super::make_cache(&fleet_root, id)?;
                if r.header.size > 0 {
                    repo::unbundle(&cache, &r.path)?;
                }
                repo::set_refs(&cache, refs)?;
                // Again, right before landing: the path may have changed meanwhile.
                match check_target(&tree, &copies, &r.header, id)? {
                    (to, Probe::Missing) => land_new(&cache, &tree, &to, branch, wip, id),
                    (to, _) => {
                        let at = tree.join(&to);
                        if repo::land_from(&cache, &at, branch, wip, true)? == repo::Landed::Kept {
                            log(&format!(
                                "fleet: {} is dirty or not behind the sent tip: {branch} stays where it is there, the sent tip is in the repo cache",
                                at.display()
                            ));
                        }
                        Ok(())
                    }
                }
            })();
            let _ = std::fs::remove_file(&r.path);
            landed
        })
}

/// A fresh checkout of `branch` (with `wip`) of repo `id` from `cache` at
/// `tree/<to>`: parents made (real dirs, [`super::real_dir`]), cut in a
/// temp dir next to it, marked, renamed into place.
fn land_new(cache: &Path, tree: &Path, to: &str, branch: &str, wip: Option<&str>, id: &str) -> Result<()> {
    let parts: Vec<&str> = to.split('/').collect();
    let (leaf, parents) = parts.split_last().context("a path has a component")?;
    let mut dir = tree.to_path_buf();
    for p in parents {
        dir.push(p);
        super::real_dir(&dir)?;
    }
    let at = dir.join(leaf);
    let temp = dir.join(format!(".{leaf}.landing-{}", repo::nonce()));
    let cut = (|| {
        repo::land_from(cache, &temp, branch, wip, false)?;
        let marker = temp.join(".git").join(MARKER);
        std::fs::write(&marker, format!("{id}\n")).with_context(|| format!("cannot write {}", marker.display()))?;
        std::fs::rename(&temp, &at).with_context(|| format!("cannot move the checkout to {}", at.display()))
    })();
    if cut.is_err() {
        let _ = std::fs::remove_dir_all(&temp);
    }
    cut
}

/// `path` with `.` and `..` resolved lexically (it may not exist).
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

/// A missing folder's error in a signed copy (pure): `what` (`sandbox
/// folder`, `` `folders` entry ``) `config` at `abs`, that the signed
/// config `copy` carries no code, and how to get it: a run from the
/// fleet's `authority`, which lands it from its checkout, or a clone.
pub fn missing_text(what: &str, config: &str, abs: &Path, copy: &str, authority: Option<&str>) -> String {
    let from = match authority {
        Some(a) => format!("the fleet's authority `{a}`"),
        None => "the fleet's authority".to_string(),
    };
    format!(
        "{what} `{config}` does not exist here ({}): the signed config `{copy}` carries no code; \
         run it from {from} (which lands it from its checkout), or clone it there",
        abs.display()
    )
}

/// [`missing_text`] when config dir `dir` is a signed config's copy under
/// data dir `data` ([`mesh::copy_of`], the copy itself), the authority
/// read from the log next to its `member.toml`; `None` for any other dir.
pub fn missing_in(data: &Path, dir: &Path, what: &str, config: &str) -> Option<String> {
    let (copy, true) = mesh::copy_of(data, dir)? else { return None };
    let base = std::fs::canonicalize(dir).ok()?;
    let stored = crate::serve::fleet::membership_api::load(&data.join("member.toml")).ok().flatten();
    let authority = stored.as_ref().and_then(|s| s.log.membership()).map(|m| m.authority.0.clone());
    Some(missing_text(what, config, &normalize(&base.join(config)), &copy, authority.as_deref()))
}

/// [`missing_in`] over the process env's data dir (as `mesh::check_root`).
pub fn missing_here(dir: &Path, what: &str, config: &str) -> Option<String> {
    let data = crate::daemon_config::data_dir_from(std::env::var_os("XDG_DATA_HOME"), std::env::var_os("HOME")).ok()?;
    missing_in(&data, dir, what, config)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::super::repo::tests::{git, repo_with_branches, scratch};
    use super::super::{haves_of, transfer};
    use super::*;
    use crate::serve::fleet::identity::{SigningKey, Signer};
    use crate::serve::fleet::membership_api::test_support;

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_folder_resolves_below_mesh_tree_outside_every_copy() {
        let copies = set(&["cfg", "other"]);
        let t = |c: &str| target("cfg", c, &copies);
        assert_eq!(t("../stackblitz/bolt-aurora").unwrap(), "stackblitz/bolt-aurora");
        assert_eq!(t("../a/./b/../c").unwrap(), "a/c");
        assert_eq!(t("./../web").unwrap(), "web");
        for (c, why) in [
            (".", "inside the signed config's copy `cfg`"),
            ("sub/x", "inside the signed config's copy `cfg`"),
            ("../other/x", "inside the copy of signed config `other`"),
            ("../other", "inside the copy of signed config `other`"),
            ("..", "`mesh/tree/` itself"),
            ("../../x", "outside `mesh/tree/`"),
            ("/abs/x", "absolute path"),
            ("../${localEnv:X}", "uses a variable"),
            ("../a\\b", "isn't a plain path"),
        ] {
            let got = t(c).unwrap_err();
            assert!(got.contains(why), "{c}: {got}");
        }
        assert!(check_to("a/b", &copies).is_ok());
        for bad in ["", "a//b", "a/../b", "./a", "/a", "other/x", "a/\n"] {
            assert!(check_to(bad, &copies).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn classify_maps_each_entry_to_its_state() {
        let copies = set(&["cfg"]);
        let probes: BTreeMap<&str, Probe> = [
            ("web", Probe::Missing),
            ("lib", Probe::Landed("0123456789abcdef".into())),
            ("mine", Probe::User),
            ("x/y", Probe::Blocked("`x` isn't a directory (a symlink or a file)".into())),
        ]
        .into();
        let entries: Vec<String> = ["../web", "../lib", "../mine", "../x/y", "../a", "../a/b", "../web", "."].map(String::from).to_vec();
        let got = classify("cfg", &entries, &copies, |rel| probes.get(rel).cloned().unwrap_or(Probe::Missing));
        let states: Vec<(FolderState, Option<&str>)> = got.iter().map(|f| (f.state, f.path.as_deref())).collect();
        use FolderState::*;
        assert_eq!(
            states,
            [
                (Missing, Some("web")),
                (Landed, Some("lib")),
                (User, Some("mine")),
                (Unsupported, Some("x/y")),
                (Unsupported, Some("a")),
                (Unsupported, Some("a/b")),
                // The same path twice doesn't nest.
                (Missing, Some("web")),
                (Unsupported, None),
            ]
        );
        assert_eq!(got[1].repo.as_deref(), Some("0123456789abcdef"));
        assert_eq!(got[4].why.as_deref(), Some("it nests with `../a/b`"));
        assert_eq!(got[5].why.as_deref(), Some("it nests with `../a`"));
        assert!(got[3].why.as_deref().unwrap().contains("isn't a directory"));
        assert!(got.iter().all(|f| (f.state == Unsupported) == f.why.is_some()));
    }

    #[test]
    fn probe_tells_landed_from_the_users() {
        let dir = scratch("folders-probe");
        let tree = dir.join("tree");
        std::fs::create_dir_all(tree.join("plain")).unwrap();
        assert_eq!(probe(&tree, "gone"), Probe::Missing);
        assert_eq!(probe(&tree, "gone/deeper"), Probe::Missing);
        assert_eq!(probe(&tree, "plain"), Probe::User);
        assert_eq!(probe(&tree, "plain/new"), Probe::Missing);
        let user = repo_with_branches(&tree, "user");
        assert_eq!(probe(&tree, "user"), Probe::User);
        assert!(matches!(probe(&tree, "user/sub"), Probe::Blocked(w) if w.contains("inside the checkout `user`")));
        // Something there inside a checkout is the user's.
        assert_eq!(probe(&tree, "user/README"), Probe::User);
        std::fs::write(user.join(".git").join(MARKER), "0123456789abcdef\n").unwrap();
        assert_eq!(probe(&tree, "user"), Probe::Landed("0123456789abcdef".into()));
        std::fs::write(user.join(".git").join(MARKER), "garbage\n").unwrap();
        assert_eq!(probe(&tree, "user"), Probe::User);
        // Symlinks: one at the path is the user's, one on the way blocks.
        std::os::unix::fs::symlink(&user, tree.join("link")).unwrap();
        assert_eq!(probe(&tree, "link"), Probe::User);
        std::os::unix::fs::symlink(dir.join("elsewhere"), tree.join("via")).unwrap();
        assert!(matches!(probe(&tree, "via/x"), Probe::Blocked(w) if w.contains("`via` isn't a directory")));
        std::fs::create_dir_all(dir.join("elsewhere/x")).unwrap();
        assert_eq!(probe(&tree, "via/x"), Probe::User);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `<dir>/data` with copies `cfg` (its config `toml`) and `other`
    /// installed (as `installed.toml` lists them).
    fn data_with_copies(dir: &Path, toml: &str) -> PathBuf {
        let data = dir.join("data");
        for name in ["cfg", "other"] {
            std::fs::create_dir_all(mesh::tree_path(&data, name)).unwrap();
        }
        std::fs::write(mesh::tree_path(&data, "cfg").join("devsandboxes.toml"), toml).unwrap();
        let entry = "hash = \"00\"\nconfig = \"devsandboxes.toml\"\nfiles = []\ndirs = []\n";
        std::fs::write(data.join("mesh/installed.toml"), format!("[cfg]\n{entry}[other]\n{entry}")).unwrap();
        data
    }

    #[test]
    fn mesh_folders_answers_an_installed_copy_only() {
        let dir = scratch("folders-call");
        let data = data_with_copies(
            &dir,
            "[sandbox.web]\nimage = \"i\"\nfolder = \"../web\"\nfolders = { \"/w/lib\" = \"../lib\", \"/w/cfg\" = \".\" }\n",
        );
        std::fs::create_dir_all(data.join("mesh/tree/lib")).unwrap();
        let copy = mesh::tree_path(&data, "cfg");
        let got = folders_call(&data, &json!({"root": copy, "sandbox": "web"})).unwrap();
        crate::serve::dts::assert_matches("MeshFolders", &got);
        for f in got["folders"].as_array().unwrap() {
            crate::serve::dts::assert_matches("MeshFolder", f);
        }
        let got: Folders = serde_json::from_value(got).unwrap();
        let states: Vec<(&str, Option<&str>, FolderState)> = got.folders.iter().map(|f| (f.config.as_str(), f.path.as_deref(), f.state)).collect();
        assert_eq!(
            states,
            [("../web", Some("web"), FolderState::Missing), (".", None, FolderState::Unsupported), ("../lib", Some("lib"), FolderState::User)]
        );
        let code = |params: Value| folders_call(&data, &params).unwrap_err().code;
        assert_eq!(code(json!({"root": copy.join("sub"), "sandbox": "web"})), "denied");
        assert_eq!(code(json!({"root": data.join("mesh/tree"), "sandbox": "web"})), "denied");
        assert_eq!(code(json!({"root": dir, "sandbox": "web"})), "denied");
        std::fs::create_dir_all(data.join("mesh/tree/stray")).unwrap();
        assert_eq!(code(json!({"root": data.join("mesh/tree/stray"), "sandbox": "web"})), "denied", "not installed");
        assert_eq!(code(json!({"root": copy, "sandbox": "nope"})), "not-found");
        assert_eq!(code(json!({"root": "rel", "sandbox": "web"})), "invalid");
        assert_eq!(code(json!({"root": copy})), "invalid");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Sends straight into `Receivers` over a socket pair.
    struct Direct(Arc<Receivers>);

    impl Transfer for Direct {
        fn send(&self, header: &Header, body: &mut dyn std::io::Read) -> Result<()> {
            transfer::send(self.0.accept().unwrap(), header, body)
        }
    }

    /// The authority `auth` (seed 1) and member `m` (seed 2) in one log,
    /// written next to `data`'s `member.toml`.
    fn fleet(data: &Path) -> (PublicKey, PublicKey) {
        let (a, m) = (SigningKey::from_seed([1; 32]), SigningKey::from_seed([2; 32]));
        let stored = test_support::fleet("auth", &a, "auth", &[("m", m.public_key())]);
        test_support::write(&data.join("member.toml"), &stored);
        (a.public_key(), m.public_key())
    }

    /// A member's receivers for peer `peer` with `key`.
    fn member(data: &Path, peer: &str, key: PublicKey) -> (Direct, PathBuf) {
        let fleet_root = crate::serve::fleet::lent::peer_fleet_root(data, peer).unwrap();
        (Direct(Arc::new(receivers(data, &fleet_root, peer, Some(key), Receivers::new(|_| {})))), fleet_root)
    }

    fn offer(host: &Path, branch: &str, wip: bool) -> RepoOffer {
        let id = repo::identity(repo::Repo::Checkout(host), "HEAD").unwrap();
        RepoOffer { host: host.to_path_buf(), at: PathBuf::new(), branch: branch.into(), wip, id }
    }

    /// What landing must leave alone in an existing checkout: its index
    /// (hashed), status, `HEAD`, working-tree files.
    fn tree_state(repo: &Path) -> Vec<String> {
        // Status first: it refreshes the index's stat data itself.
        let status = git(repo, &["status", "--porcelain=v1", "-uall"]);
        vec![
            std::fs::read(repo.join(".git/index")).map(|b| super::super::short_hash(&b)).unwrap_or_default(),
            status,
            git(repo, &["symbolic-ref", "HEAD"]),
            std::fs::read_to_string(repo.join("README")).unwrap_or_default(),
        ]
    }

    #[test]
    fn the_authority_lands_a_missing_folder_then_moves_its_refs() {
        let dir = scratch("folders-land");
        let data = data_with_copies(&dir, "[sandbox.web]\nimage = \"i\"\nfolder = \"../stackblitz/web\"\n");
        let (auth, _) = fleet(&data);
        let src = repo_with_branches(&dir.join("src"), "web");
        std::fs::write(src.join("README"), "dirty\n").unwrap();
        let (t, fleet_root) = member(&data, "auth", auth);
        let o = offer(&src, "main", true);

        // Missing: cut fresh, on `main`, the WIP applied, marked.
        let refs = send(&o, "stackblitz/web", &[], &t).unwrap();
        let at = data.join("mesh/tree/stackblitz/web");
        assert_eq!(git(&at, &["symbolic-ref", "--short", "HEAD"]), "main");
        assert_eq!(git(&at, &["rev-parse", "HEAD"]), refs["refs/heads/main"]);
        assert_eq!(std::fs::read_to_string(at.join("README")).unwrap(), "dirty\n");
        assert_eq!(git(&at, &["status", "--porcelain=v1"]), "M README");
        assert_eq!(git(&at, &["remote"]), "");
        assert_eq!(std::fs::read_to_string(at.join(".git").join(MARKER)).unwrap(), format!("{}\n", o.id));
        assert_eq!(probe(&data.join("mesh/tree"), "stackblitz/web"), Probe::Landed(o.id.clone()));
        let cache = super::super::cache_path(&fleet_root, &o.id);
        assert_eq!(git(&cache, &["rev-parse", "refs/heads/main"]), refs["refs/heads/main"]);
        let leftovers: Vec<_> = std::fs::read_dir(data.join("mesh/tree/stackblitz")).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(leftovers, ["web"], "no landing temp dir left");

        // Landed: a delta (`--not` the cache's tips), the working tree and
        // index untouched (dirty: `main` kept), `feature`'s ref moved.
        git(&src, &["checkout", "-q", "--", "README"]);
        std::fs::write(src.join("N"), "n\n").unwrap();
        git(&src, &["add", "N"]);
        git(&src, &["commit", "-q", "-m", "n"]);
        let before = tree_state(&at);
        let tips = haves_of(&fleet_root, std::slice::from_ref(&o.id)).unwrap().tips.remove(&o.id).unwrap();
        let refs = send(&o, "stackblitz/web", &tips, &t).unwrap();
        assert_eq!(tree_state(&at), before);
        assert_eq!(git(&cache, &["rev-parse", "refs/heads/main"]), refs["refs/heads/main"]);
        assert!(!at.join("N").exists());
        let feature = offer(&src, "feature", false);
        let tips = haves_of(&fleet_root, std::slice::from_ref(&o.id)).unwrap().tips.remove(&o.id).unwrap();
        let sent = send(&feature, "stackblitz/web", &tips, &t).unwrap();
        assert_eq!(git(&at, &["rev-parse", "refs/heads/feature"]), sent["refs/heads/feature"]);
        assert_eq!(tree_state(&at), before);

        // Clean and behind: fast-forwarded, never recut.
        git(&at, &["checkout", "-q", "--", "README"]);
        let refs = send(&o, "stackblitz/web", &[], &t).unwrap();
        assert_eq!(git(&at, &["rev-parse", "HEAD"]), refs["refs/heads/main"]);
        assert_eq!(std::fs::read_to_string(at.join("N")).unwrap(), "n\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_mesh_folder_comes_from_the_authority_to_a_landable_path_only() {
        let dir = scratch("folders-refuse");
        let data = data_with_copies(&dir, "[sandbox.web]\nimage = \"i\"\nfolder = \"../web\"\n");
        let (auth, m) = fleet(&data);
        let src = repo_with_branches(&dir.join("src"), "web");
        let o = offer(&src, "main", false);
        let tree = data.join("mesh/tree");
        let refused = |t: &Direct, to: &str| format!("{:#}", send(&o, to, &[], t).unwrap_err());

        // Not the authority: another member, or the authority's name with another key.
        let err = refused(&member(&data, "m", m).0, "web");
        assert!(err.contains("come from the fleet's authority `auth`, not `m`"), "{err}");
        let err = refused(&member(&data, "auth", m).0, "web");
        assert!(err.contains("not `auth`"), "{err}");
        assert!(!tree.join("web").exists());

        let (t, _) = member(&data, "auth", auth);
        // `..`, inside a copy: refused before anything is sent.
        assert!(send(&o, "../web", &[], &t).is_err());
        let err = refused(&t, "cfg/web");
        assert!(err.contains("inside the copy of signed config `cfg`"), "{err}");
        assert!(!tree.join("cfg/web").exists());
        // A raw header with `..` never reaches the receiver.
        let (size, sha256) = transfer::digest(&b""[..]).unwrap();
        let h = Header {
            kind: FOLDER_KIND.into(),
            name: "repo.bundle".into(),
            size,
            sha256,
            to: Some("x/../../escape".into()),
            repo: Some(o.id.clone()),
            refs: Some([("refs/heads/main".to_string(), "a".repeat(40))].into()),
        };
        let err = format!("{:#}", t.send(&h, &mut &b""[..]).unwrap_err());
        assert!(err.contains("bad transfer path"), "{err}");

        // A user's checkout: untouched.
        let user = repo_with_branches(&tree, "mine");
        std::fs::write(user.join("README"), "mine\n").unwrap();
        let before = tree_state(&user);
        let err = refused(&t, "mine");
        assert!(err.contains("devsandbox didn't land it"), "{err}");
        assert_eq!(tree_state(&user), before);
        // Landed as another repo: untouched too.
        std::fs::write(user.join(".git").join(MARKER), "0123456789abcdef\n").unwrap();
        let err = refused(&t, "mine");
        assert!(err.contains("landed as repo 0123456789abcdef"), "{err}");
        assert_eq!(tree_state(&user), before);
        // Through a symlinked parent, or inside a checkout.
        std::fs::create_dir_all(dir.join("elsewhere")).unwrap();
        std::os::unix::fs::symlink(dir.join("elsewhere"), tree.join("via")).unwrap();
        let err = refused(&t, "via/web");
        assert!(err.contains("`via` isn't a directory"), "{err}");
        assert_eq!(std::fs::read_dir(dir.join("elsewhere")).unwrap().count(), 0);
        let err = refused(&t, "mine/sub");
        assert!(err.contains("inside the checkout `mine`"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn entry(config: &str, path: Option<&str>, state: FolderState) -> Folder {
        let repo = (state == FolderState::Landed).then(|| "0123456789abcdef".to_string());
        let why = (state == FolderState::Unsupported).then(|| "it uses a variable".to_string());
        Folder { config: config.into(), path: path.map(str::to_string), state, repo, why }
    }

    #[test]
    fn the_authority_sends_missing_and_landed_folders_once() {
        use FolderState::*;
        let folders = [
            entry("../web", Some("web"), Missing),
            entry("../lib", Some("lib"), Landed),
            entry("../mine", Some("mine"), User),
            entry("../${localEnv:X}", None, Unsupported),
            entry("./../web", Some("web"), Missing),
        ];
        let bound = Path::new("/h/cfg");
        let got = plan(&folders, bound, "juchu", Some("feature"));
        assert_eq!(
            got.wanted,
            [
                Wanted { config: "../web".into(), to: "web".into(), host: bound.join("../web"), landed: None, branch: Some("feature".into()) },
                Wanted {
                    config: "../lib".into(),
                    to: "lib".into(),
                    host: bound.join("../lib"),
                    landed: Some("0123456789abcdef".into()),
                    branch: None,
                },
            ]
        );
        assert_eq!(
            got.notes,
            [
                "note: `../mine` on juchu is a checkout devsandbox didn't land: left as it is",
                "note: `../${localEnv:X}` isn't sent to juchu: it uses a variable",
            ]
        );
        // `--branch` is the `folder`'s only, even when that one isn't sent.
        let got = plan(&[folders[2].clone(), folders[0].clone()], bound, "juchu", Some("feature"));
        assert_eq!(got.wanted.len(), 1);
        assert!(got.wanted.iter().all(|w| w.branch.is_none()));
        assert_eq!(plan(&[], bound, "juchu", None), Plan::default());
    }

    #[test]
    fn a_folder_the_authority_cant_send_stops_the_run() {
        let dir = scratch("folders-unsent");
        let data = data_with_copies(&dir, "[sandbox.web]\nimage = \"i\"\nfolder = \"../web\"\n");
        let (auth, _) = fleet(&data);
        let (t, _) = member(&data, "auth", auth);
        let bound = dir.join("src/cfg");
        std::fs::create_dir_all(&bound).unwrap();
        let src = repo_with_branches(&dir.join("src"), "web");
        let want = |config: &str, landed: Option<&str>| Wanted {
            config: config.into(),
            to: "web".into(),
            host: bound.join(config),
            landed: landed.map(str::to_string),
            branch: None,
        };
        let err = |w: Wanted| send_planned(&[w], "juchu", &t).unwrap_err();
        let e = err(want("../gone", None));
        assert!(e.starts_with("cannot bring `../gone` to juchu: ") && e.contains("doesn't resolve here"), "{e}");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        let e = err(want("../web/sub", None));
        assert!(e.contains("isn't a git checkout"), "a checkout's subdir: {e}");
        let e = err(want("../web", Some("0123456789abcdef")));
        assert!(e.contains("was landed as repo 0123456789abcdef") && e.contains("here is repo"), "{e}");
        let head = git(&src, &["rev-parse", "HEAD"]);
        git(&src, &["checkout", "-q", "--detach", &head]);
        let e = err(want("../web", None));
        assert!(e.contains("detached HEAD"), "{e}");
        assert!(!data.join("mesh/tree/web").exists(), "nothing sent");
        git(&src, &["checkout", "-q", "main"]);
        assert_eq!(send_planned(&[want("../web", None)], "juchu", &t).unwrap(), ["note: sent `../web` (main) to juchu"]);
        assert!(data.join("mesh/tree/web/.git").join(MARKER).is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_folder_in_a_copy_says_how_to_get_it() {
        let text = missing_text("sandbox folder", "../web", Path::new("/d/mesh/tree/web"), "cfg", Some("juchu"));
        assert_eq!(
            text,
            "sandbox folder `../web` does not exist here (/d/mesh/tree/web): the signed config `cfg` carries no code; \
             run it from the fleet's authority `juchu` (which lands it from its checkout), or clone it there"
        );
        assert!(missing_text("`folders` entry", "../x", Path::new("/x"), "cfg", None).contains("run it from the fleet's authority (which"));

        let dir = scratch("folders-missing");
        let data = data_with_copies(&dir, "[sandbox.web]\nimage = \"i\"\nfolder = \"../web\"\n");
        let copy = mesh::tree_path(&data, "cfg");
        let tree = std::fs::canonicalize(data.join("mesh/tree")).unwrap();
        let got = missing_in(&data, &copy, "sandbox folder", "../web").unwrap();
        assert!(got.contains(&format!("({})", tree.join("web").display())) && got.contains("authority (which"), "{got}");
        fleet(&data);
        assert!(missing_in(&data, &copy, "sandbox folder", "../web").unwrap().contains("authority `auth`"));
        assert_eq!(missing_in(&data, &copy.join("sub"), "sandbox folder", "../web"), None);
        assert_eq!(missing_in(&data, &dir, "sandbox folder", "../web"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
