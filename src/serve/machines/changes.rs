//! Membership changes over the links (docs/architecture/daemon/fleet.md,
//! *Approvals* and *Pushes*; fleet plan step 10a.6, decisions 45 and 49):
//!
//! - **Signed on the authority.** `machine rm <m>` there ([`revoke`]), its
//!   `machine key rotate` ([`rotate`]) or an approved proposal
//!   ([`approve`]) appends the entry, signed with this host's key, and
//!   stores it through the adopt path (`membership_api::adopt`, which
//!   writes `hub` from the log and promotes a rotated key); a revoke then
//!   runs the removal (`super::leave`).
//! - **Pushed.** After every signed change the authority sends its log to
//!   every connected machine ([`push`], `fleet.sync`). A host that takes
//!   newer entries from a `fleet.sync` sends them on to its other connected
//!   machines, off the request thread; one not reached catches up at its
//!   next link's head comparison (`fleet::proof`).
//! - **Proposed elsewhere.** On another member the change goes to the
//!   authority as `fleet.propose` over any path ([`propose`]: a direct link
//!   while up, else a route), queued there (`fleet::proposals`)
//!   with a desktop notification until the user runs `devsandbox machine
//!   approve|deny <id>` there (`membership.approve|deny`). The authority's
//!   key never signs on a remote request alone.
//! - **Key rotation.** `membership.rotate` makes the next key
//!   (`member.key.next`, reused while one is pending) and countersigns it
//!   with the key. On the authority the `rotate` is signed with the old
//!   key, stored (promoting the next key) and pushed; elsewhere it's
//!   proposed, and the member promotes the next key when a log listing it
//!   reaches it (`fleet.sync`, a link's sync: `membership_api::settle`).
//!   A host that promoted ends every link it's on (`Ctx::key_rotated`),
//!   after its push (the authority) or its answer and onward push (a
//!   member's `fleet.sync`), so each redials proving the new key.
//! - **Hub move.** `membership.hub {name}` ([`hub`]) signs a `hub` entry on
//!   the authority, else proposes it there. A host follows a hub change
//!   (`Ctx::hub_changed`: `hub` kept, every link ended so both ends redial
//!   saying it) only once the change has left it (fleet plan decision 69):
//!   the authority after its push, a member a `fleet.sync` moved after its
//!   onward push and its answer (`After::Hub`). Cycling first would cut
//!   the links carrying it; a host it misses learns it at its next link's
//!   proof (`fleet::proof::hub_moved`).
//! - **Config versions.** A `run`/`rebuild`/`start` typed on the
//!   authority ([`sign_config`], called by `serve::daemon`'s verb path)
//!   first signs its config root's hash as the root's next `config`
//!   version when it differs from the latest, and pushes it. Never
//!   proposed: only the authority's own verbs sign. A host the push (or a
//!   flood) moves a head on installs it from the sender before it answers
//!   or sends the log on ([`sync`]), so a push that reached a machine
//!   left the config there.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::api::{self, Ctx};
use super::leave::{self, After, Authority, Missed, Removed};
use super::service::Handle;
use crate::serve::api::{ApiError, parse};
use crate::serve::fleet::identity::{FileKeyStore, KeyStore, PublicKey, Signer, SigningKey};
use crate::serve::fleet::member::{self, Hub};
use crate::serve::fleet::membership::{self, Op, Stored, Wire, countersign_rotate};
use crate::serve::fleet::membership_api;
use crate::serve::fleet::mesh;
use crate::serve::fleet::proposals::{self, Change, Proposal};

type Answer = Result<Value, ApiError>;

/// How long a push waits for each `fleet.sync` answer. A receiver whose
/// log took a moved `config` head installs it before answering
/// ([`sync`]): pulling a bundle of up to `code::ROOT_TAR_LIMIT` bytes from
/// this host (each read bounded by `code::SEND_WRITE_WAIT`), unpacking and
/// re-hashing it. Far over `Timing::call`, whose timeout would end the link
/// mid-install; a 256 MiB bundle in 10 minutes needs about 0.45 MB/s.
pub const SYNC_WAIT: Duration = Duration::from_secs(10 * 60);

/// What a change signed on the authority did: `machines.rm`'s answer
/// there (`type: revoked`), and `membership.approve`'s.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signed {
    /// The member the change is about.
    pub name: String,
    /// The entry signed, as the log says it (``revoke `pi` ``).
    pub change: String,
    /// The machines its push reached.
    pub pushed: Vec<String>,
    /// Those it didn't (they catch up on their next link).
    pub unpushed: Vec<Missed>,
    /// A revoke's removal: run here on the hub, or the hub's answer to
    /// `fleet.rm`.
    pub removed: Option<Removed>,
    /// Why a revoke has no `removed` (the hub not reached, or refusing).
    pub error: Option<String>,
}

/// A change proposed to the authority: its id there, or why it wasn't.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proposed {
    /// The member the change is about.
    pub name: String,
    pub authority: String,
    pub id: Option<u64>,
    pub error: Option<String>,
}

/// `membership.approve`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approved {
    pub id: u64,
    #[serde(flatten)]
    pub signed: Signed,
}

/// `membership.hub`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum HubMove {
    /// On the authority: the `hub` entry signed, stored, pushed, then
    /// followed here.
    Signed(Signed),
    /// On another member: proposed to the authority.
    Proposed(Proposed),
}

/// `membership.rotate`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Rotation {
    /// On the authority: signed, stored, the next key promoted, pushed.
    Rotated(Rotated),
    /// On another member: proposed to the authority; the next key waits.
    Proposed(RotateProposed),
}

/// A rotate signed here (the authority).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rotated {
    /// The new key, this host's now.
    pub key: PublicKey,
    pub fingerprint: String,
    #[serde(flatten)]
    pub signed: Signed,
}

/// A rotate proposed to the authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RotateProposed {
    /// The next key (`member.key.next`), this host's once the signed
    /// rotate reaches it.
    pub key: PublicKey,
    pub fingerprint: String,
    #[serde(flatten)]
    pub proposed: Proposed,
}

/// Run `method` if it's one of the local
/// `membership.pending|approve|deny|rotate|hub`; `None` otherwise.
pub fn call(method: &str, params: Value, ctx: &Ctx) -> Option<Answer> {
    let answer = match method {
        "membership.rotate" => parse::<Empty>(params).and_then(|_| rotate(ctx)).and_then(to_value),
        "membership.hub" => parse::<NameParams>(params).and_then(|p| hub(ctx, &p.name)).and_then(to_value),
        "membership.pending" => pending(ctx),
        "membership.approve" => parse::<IdParams>(params).and_then(|p| approve(p.id, ctx)),
        "membership.deny" => parse::<IdParams>(params).and_then(|p| deny(p.id, ctx)),
        _ => return None,
    };
    Some(answer)
}

/// `fleet.propose` / `fleet.sync` from link peer `from`, admitted
/// (`fleet::lent::admit_fleet`).
pub fn fleet_call(method: &str, params: Value, from: &str, ctx: &Ctx) -> Result<(Value, Option<After>), ApiError> {
    match method {
        "fleet.propose" => queue(params, from, ctx).map(|v| (v, None)),
        "fleet.sync" => sync(params, from, ctx),
        m => Err(ApiError::unknown_method(m)),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IdParams {
    id: u64,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Empty {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NameParams {
    name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProposeParams {
    op: Change,
}

/// `fleet.propose`'s answer.
#[derive(Debug, Serialize, Deserialize)]
pub struct Queued {
    pub id: u64,
    pub pending: bool,
}

/// `fleet.sync`'s answer: whether this host's log took something, and its
/// head after.
#[derive(Debug, Serialize, Deserialize)]
pub struct Synced {
    pub changed: bool,
    pub head: crate::serve::fleet::membership::Head,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn to_value(x: impl Serialize) -> Answer {
    serde_json::to_value(x).map_err(|e| ApiError::internal(e.into()))
}

fn no_log() -> ApiError {
    ApiError::failed("this host is in no fleet's membership log")
}

/// This host's member name and stored log, when it's the log's authority
/// (by its name and key); `denied` naming the authority otherwise.
fn on_authority(ctx: &Ctx) -> Result<(String, Stored), ApiError> {
    let path = leave::member_toml(ctx)?;
    let (me, key) = membership_api::own(path, &ctx.hostname)?;
    let stored = membership_api::load(path)?.filter(|s| s.log.membership().is_some()).ok_or_else(no_log)?;
    match leave::authority_of(Some(&stored), &me, &key.public_key()) {
        Authority::This => Ok((me, stored)),
        Authority::Other(a) => Err(ApiError::denied(format!(
            "this host isn't the fleet's authority `{a}`: membership changes are signed there"
        ))),
        Authority::None => Err(no_log()),
    }
}

/// `op` signed with this host's key (the authority's) and stored through
/// the adopt path (`hub` written from the log). What the store did, for the
/// caller to [`follow`] once it has pushed: the hub written (`Taken::hub`),
/// a `rotate` of this host's own key promoting its next key
/// (`Taken::rotated`).
fn sign(ctx: &Ctx, me: &str, mut stored: Stored, op: Op) -> Result<membership_api::Taken, ApiError> {
    let path = leave::member_toml(ctx)?;
    let (_, signer) = membership_api::own(path, &ctx.hostname)?;
    stored.log.append(op, me, &signer).map_err(|e| ApiError::invalid(format!("{e:#}")))?;
    membership_api::adopt(path, &ctx.hostname, &stored)
}

/// What a [`sign`] stored, followed by the links once its push is out
/// (fleet plan decision 69): the hub (a move ends every link, so both ends
/// redial saying it), a promoted key (every link ends, redialing to prove
/// it). Each entry does one or neither.
fn follow(ctx: &Ctx, taken: &membership_api::Taken) {
    (ctx.hub_changed)(taken.hub.clone());
    if taken.rotated {
        (ctx.key_rotated)();
    }
}

/// [`sign`], then [`push`] over the links as they are, then [`follow`].
fn sign_and_push(ctx: &Ctx, me: &str, stored: Stored, op: Op) -> Result<(Stored, Vec<String>, Vec<Missed>), ApiError> {
    let taken = sign(ctx, me, stored, op)?;
    let (pushed, unpushed) = ctx.links.as_ref().map(|l| push(l, &taken.stored, None)).unwrap_or_default();
    follow(ctx, &taken);
    Ok((taken.stored, pushed, unpushed))
}

/// `stored` sent to every machine `links` holds but `except`
/// (`fleet.sync`, one at a time, each answer waited for up to
/// [`SYNC_WAIT`]): those it reached, and those it didn't, with why. A
/// machine reached holds every config head `stored` moved there, installed
/// (or its install failed, logged there).
pub fn push(links: &Handle, stored: &Stored, except: Option<&str>) -> (Vec<String>, Vec<Missed>) {
    let params = match serde_json::to_value(stored.to_wire()) {
        Ok(v) => v,
        Err(e) => return (Vec::new(), vec![Missed { name: "*".into(), error: format!("cannot encode the log: {e}") }]),
    };
    let (mut pushed, mut missed) = (Vec::new(), Vec::new());
    for entry in links.entries() {
        if except == Some(entry.name.as_str()) {
            continue;
        }
        match leave::call_on_within(links, &entry.name, "fleet.sync", params.clone(), SYNC_WAIT) {
            Ok(_) => pushed.push(entry.name),
            Err(error) => missed.push(Missed { name: entry.name, error }),
        }
    }
    (pushed, missed)
}

/// On the authority: `name`'s key revoked (signed, unless the log already
/// revokes `name`: a retry), the log pushed, then the removal: here when
/// this host is the hub, else the hub's `fleet.rm` (which the push let in).
pub fn revoke(ctx: &Ctx, name: &str) -> Result<Signed, ApiError> {
    let (me, stored) = on_authority(ctx)?;
    let (pushed, unpushed) = if stored.log.revoked_name(name) {
        ctx.links.as_ref().map(|l| push(l, &stored, None)).unwrap_or_default()
    } else {
        let m = stored.log.membership().expect("on_authority checked");
        if !m.members.contains_key(name) {
            let held = stored.introductions.iter().find(|i| i.name == name);
            return Err(match held {
                Some(i) => ApiError::denied(format!(
                    "`{name}` is introduced by `{}`, not in the fleet's membership log: there's no key of it to revoke",
                    i.introducer
                )),
                None => ApiError::not_found(format!("`{name}` is not a member of the fleet's membership log")),
            });
        }
        let op = proposals::check(m, &me, &Change::Revoke { name: name.into() }).map_err(ApiError::denied)?;
        let (_, pushed, unpushed) = sign_and_push(ctx, &me, stored, op)?;
        (pushed, unpushed)
    };
    let (_, hub) = api::own_member(ctx)?;
    let (removed, error) = match hub {
        Hub::This => {
            let machines: Vec<String> = api::machines(ctx)?.into_iter().map(|e| e.name).collect();
            let (removed, after) = leave::remove(ctx, &me, name, true, &machines)?;
            leave::apply(after, ctx);
            (Some(removed), None)
        }
        Hub::Other(hub) => match leave::call(ctx, &hub, "fleet.rm", json!({ "name": name })) {
            Ok(v) => match serde_json::from_value::<Removed>(v) {
                Ok(r) => (Some(r), None),
                Err(e) => (None, Some(format!("{hub}: fleet.rm: unexpected answer: {e}"))),
            },
            Err(why) => (None, Some(format!("the hub `{hub}` didn't remove it ({why}): it does once the revoke reaches it and `machine rm {name}` runs here again"))),
        },
        Hub::None => (None, Some("this host has no hub: nothing removes it from the links".into())),
    };
    Ok(Signed { name: name.into(), change: format!("revoke `{name}`"), pushed, unpushed, removed, error })
}

/// On the authority: `change` from member `from` (an introduction promoted
/// to an `add`, a member's `rotate` of its own key, a `hub`) signed, the log
/// pushed, then followed here.
fn sign_change(ctx: &Ctx, from: &str, change: &Change) -> Result<Signed, ApiError> {
    let (me, stored) = on_authority(ctx)?;
    let m = stored.log.membership().expect("on_authority checked");
    let op = proposals::check(m, from, change).map_err(ApiError::invalid)?;
    let what = op.to_string();
    let (_, pushed, unpushed) = sign_and_push(ctx, &me, stored, op)?;
    Ok(Signed { name: change.subject().to_string(), change: what, pushed, unpushed, removed: None, error: None })
}

/// What a verb typed here did to its config root's version
/// ([`sign_config`]), for `serve.log` and the verb's own output
/// ([`ConfigSigned::notes`]). `root` is the root's name in the log (its
/// directory's name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSigned {
    /// Signed and pushed; `unpushed` are the machines the push didn't
    /// reach, so they don't have `version` (a reached one installed it).
    Signed { root: String, version: u64, unpushed: Vec<Missed> },
    Failed { root: String, error: String },
    /// This host is in a fleet but isn't its authority `authority`: what
    /// it types signs nothing.
    NotAuthority { root: String, authority: String },
}

impl ConfigSigned {
    fn failed(root: &str, error: String) -> ConfigSigned {
        ConfigSigned::Failed { root: root.into(), error }
    }

    /// Its `serve.log` lines (pure): a sign, and in one more line the
    /// machines its push missed; a failure. A non-authority logs nothing
    /// (said to the verb only).
    pub fn log_lines(&self) -> Vec<String> {
        match self {
            ConfigSigned::Signed { root, version, unpushed } => {
                let mut lines = vec![format!("signed config `{root}` v{version}")];
                if !unpushed.is_empty() {
                    let missed: Vec<String> = unpushed.iter().map(|m| format!("{} ({})", m.name, m.error)).collect();
                    lines.push(format!("config `{root}` v{version} not pushed to {}", missed.join(", ")));
                }
                lines
            }
            ConfigSigned::Failed { root, error } => vec![format!("config `{root}` not signed: {error}")],
            ConfigSigned::NotAuthority { .. } => Vec::new(),
        }
    }

    /// The `note:` lines a verb's output starts with (pure).
    pub fn notes(&self) -> Vec<String> {
        match self {
            ConfigSigned::Signed { root, version, unpushed } => {
                let mut lines = vec![format!("note: signed config `{root}` v{version}")];
                lines.extend(
                    unpushed.iter().map(|m| format!("note: `{}` doesn't have config `{root}` v{version}: {}", m.name, m.error)),
                );
                lines
            }
            ConfigSigned::Failed { root, error } => vec![format!("note: config `{root}` not signed: {error}")],
            ConfigSigned::NotAuthority { root, authority } => vec![format!(
                "note: config `{root}` not signed: the fleet's authority is `{authority}`, not this host, and configs here aren't signed"
            )],
        }
    }
}

/// The fleet's authority, when this host is in a fleet (a membership log)
/// but isn't it: what it types signs no config ([`sign_config`]).
pub fn authority_elsewhere(ctx: &Ctx) -> Option<String> {
    let path = leave::member_toml(ctx).ok()?;
    let (me, key) = membership_api::own(path, &ctx.hostname).ok()?;
    let stored = membership_api::load(path).ok()??;
    stored.log.membership()?;
    match leave::authority_of(Some(&stored), &me, &key.public_key()) {
        Authority::Other(a) => Some(a.to_string()),
        Authority::This | Authority::None => None,
    }
}

/// The local config root this host bound root name `name` to
/// ([`mesh::bound_root`]): what a verb typed here for a machine signs
/// ([`sign_config`]) when its remote root has that name. `None` when
/// unbound, or this host is in no fleet.
pub fn bound_root(ctx: &Ctx, name: &str) -> Option<PathBuf> {
    let data = leave::member_toml(ctx).ok()?.parent()?;
    mesh::bound_root(data, name).ok()?
}

/// Before a `run`/`rebuild`/`start` typed here: config root `root`
/// (absolute) bundled ([`mesh::collect`]) and, when the bundle's hash isn't
/// the root's latest `config` entry's, the bundle stored
/// ([`mesh::store`], next to `member.toml`) then signed as the next
/// version (1 for the first) and pushed. `None`, nothing bundled: this
/// host is in no fleet, or `root` holds no config file (the verb says
/// why). `None` too when the hash is the latest's. `NotAuthority` when
/// this host is in a fleet whose authority is another member. A config
/// that doesn't load is `Failed`, nothing signed. So is a name bound to
/// another root: the first canonical path a name is signed from keeps it
/// ([`mesh::bind`], recorded before that first entry is signed).
pub fn sign_config(ctx: &Ctx, root: &Path) -> Option<ConfigSigned> {
    let path = leave::member_toml(ctx).ok()?;
    let (me, key) = membership_api::own(path, &ctx.hostname).ok()?;
    let stored = membership_api::load(path).ok()??;
    stored.log.membership()?;
    let authority = leave::authority_of(Some(&stored), &me, &key.public_key());
    if authority == Authority::None {
        return None;
    }
    crate::config::find(root).ok()??;
    let name = root.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    if let Authority::Other(a) = authority {
        return Some(ConfigSigned::NotAuthority { root: name.into(), authority: a.into() });
    }
    if let Err(e) = membership::check_root_name(name) {
        return Some(ConfigSigned::failed(name, format!("config root {}: {e:#}", root.display())));
    }
    let canon = match root.canonicalize() {
        Ok(c) => c,
        Err(e) => return Some(ConfigSigned::failed(name, format!("cannot resolve {}: {e}", root.display()))),
    };
    let data = path.parent().expect("member.toml has a parent");
    let elsewhere = |bound: &Path| {
        ConfigSigned::failed(name, format!("the name `{name}` is bound to {}, not {}", bound.display(), canon.display()))
    };
    match mesh::bound_root(data, name) {
        Ok(Some(bound)) if bound != canon => return Some(elsewhere(&bound)),
        Ok(_) => {}
        Err(e) => return Some(ConfigSigned::failed(name, format!("{e:#}"))),
    }
    let bundle = match mesh::collect(root) {
        Ok(b) => b,
        Err(e) => return Some(ConfigSigned::failed(name, format!("{e:#}"))),
    };
    let hash = bundle.hash();
    let current = stored.log.membership().expect("checked").config.get(name).copied();
    if current.is_some_and(|(_, h)| h == hash) {
        // Nothing to sign, but the stored bundle peers pull is rewritten
        // when it's missing or damaged (`mesh::store` keeps an intact
        // one), else this host would serve a bad one until the next edit.
        return mesh::store(data, &bundle).err().map(|e| ConfigSigned::failed(name, format!("{e:#}")));
    }
    match mesh::bind(data, name, &canon) {
        Ok(Ok(())) => {}
        Ok(Err(bound)) => return Some(elsewhere(&bound)),
        Err(e) => return Some(ConfigSigned::failed(name, format!("{e:#}"))),
    }
    if let Err(e) = mesh::store(data, &bundle) {
        return Some(ConfigSigned::failed(name, format!("{e:#}")));
    }
    let version = current.map_or(1, |(v, _)| v + 1);
    match sign_and_push(ctx, &me, stored, Op::Config { root: name.into(), version, hash }) {
        Ok((_, _, unpushed)) => Some(ConfigSigned::Signed { root: name.into(), version, unpushed }),
        Err(e) => Some(ConfigSigned::failed(name, e.message)),
    }
}

/// Whether `me` (key `key`) can rotate its key in `stored`'s log (pure):
/// only a member of the log, by that key. A host only introduced (capped)
/// has no log key to rotate; one in no log has none either.
pub fn can_rotate(stored: Option<&Stored>, me: &str, key: &PublicKey) -> Result<(), ApiError> {
    let Some(s) = stored.filter(|s| s.log.membership().is_some()) else { return Err(no_log()) };
    let m = s.log.membership().expect("filtered");
    if m.key_of(me) == Some(*key) {
        return Ok(());
    }
    if let Some(i) = s.introductions.iter().find(|i| i.name == me && i.key == *key) {
        return Err(ApiError::denied(format!(
            "`{me}` is introduced by `{}`, not in the fleet's membership log: its key has no entry to rotate until the \
             authority `{}` adds it (`machine approve` there)",
            i.introducer, m.authority.0
        )));
    }
    Err(ApiError::denied(format!("`{me}` (this key) isn't a member of its fleet's membership log: it has no key there to rotate")))
}

/// What `membership.rotate` ([`plan_rotate`]) or `membership.hub`
/// ([`plan_hub`]) does (pure): the entry checked against the log, signed
/// here on the authority, else proposed to it.
pub enum RotatePlan {
    Sign(Op),
    Propose { authority: String, change: Change },
}

/// `membership.rotate`'s, once [`can_rotate`] holds: the `rotate` of `me`'s
/// key (`key`) to `next`, countersigned by `key`.
pub fn plan_rotate(stored: &Stored, me: &str, key: &SigningKey, next: &PublicKey) -> Result<RotatePlan, ApiError> {
    can_rotate(Some(stored), me, &key.public_key())?;
    let m = stored.log.membership().expect("can_rotate checked");
    let by_old = countersign_rotate(&m.genesis, me, next, key);
    let change = Change::Rotate { name: me.to_string(), old: key.public_key(), new: *next, by_old };
    let op = proposals::check(m, me, &change).map_err(ApiError::invalid)?;
    Ok(match leave::authority_of(Some(stored), me, &key.public_key()) {
        Authority::This => RotatePlan::Sign(op),
        Authority::Other(a) => RotatePlan::Propose { authority: a.to_string(), change },
        Authority::None => return Err(no_log()),
    })
}

/// `membership.rotate`: this host's next key (`member.key.next`; a pending
/// one is reused), its `rotate` signed here on the authority (with the old
/// key; storing it promotes the next key, the push sends it on) or proposed
/// to the authority over any path ([`propose`], collapsing with a
/// pending one). Refused for a capped member or one in no log, before any
/// key is made.
pub fn rotate(ctx: &Ctx) -> Result<Rotation, ApiError> {
    let path = leave::member_toml(ctx)?;
    let (me, key) = membership_api::own(path, &ctx.hostname)?;
    let stored = membership_api::load(path)?;
    can_rotate(stored.as_ref(), &me, &key.public_key())?;
    let stored = stored.expect("can_rotate checked");
    let store = FileKeyStore::new(member::key_path(path));
    let next = store.next_key().map_err(ApiError::internal)?.public_key();
    let (key_text, fingerprint) = (next, next.fingerprint());
    match plan_rotate(&stored, &me, &key, &next)? {
        RotatePlan::Sign(op) => {
            let what = op.to_string();
            let taken = sign(ctx, &me, stored, op)?;
            let (_, now) = membership_api::own(path, &ctx.hostname)?;
            if now.public_key() != next {
                return Err(ApiError::internal(anyhow::anyhow!("the rotate is stored, but {} isn't promoted", store.next_path().display())));
            }
            // The push first, over the links as they are (each peer proved
            // and holds the old key, which its log still lists), so every
            // peer holds the `rotate` before the links end and redial
            // proving the new key.
            let (pushed, unpushed) = ctx.links.as_ref().map(|l| push(l, &taken.stored, None)).unwrap_or_default();
            follow(ctx, &taken);
            let signed = Signed { name: me, change: what, pushed, unpushed, removed: None, error: None };
            Ok(Rotation::Rotated(Rotated { key: key_text, fingerprint, signed }))
        }
        RotatePlan::Propose { authority, change } => {
            let id = propose(ctx, &authority, change)?;
            let proposed = Proposed { name: me, authority, id: Some(id), error: None };
            Ok(Rotation::Proposed(RotateProposed { key: key_text, fingerprint, proposed }))
        }
    }
}

/// `membership.hub`'s plan (pure): a `hub` entry moving the role to `name`,
/// asked by `me` (key `key`), a member of `stored`'s log by that key (an
/// introduced host or one in no log has no say in it), checked against the
/// log ([`proposals::check`]: `name` a member, not the hub already).
pub fn plan_hub(stored: Option<&Stored>, me: &str, key: &PublicKey, name: &str) -> Result<RotatePlan, ApiError> {
    let Some(s) = stored.filter(|s| s.log.membership().is_some()) else { return Err(no_log()) };
    let m = s.log.membership().expect("filtered");
    if m.key_of(me) != Some(*key) {
        return Err(match s.introductions.iter().find(|i| i.name == me && i.key == *key) {
            Some(i) => ApiError::denied(format!(
                "`{me}` is introduced by `{}`, not in the fleet's membership log: it can't move the hub role until the \
                 authority `{}` adds it (`machine approve` there)",
                i.introducer, m.authority.0
            )),
            None => ApiError::denied(format!("`{me}` (this key) isn't a member of its fleet's membership log: it can't move the hub role")),
        });
    }
    let change = Change::Hub { name: name.to_string() };
    let op = proposals::check(m, me, &change).map_err(ApiError::invalid)?;
    Ok(match leave::authority_of(Some(s), me, key) {
        Authority::This => RotatePlan::Sign(op),
        Authority::Other(a) => RotatePlan::Propose { authority: a.to_string(), change },
        Authority::None => return Err(no_log()),
    })
}

/// `membership.hub {name}`: the fleet's hub role moved to member `name`. On
/// the authority the `hub` entry is signed, stored, pushed over the links as
/// they are, and only then followed here (every link cycled, so the push
/// isn't cut); elsewhere it's proposed to the authority over any path
/// ([`propose`], collapsing with a pending one of the same name).
pub fn hub(ctx: &Ctx, name: &str) -> Result<HubMove, ApiError> {
    let path = leave::member_toml(ctx)?;
    let (me, key) = membership_api::own(path, &ctx.hostname)?;
    let stored = membership_api::load(path)?;
    match plan_hub(stored.as_ref(), &me, &key.public_key(), name)? {
        RotatePlan::Sign(op) => {
            let what = op.to_string();
            let (_, pushed, unpushed) = sign_and_push(ctx, &me, stored.expect("plan_hub checked"), op)?;
            Ok(HubMove::Signed(Signed { name: name.into(), change: what, pushed, unpushed, removed: None, error: None }))
        }
        RotatePlan::Propose { authority, change } => {
            let id = propose(ctx, &authority, change)?;
            Ok(HubMove::Proposed(Proposed { name: name.into(), authority, id: Some(id), error: None }))
        }
    }
}

/// `change` proposed to the fleet's authority `authority` over any path
/// (`leave::call`: its direct link while up, else a route over this
/// host's link graph): its id there. `failed` naming it when no path
/// reaches it, or it refuses.
pub fn propose(ctx: &Ctx, authority: &str, change: Change) -> Result<u64, ApiError> {
    let v = leave::call(ctx, authority, "fleet.propose", json!({ "op": change }))
        .map_err(|why| ApiError::failed(format!("the fleet's authority `{authority}`: {why}")))?;
    let q: Queued = serde_json::from_value(v).map_err(|e| ApiError::failed(format!("{authority}: fleet.propose: unexpected answer: {e}")))?;
    Ok(q.id)
}

/// `fleet.propose {op}` on the authority, from member `from`: checked
/// against the log now, queued (a pending one of the same change answers
/// its id), and the user told on the desktop when it's new.
fn queue(params: Value, from: &str, ctx: &Ctx) -> Answer {
    let p: ProposeParams = parse(params)?;
    let (_, stored) = on_authority(ctx)?;
    let m = stored.log.membership().expect("on_authority checked");
    proposals::check(m, from, &p.op).map_err(ApiError::invalid)?;
    let what = p.op.to_string();
    let path = proposals::path(leave::member_toml(ctx)?);
    let (id, new) = proposals::update(&path, |q| q.queue(from, p.op, now())).map_err(ApiError::internal)?;
    if new {
        (ctx.notify)(&format!("{from} proposes to {what}: devsandbox machine approve {id}"));
    }
    to_value(Queued { id, pending: true })
}

/// `fleet.sync {entries, introductions}` from `from`: merged as a link's
/// handshake merges (self-verifying; another fleet's or a fork refused;
/// a log revoking this host's own key taken, for the `fleet.leave` that
/// follows it). When it made this host's log newer: when it moved a
/// `config` head ([`config_moved`]), the signed configs followed from
/// `from` (`Ctx::log_from`: pulled and installed) before anything else, so
/// the answer means this host has them and the hosts it sends the log on
/// to can pull them from it; an install that fails is logged there, the
/// sync answered all the same (the log is taken). Then `hub` followed, and
/// the log sent on to every other machine this host holds, off the request
/// thread. When it promoted this host's next key (a rotate of it is in) or
/// moved this host's hub, the log is sent on before answering, nothing
/// installed, and every link ends once the answer is out
/// (`After::Rotated`: redialing to prove the new key; `After::Hub`: the
/// hub followed, redialing to say it); the redial's link-up follows the
/// configs.
fn sync(params: Value, from: &str, ctx: &Ctx) -> Result<(Value, Option<After>), ApiError> {
    let wire: Wire = parse(params)?;
    let theirs = Stored::from_wire(wire).map_err(|e| ApiError::invalid(format!("{e:#}")))?;
    let path = leave::member_toml(ctx)?;
    let before = api::own_member(ctx).ok().map(|(_, hub)| hub);
    let configs_before = config_heads(membership_api::load(path).ok().flatten().as_ref());
    let taken = membership_api::sync(path, &ctx.hostname, &theirs)?;
    let head = taken.stored.log.head();
    let mut after = None;
    if taken.changed {
        let moved = before.as_ref() != Some(&taken.hub);
        // Both end every link once the answer is out. A move that comes
        // with a rotate needs one cycle only: `After::Hub` keeps the new
        // hub, and the redial proves the key `member.key` holds by then.
        let follow = match (moved, taken.rotated) {
            (true, _) => Some(After::Hub(taken.hub.clone())),
            (false, true) => Some(After::Rotated),
            (false, false) => None,
        };
        if follow.is_none() && config_moved(&configs_before, &config_heads(Some(&taken.stored))) {
            (ctx.log_from)(from);
        }
        if let Some(links) = ctx.links.clone() {
            if follow.is_some() {
                // The links end once the answer is out, so the log goes on
                // over them now, before that.
                flood(&links, &taken.stored, from);
            } else {
                let (sender, stored) = (from.to_string(), taken.stored.clone());
                let thread = std::thread::Builder::new().name("serve-fleet-sync".into()).spawn(move || flood(&links, &stored, &sender));
                if let Err(e) = thread {
                    crate::serve::daemon::log(&format!("fleet.sync from {from}: cannot send it on: {e}"));
                }
            }
        }
        if follow.is_none() {
            (ctx.hub_changed)(taken.hub);
        }
        after = follow;
    }
    Ok((to_value(Synced { changed: taken.changed, head })?, after))
}

/// `stored`'s `config` heads (`Membership::config`), none without a log.
fn config_heads(stored: Option<&Stored>) -> BTreeMap<String, (u64, [u8; 32])> {
    stored.and_then(|s| s.log.membership()).map(|m| m.config.clone()).unwrap_or_default()
}

/// Whether a log's `config` heads went from `before` to `after` with one
/// moved or added (pure): what [`sync`] installs before answering.
fn config_moved(before: &BTreeMap<String, (u64, [u8; 32])>, after: &BTreeMap<String, (u64, [u8; 32])>) -> bool {
    after.iter().any(|(name, head)| before.get(name) != Some(head))
}

/// `stored`, newer than what `from` sent had here, sent on to every other
/// machine `links` holds, logged.
fn flood(links: &Handle, stored: &Stored, from: &str) {
    let (pushed, missed) = push(links, stored, Some(from));
    let missed: Vec<String> = missed.iter().map(|m| format!("{} ({})", m.name, m.error)).collect();
    crate::serve::daemon::log(&format!(
        "fleet.sync from {from}: newer membership entries ({}) sent on to [{}]{}",
        stored.log.head(),
        pushed.join(", "),
        if missed.is_empty() { String::new() } else { format!(", not to {}", missed.join(", ")) }
    ));
}

/// `membership.pending`: the proposals waiting here, oldest first.
fn pending(ctx: &Ctx) -> Answer {
    let path = proposals::path(leave::member_toml(ctx)?);
    let q = proposals::load(&path).map_err(ApiError::internal)?;
    let list: Vec<Proposal> = q.pending;
    to_value(list)
}

/// `membership.approve {id}`: the proposal signed here (this host the
/// authority, the change checked against the log now, a rotate as its
/// proposer's), pushed, and a revoke's removal run as `machine rm` typed
/// here would; then dropped
/// from the queue. A change that no longer applies stays queued, `invalid`
/// saying why (`membership.deny` drops it).
fn approve(id: u64, ctx: &Ctx) -> Answer {
    let path = proposals::path(leave::member_toml(ctx)?);
    let q = proposals::load(&path).map_err(ApiError::internal)?;
    let p = q.get(id).cloned().ok_or_else(|| ApiError::not_found(format!("no pending proposal {id}")))?;
    let signed = match &p.op {
        Change::Revoke { name } => revoke(ctx, name),
        Change::Add { .. } | Change::Rotate { .. } | Change::Hub { .. } => sign_change(ctx, &p.from, &p.op),
    };
    let signed = signed.map_err(|mut e| {
        e.message = format!("proposal {id} ({}): {}; `devsandbox machine deny {id}` drops it", p.op, e.message);
        e
    })?;
    proposals::update(&path, |q| q.take(id)).map_err(ApiError::internal)?;
    to_value(Approved { id, signed })
}

/// `membership.deny {id}`: the proposal dropped, nothing signed.
fn deny(id: u64, ctx: &Ctx) -> Answer {
    let path = proposals::path(leave::member_toml(ctx)?);
    match proposals::update(&path, |q| q.take(id)).map_err(ApiError::internal)? {
        Some(_) => Ok(json!({ "ok": true })),
        None => Err(ApiError::not_found(format!("no pending proposal {id}"))),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Duration;

    use serde_json::{Value, json};

    use crate::serve::daemon::Exit;
    use crate::serve::daemon::tests::{scratch, spawn_daemon};
    use crate::serve::endpoint;
    use crate::serve::fleet::mesh;
    use crate::serve::fleet::dialer::tests::{Pair, connect, connected, daemon_opts, lap_on_k, named, peer_transport, wait_for};
    use super::{Change, ConfigSigned, Missed, Op, RotatePlan, can_rotate, config_moved, countersign_rotate, plan_hub, plan_rotate};
    use crate::serve::fleet::identity::{PublicKey, Signer, SigningKey};
    use crate::serve::fleet::member::{Member, write_for_test};
    use crate::serve::fleet::membership::{Introduction, Stored};
    use crate::serve::fleet::membership_api::test_support::{self, key_of};
    use crate::serve::fleet::link;
    use crate::serve::machines::Entry;
    use crate::serve::machines::link::{Over, handshake};
    use crate::serve::machines::ssh::Duplex;

    fn pk(n: u8) -> PublicKey {
        SigningKey::from_seed([n; 32]).public_key()
    }

    /// Each `(member.toml, name, hub field)` named and hubbed so, and given
    /// `stored` as its `membership.json`.
    fn write_fleet(stored: &Stored, members: &[(&Path, &str, &str)]) {
        for (toml, name, hub) in members {
            write_for_test(toml, &Member { name: name.to_string(), hub: hub.to_string(), ..Default::default() });
            test_support::write(toml, stored);
        }
    }

    /// A [`Pair`]: K (`box`, the hub, with a registry entry `x` it never
    /// reaches) and L (`lap`), in the fleet `log` makes from K's and L's
    /// `member.toml`.
    fn pair(tag: &str, log: impl FnOnce(&Path, &Path) -> Stored + Send + 'static) -> Pair {
        Pair::build(tag, vec![Entry::ssh("x", "me@x")], |_| {}, |_| {}, move |k, l| {
            let stored = log(k, l);
            write_fleet(&stored, &[(k, "box", "self"), (l, "lap", "box")]);
        })
    }

    /// An `API` stream over this host's link to `name` (the far end sees
    /// this host as its peer).
    fn api_to(dir: &Path, name: &str) -> crate::serve::client::Conn {
        let stream = link::connect(&endpoint::fleet_socket(dir, name), crate::devsbd::proto::channel::API).unwrap();
        handshake(name, Duplex::socket(stream), Duration::from_secs(10), Over::Link).unwrap().conn
    }

    fn call(dir: &Path, method: &str, params: Value) -> Result<Value, (String, String)> {
        let r = connect(dir).call(method, params).unwrap();
        match r.error {
            Some(e) => Err((e.code, e.message)),
            None => Ok(r.result.unwrap_or(Value::Null)),
        }
    }

    fn up(pair: &Pair) {
        pair.wait_box("box connected", connected);
        wait_for("lap connected on K", Duration::from_secs(10), || lap_on_k(pair).is_some_and(|r| connected(&r)));
    }

    /// `lap` (L) the authority, `box` (K) the hub; `zed` and `yan` members
    /// never running. K's `machine rm zed` is proposed to L over K's link to
    /// it, collapses while pending, is denied, proposed again and approved:
    /// L signs the revoke, pushes it to K, and K's removal (asked with
    /// `fleet.rm`) has L forget zed. L's own `machine rm yan` is signed at
    /// once and removed by K the same way.
    #[test]
    fn a_members_rm_is_proposed_and_approved_on_the_authority() {
        let pair = pair("changes-approve", |k, l| test_support::fleet("lap", &key_of(l), "box", &[("box", key_of(k).public_key()), ("zed", pk(7)), ("yan", pk(8))]));
        up(&pair);
        let k_toml = pair.base.join("k-data/member.toml");
        let l_toml = pair.base.join("l-data/member.toml");

        let r = call(&pair.k_dir, "machines.rm", json!({"name": "zed"})).unwrap();
        crate::serve::dts::assert_variant("MachineRmProposed", &r);
        assert_eq!(r, json!({"type": "proposed", "name": "zed", "authority": "lap", "id": 1, "error": null}));
        assert_eq!(call(&pair.k_dir, "machines.rm", json!({"name": "zed"})).unwrap()["id"], 1, "pending: the same id");
        let pending = call(&pair.l_dir, "membership.pending", json!({})).unwrap();
        assert_eq!(pending.as_array().unwrap().len(), 1, "{pending}");
        assert_eq!((&pending[0]["from"], &pending[0]["op"]), (&json!("box"), &json!({"kind": "revoke", "name": "zed"})));
        assert_eq!(call(&pair.k_dir, "membership.pending", json!({})).unwrap(), json!([]), "queued on the authority only");
        // Never signed on the request alone.
        assert!(!test_support::read(&l_toml).unwrap().log.revoked_name("zed"));

        assert_eq!(call(&pair.l_dir, "membership.deny", json!({"id": 1})).unwrap(), json!({"ok": true}));
        assert_eq!(call(&pair.l_dir, "membership.pending", json!({})).unwrap(), json!([]));
        assert_eq!(call(&pair.l_dir, "membership.deny", json!({"id": 1})).unwrap_err().0, "not-found");
        assert_eq!(call(&pair.l_dir, "membership.approve", json!({"id": 1})).unwrap_err().0, "not-found");

        let r = call(&pair.k_dir, "machines.rm", json!({"name": "zed"})).unwrap();
        assert_eq!(r["id"], 2);
        // Only the authority approves.
        assert_eq!(call(&pair.k_dir, "membership.approve", json!({"id": 2})).unwrap_err().0, "not-found");
        let mut approved = call(&pair.l_dir, "membership.approve", json!({"id": 2})).unwrap();
        crate::serve::dts::assert_matches("MembershipApproved", &approved);
        let missed = approved["removed"]["missed"].take();
        assert_eq!(missed.as_array().unwrap().iter().map(|m| m["name"].clone()).collect::<Vec<_>>(), [json!("x")]);
        assert_eq!(
            approved,
            json!({
                "id": 2, "name": "zed", "change": "revoke `zed`", "pushed": ["box"], "unpushed": [], "error": null,
                "removed": {
                    "name": "zed", "hub": "box", "leave": "unreachable",
                    "error": "no link to it, and no path to it in this host's link graph", "forgot": ["lap"], "missed": null,
                },
            })
        );
        assert!(test_support::read(&l_toml).unwrap().log.revoked_name("zed"));
        assert!(test_support::read(&k_toml).unwrap().log.revoked_name("zed"), "the push reached the hub");
        assert_eq!(call(&pair.l_dir, "membership.pending", json!({})).unwrap(), json!([]));

        // Typed on the authority: signed at once.
        let r = call(&pair.l_dir, "machines.rm", json!({"name": "yan"})).unwrap();
        assert_eq!((&r["type"], &r["pushed"], &r["removed"]["forgot"]), (&json!("revoked"), &json!(["box"]), &json!(["lap"])));
        assert!(test_support::read(&k_toml).unwrap().log.revoked_name("yan"));
        // Neither the hub nor the authority goes this way.
        let e = call(&pair.k_dir, "machines.rm", json!({"name": "lap"})).unwrap_err();
        assert_eq!(e, ("denied".into(), "`lap` is the fleet's authority: it can't be removed".into()));
        assert_eq!(call(&pair.l_dir, "machines.rm", json!({"name": "box"})).unwrap_err().0, "denied");
        pair.finish();
    }

    /// The fleet's authority (`auth`) runs nowhere here, no link nor path
    /// to it: a member's `machine rm` fails naming it and why, and nothing
    /// is queued anywhere. (A route to it: `fleet::dialer`'s
    /// `a_proposal_reaches_the_authority_over_a_route`.)
    #[test]
    fn a_proposal_needs_a_path_to_the_authority() {
        let pair = pair("changes-unlinked", |k, l| {
            let auth = SigningKey::from_seed([9; 32]);
            test_support::fleet("auth", &auth, "box", &[("lap", key_of(l).public_key()), ("box", key_of(k).public_key()), ("zed", pk(7))])
        });
        up(&pair);
        for dir in [&pair.l_dir, &pair.k_dir] {
            let (code, message) = call(dir, "machines.rm", json!({"name": "zed"})).unwrap_err();
            assert_eq!(code, "failed", "{message}");
            assert_eq!(message, "the fleet's authority `auth`: no link to it, and no path to it in this host's link graph");
        }
        assert!(!pair.base.join("l-data/proposals.json").exists() && !pair.base.join("k-data/proposals.json").exists());
        pair.finish();
    }

    /// `box` (K) is only introduced (by `pi`, a member never running) into
    /// `lap`'s fleet, `lap` (L) the authority and the hub: the link comes up
    /// with box capped, and its `fleet.propose` to L is refused.
    #[test]
    fn a_capped_peer_proposes_nothing() {
        let pair = Pair::build("changes-capped", Vec::new(), |_| {}, |_| {}, |k, l| {
            let pi = SigningKey::from_seed([3; 32]);
            let mut s = test_support::fleet("lap", &key_of(l), "lap", &[("pi", pi.public_key())]);
            let g = s.log.membership().unwrap().genesis;
            s.introduce(Introduction::sign(&g, "box", key_of(k).public_key(), "pi", &pi)).unwrap();
            write_fleet(&s, &[(k, "box", "lap"), (l, "lap", "self")]);
        });
        pair.wait_box("box connected", connected);
        wait_for("lap connected on K", Duration::from_secs(10), || lap_on_k(&pair).is_some_and(|r| connected(&r)));
        let mut lap = api_to(&pair.k_dir, "lap");
        let e = lap.call("fleet.propose", json!({"op": {"kind": "revoke", "name": "pi"}})).unwrap().error.unwrap();
        assert!(e.code == "denied" && e.message.contains("capped"), "{e:?}");
        // So is K's `machine rm`, which proposes over that link.
        let (code, message) = call(&pair.k_dir, "machines.rm", json!({"name": "pi"})).unwrap_err();
        assert!(code == "failed" && message.contains("capped"), "{code}: {message}");
        // It has no log key to rotate: refused before a next key is made.
        let (code, message) = call(&pair.k_dir, "membership.rotate", json!({})).unwrap_err();
        assert!(code == "denied" && message.contains("introduced by `pi`") && message.contains("no entry to rotate"), "{code}: {message}");
        assert!(!pair.base.join("k-data/member.key.next").exists());
        assert_eq!(call(&pair.l_dir, "membership.pending", json!({})).unwrap(), json!([]));
        drop(lap);
        pair.finish();
    }

    /// `lap` (L), the authority, rotates its own key: signed with the old
    /// key, the next key promoted, pushed to K (`box`, the hub), which takes
    /// the new authority key; then L ends its links, and the redial proves
    /// the new key. L's next entry (a revoke of `yan`), signed with it, is
    /// taken by K, its `fleet.rm` admitted there.
    #[test]
    fn the_authority_rotates_its_own_key() {
        let pair = pair("changes-rotate-auth", |k, l| test_support::fleet("lap", &key_of(l), "box", &[("box", key_of(k).public_key()), ("yan", pk(8))]));
        up(&pair);
        let (k_toml, l_toml) = (pair.base.join("k-data/member.toml"), pair.base.join("l-data/member.toml"));
        let old = key_of(&l_toml).public_key();
        let dials = pair.dialed.count.load(std::sync::atomic::Ordering::SeqCst);
        let r = call(&pair.l_dir, "membership.rotate", json!({})).unwrap();
        crate::serve::dts::assert_variant("MembershipRotated", &r);
        let new: PublicKey = serde_json::from_value(r["key"].clone()).unwrap();
        assert_ne!(new, old);
        assert_eq!((&r["change"], &r["pushed"], &r["fingerprint"]), (&json!("rotate `lap`'s key"), &json!(["box"]), &json!(new.fingerprint())));
        assert_eq!(key_of(&l_toml).public_key(), new, "promoted");
        assert!(!pair.base.join("l-data/member.key.next").exists());
        for toml in [&l_toml, &k_toml] {
            let s = test_support::read(toml).unwrap();
            let m = s.log.membership().unwrap();
            assert_eq!((m.authority.1, m.key_of("lap"), m.revoked.contains(&old)), (new, Some(new), true));
        }
        assert_eq!(call(&pair.l_dir, "member.get", json!({})).unwrap()["next"], Value::Null);

        // L ended its links after the push: the redial proves the new key.
        pair.wait_box("box redialed", |r| connected(r) && pair.dialed.count.load(std::sync::atomic::Ordering::SeqCst) > dials);
        wait_for("lap connected on K", Duration::from_secs(10), || lap_on_k(&pair).is_some_and(|r| connected(&r)));
        let r = call(&pair.l_dir, "machines.rm", json!({"name": "yan"})).unwrap();
        assert_eq!((&r["type"], &r["pushed"], &r["removed"]["forgot"]), (&json!("revoked"), &json!(["box"]), &json!(["lap"])), "{r}");
        let k_log = test_support::read(&k_toml).unwrap();
        assert!(k_log.log.revoked_name("yan"));
        assert_eq!(k_log.log.entries().last().unwrap().statement.key, new, "signed with the new key");
        assert_eq!(k_log.log.head(), test_support::read(&l_toml).unwrap().log.head());
        pair.finish();
    }

    /// A head moved or added is a move; one dropped or none changed isn't.
    #[test]
    fn a_config_head_moves() {
        let heads = |list: &[(&str, u64, u8)]| list.iter().map(|(n, v, h)| (n.to_string(), (*v, [*h; 32]))).collect();
        let before = heads(&[("a", 1, 1), ("b", 1, 2)]);
        assert!(!config_moved(&before, &before));
        assert!(!config_moved(&before, &heads(&[("a", 1, 1)])));
        assert!(config_moved(&before, &heads(&[("a", 2, 3), ("b", 1, 2)])));
        assert!(config_moved(&before, &heads(&[("a", 1, 1), ("b", 1, 2), ("c", 1, 4)])));
        assert!(config_moved(&heads(&[]), &heads(&[("a", 1, 1)])));
    }

    /// A verb `method` with `params` on the daemon at `dir`: whether it
    /// was answered (signing runs before the verb, so it's done by then).
    fn verb_answered(dir: &Path, method: &str, params: Value) -> bool {
        verb_answer(dir, method, params).is_some()
    }

    /// [`verb_answered`]'s answer: `None` unanswered, else its error's
    /// `(code, message)` if any.
    fn verb_answer(dir: &Path, method: &str, params: Value) -> Option<Option<(String, String)>> {
        verb_notes(dir, method, params).0
    }

    /// [`verb_answer`], with the `note:` lines its `stderr` output carried.
    fn verb_notes(dir: &Path, method: &str, params: Value) -> (Option<Option<(String, String)>>, Vec<String>) {
        let mut notes = Vec::new();
        let streamed = connect(dir)
            .call_streaming(method, params, Duration::from_secs(1), &mut |n| {
                if let Some(n) = n.filter(|n| n.method == "output" && n.params["stream"] == "stderr") {
                    let data = n.params["data"].as_str().unwrap_or_default();
                    notes.extend(data.lines().filter(|l| l.starts_with("note: ")).map(str::to_string));
                }
                true
            })
            .unwrap();
        let answer = match streamed {
            crate::serve::client::Streamed::Answer(r) => Some(r.error.map(|e| (e.code, e.message))),
            _ => None,
        };
        (answer, notes)
    }

    /// A sign's notes: the version, then each machine that doesn't have
    /// it and why; a failure's error; a non-authority naming the authority.
    /// Logged: the sign, the missed machines in one line, a failure.
    #[test]
    fn config_signed_notes() {
        let signed = ConfigSigned::Signed { root: "cfg".into(), version: 3, unpushed: vec![] };
        assert_eq!(signed.notes(), ["note: signed config `cfg` v3"]);
        assert_eq!(signed.log_lines(), ["signed config `cfg` v3"]);
        let missed = vec![Missed { name: "box".into(), error: "down".into() }, Missed { name: "pi".into(), error: "timed out".into() }];
        let signed = ConfigSigned::Signed { root: "cfg".into(), version: 3, unpushed: missed };
        assert_eq!(
            signed.notes(),
            [
                "note: signed config `cfg` v3",
                "note: `box` doesn't have config `cfg` v3: down",
                "note: `pi` doesn't have config `cfg` v3: timed out",
            ]
        );
        assert_eq!(signed.log_lines(), ["signed config `cfg` v3", "config `cfg` v3 not pushed to box (down), pi (timed out)"]);
        let failed = ConfigSigned::Failed { root: "cfg".into(), error: "bad toml".into() };
        assert_eq!(failed.notes(), ["note: config `cfg` not signed: bad toml"]);
        assert_eq!(failed.log_lines(), ["config `cfg` not signed: bad toml"]);
        let other = ConfigSigned::NotAuthority { root: "cfg".into(), authority: "lap".into() };
        assert_eq!(
            other.notes(),
            ["note: config `cfg` not signed: the fleet's authority is `lap`, not this host, and configs here aren't signed"]
        );
        assert_eq!(other.log_lines(), Vec::<String>::new());
    }

    /// `lap` (L), the authority: a `start` typed there signs its root's
    /// `config` v1 first, its bundle stored, and pushes it to K (`box`); the
    /// same structure (a comment, a note in the root) signs nothing; a
    /// structural edit is v2, signed by a `rebuild` as by `start`. A config
    /// that won't load signs nothing, the verb answered anyway. K's verbs
    /// (not the authority), `stop`, a relative root and a root with no
    /// config file sign nothing. The first sign binds `cfg` to the root's
    /// path; another root named `cfg` is refused, nothing signed. A verb
    /// for machine `box` signs the local root bound to its remote root's
    /// name; a name with no binding, nothing.
    #[test]
    fn the_authoritys_verbs_sign_config_versions() {
        let pair = pair("changes-config", |k, l| test_support::fleet("lap", &key_of(l), "box", &[("box", key_of(k).public_key())]));
        up(&pair);
        let (k_toml, l_toml) = (pair.base.join("k-data/member.toml"), pair.base.join("l-data/member.toml"));
        let root = pair.base.join("cfg");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("devsandboxes.toml"), "[sandbox.a]\n").unwrap();
        let params = json!({"root": root, "env": {}, "instance": "nope"});
        let config = |toml: &Path| test_support::read(toml).unwrap().log.membership().unwrap().config.get("cfg").map(|(v, h)| (*v, hex::encode(h)));

        let head = |toml: &Path| test_support::read(toml).unwrap().log.head();

        let (answered, notes) = verb_notes(&pair.l_dir, "start", params.clone());
        assert!(answered.is_some());
        assert_eq!(notes, ["note: signed config `cfg` v1"], "said in the verb's output");
        let v1 = config(&l_toml);
        assert_eq!(v1.as_ref().map(|(v, h)| (*v, h.len())), Some((1, 64)), "{v1:?}");
        assert_eq!(config(&k_toml), v1, "signed and pushed");
        let signed = head(&l_toml);
        assert_eq!(head(&k_toml), signed);
        let bundle = |v: &Option<(u64, String)>| pair.base.join(format!("l-data/mesh/bundles/{}.tar", v.as_ref().unwrap().1));
        assert!(bundle(&v1).is_file(), "the bundle stored");
        let l_data = pair.base.join("l-data");
        assert_eq!(mesh::bound_root(&l_data, "cfg").unwrap(), Some(root.canonicalize().unwrap()), "bound on first sign");
        let twin = pair.base.join("twin/cfg");
        std::fs::create_dir_all(&twin).unwrap();
        std::fs::write(twin.join("devsandboxes.toml"), "[sandbox.twin]\n").unwrap();
        assert!(verb_answered(&pair.l_dir, "start", json!({"root": twin, "env": {}, "instance": "nope"})));
        assert_eq!((head(&l_toml), config(&l_toml)), (signed.clone(), v1.clone()), "another root named `cfg`: refused");
        assert_eq!(mesh::bound_root(&l_data, "cfg").unwrap(), Some(root.canonicalize().unwrap()));

        std::fs::write(root.join("devsandboxes.toml"), "# a comment\n[sandbox.a]\n").unwrap();
        std::fs::write(root.join("notes.md"), "notes").unwrap();
        std::fs::write(bundle(&v1), b"damaged").unwrap();
        let (answered, notes) = verb_notes(&pair.l_dir, "start", params.clone());
        assert_ne!(std::fs::read(bundle(&v1)).unwrap(), b"damaged", "a damaged stored bundle rewritten though nothing signs");
        assert!(answered.is_some());
        assert_eq!(head(&l_toml), signed, "unchanged structure: nothing signed");
        assert_eq!(notes, Vec::<String>::new(), "nothing signed, nothing said");
        assert!(verb_answered(&pair.l_dir, "stop", params.clone()));
        assert_eq!(head(&l_toml), signed, "stop never signs");
        assert!(verb_answered(&pair.l_dir, "start", json!({"root": "cfg", "env": {}, "instance": "nope"})));
        assert_eq!(head(&l_toml), signed, "a relative root never signs");
        let bare = pair.base.join("bare");
        std::fs::create_dir_all(&bare).unwrap();
        assert!(verb_answered(&pair.l_dir, "start", json!({"root": bare, "env": {}, "instance": "nope"})));
        assert_eq!(head(&l_toml), signed, "no config file: nothing signed");
        std::fs::write(root.join("devsandboxes.toml"), "[sandbox.b]\n").unwrap();
        let (answered, notes) = verb_notes(&pair.k_dir, "start", params.clone());
        assert!(answered.is_some());
        assert_eq!((head(&k_toml), config(&k_toml)), (signed, v1.clone()), "K isn't the authority");
        assert_eq!(
            notes,
            ["note: config `cfg` not signed: the fleet's authority is `lap`, not this host, and configs here aren't signed"]
        );
        assert_eq!(verb_notes(&pair.k_dir, "start", params.clone()).1, Vec::<String>::new(), "once per root");
        let for_lap = |root: &str| json!({"root": root, "env": {}, "instance": "nope", "machine": "lap"});
        let (answered, notes) = verb_notes(&pair.k_dir, "start", for_lap("/far/other"));
        assert!(answered.is_some());
        assert_eq!(
            notes,
            ["note: config `other` not signed: the fleet's authority is `lap`, not this host, and configs here aren't signed"],
            "a verb for a machine, no binding here: told too"
        );
        assert_eq!(verb_notes(&pair.k_dir, "start", for_lap("/elsewhere/other")).1, Vec::<String>::new(), "once per root name");
        assert!(verb_answered(&pair.l_dir, "rebuild", json!({"root": root, "env": {}, "instance": "nope"})));
        let v2 = config(&l_toml).unwrap();
        assert_eq!(v2.0, 2, "the bound path signs the next version");
        assert_ne!(Some(&v2), v1.as_ref());
        assert_eq!(config(&k_toml), Some(v2.clone()));
        assert!(bundle(&Some(v2.clone())).is_file());

        std::fs::write(root.join("devsandboxes.toml"), "[sandbox.d]\n").unwrap();
        let signed = head(&l_toml);
        assert!(verb_answered(&pair.l_dir, "start", json!({"root": "/far/other", "env": {}, "instance": "nope", "machine": "box"})));
        assert_eq!(head(&l_toml), signed, "a remote root's name with no binding signs nothing");
        let (answered, notes) = verb_notes(&pair.l_dir, "start", json!({"root": "/far/cfg", "env": {}, "instance": "nope", "machine": "box"}));
        assert!(answered.is_some());
        assert_eq!(notes, ["note: signed config `cfg` v3"], "a remote verb's output says it too");
        let v3 = config(&l_toml).unwrap();
        assert_eq!(v3.0, 3, "a verb for a machine signs the root bound to its root's name");
        assert_eq!(config(&k_toml), Some(v3.clone()));
        let v2 = v3;

        std::fs::write(root.join("devsandboxes.toml"), "[sandbox.c\n").unwrap();
        let signed = head(&l_toml);
        let (answered, notes) = verb_notes(&pair.l_dir, "start", params);
        assert!(answered.is_some(), "the verb runs anyway");
        assert!(notes.len() == 1 && notes[0].starts_with("note: config `cfg` not signed: "), "{notes:?}");
        assert_eq!((head(&l_toml), config(&l_toml)), (signed, Some(v2)), "won't load: nothing signed");
        pair.finish();
    }

    /// `lap` (L), the authority, signs `cfg` v1 then v2 (a structural edit
    /// and a referenced file): K (`box`) takes each with the `fleet.sync`
    /// push, pulls its bundle from L over the link and installs it at
    /// `mesh/tree/cfg/`, `installed.toml` saying so, before it answers: by
    /// the time L's verb is answered, K's copy is the head. L, whose name it is,
    /// gets no copy. The copy is one of K's roots (`roots.list`); a verb
    /// that builds from it runs there while it is the head (here the test
    /// runner then fails), and is refused once it was changed in place,
    /// `stop` still running.
    #[test]
    fn a_member_installs_each_signed_version() {
        let pair = pair("changes-install", |k, l| test_support::fleet("lap", &key_of(l), "box", &[("box", key_of(k).public_key())]));
        up(&pair);
        let l_toml = pair.base.join("l-data/member.toml");
        let (k_data, l_data) = (pair.base.join("k-data"), pair.base.join("l-data"));
        let root = pair.base.join("cfg");
        std::fs::create_dir_all(root.join("mcps")).unwrap();
        std::fs::write(root.join("devsandboxes.toml"), "[sandbox.a]\n").unwrap();
        let params = json!({"root": root, "env": {}, "instance": "nope"});
        let signed = || test_support::read(&l_toml).unwrap().log.membership().unwrap().config.get("cfg").copied().unwrap();
        let tree = mesh::tree_path(&k_data, "cfg");
        let at = |hash: [u8; 32]| {
            let (tree, k_data) = (tree.clone(), k_data.clone());
            move || {
                mesh::installed(&k_data).unwrap().get("cfg") == Some(&hex::encode(hash))
                    && mesh::collect(&tree).is_ok_and(|b| b.hash() == hash)
            }
        };

        assert!(verb_answered(&pair.l_dir, "start", params.clone()));
        let (v, h1) = signed();
        assert_eq!(v, 1);
        assert!(at(h1)(), "v1 installed on K once the push is answered");
        assert_eq!(mesh::verify(&k_data, "cfg", Some((1, h1))).map_err(|e| format!("{e:#}")), Ok(()));
        assert_eq!(std::fs::read_to_string(tree.join("devsandboxes.toml")).unwrap(), "[sandbox.a]\n");

        std::fs::write(
            root.join("devsandboxes.toml"),
            "[sandbox.b]\nmounts = [\"source=${configDir}/mcps/x.json,target=/x.json,type=bind\"]\n",
        )
        .unwrap();
        std::fs::write(root.join("mcps/x.json"), "{}").unwrap();
        assert!(verb_answered(&pair.l_dir, "start", params));
        let (v, h2) = signed();
        assert_eq!(v, 2);
        assert!(at(h2)(), "v2 installed on K once the push is answered");
        assert_eq!(std::fs::read_to_string(tree.join("mcps/x.json")).unwrap(), "{}");
        assert!(!mesh::tree_path(&l_data, "cfg").exists() && mesh::installed(&l_data).unwrap().is_empty(), "the authority's own: no copy");

        let copy = tree.canonicalize().unwrap();
        let rows = call(&pair.k_dir, "roots.list", json!({})).unwrap();
        assert!(rows.as_array().unwrap().iter().any(|r| r["root"] == json!(copy)), "{rows}");
        let on_copy = json!({"root": tree, "env": {}, "instance": "nope"});
        // A file the bundle doesn't list isn't the config.
        std::fs::write(tree.join("notes.md"), "notes").unwrap();
        for method in ["start", "rebuild"] {
            let (code, message) = verb_answer(&pair.k_dir, method, on_copy.clone()).unwrap().expect("the test runner fails");
            assert!(code != "denied" && !message.contains("config root"), "{method}: admitted past the check: {code}: {message}");
        }
        std::fs::write(tree.join("mcps/x.json"), "{\"evil\":1}").unwrap();
        let refused = verb_answer(&pair.k_dir, "start", on_copy.clone()).unwrap();
        assert_eq!(refused, Some(("denied".into(), "config root `cfg` isn't the signed config (v2): its copy was changed here".into())));
        let (code, _) = verb_answer(&pair.k_dir, "stop", on_copy.clone()).unwrap().expect("the test runner fails");
        assert_ne!(code, "denied", "stop runs on a changed copy");
        let inside = tree.join("mcps");
        let (code, message) = verb_answer(&pair.k_dir, "start", json!({"root": inside, "env": {}, "instance": "nope"})).unwrap().unwrap();
        assert!(code == "denied" && message.contains("inside the copy"), "{code}: {message}");
        pair.finish();
    }

    /// A `fleet.sync` to K (`box`) moving `cfg`'s head, sent over L's link
    /// (as L's push sends it, the bundle stored on L, L's own log left as
    /// is): answered only once K pulled the bundle from L and installed it.
    #[test]
    fn a_sync_is_answered_once_its_config_is_installed() {
        let pair = pair("changes-sync-install", |k, l| test_support::fleet("lap", &key_of(l), "box", &[("box", key_of(k).public_key())]));
        up(&pair);
        let l_toml = pair.base.join("l-data/member.toml");
        let (k_data, l_data) = (pair.base.join("k-data"), pair.base.join("l-data"));
        let root = pair.base.join("cfg");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("devsandboxes.toml"), "[sandbox.a]\n").unwrap();
        let bundle = mesh::collect(&root).unwrap();
        mesh::store(&l_data, &bundle).unwrap();
        let mut stored = test_support::read(&l_toml).unwrap();
        stored.log.append(Op::Config { root: "cfg".into(), version: 1, hash: bundle.hash() }, "lap", &key_of(&l_toml)).unwrap();

        let r = api_to(&pair.l_dir, "box").call("fleet.sync", serde_json::to_value(stored.to_wire()).unwrap()).unwrap();
        assert!(r.error.is_none(), "{r:?}");
        assert_eq!(r.result.unwrap()["changed"], json!(true));
        assert_eq!(mesh::verify(&k_data, "cfg", Some((1, bundle.hash()))).map_err(|e| format!("{e:#}")), Ok(()), "installed by the answer");
        pair.finish();
    }

    /// K (`box`, a member and the hub) rotates its key: proposed to L
    /// (`lap`, the authority), the same next key and id when run again,
    /// shown by `member.get`; approved on L, pushed to K, which promotes it
    /// and ends its links: the redial proves the new key, and K proposes
    /// over it.
    /// A proposal to rotate another member's key, or with a bad
    /// countersignature, is refused.
    #[test]
    fn a_members_rotation_is_proposed_approved_and_promoted() {
        let pair = pair("changes-rotate-member", |k, l| test_support::fleet("lap", &key_of(l), "box", &[("box", key_of(k).public_key()), ("zed", pk(7))]));
        up(&pair);
        let (k_toml, l_toml) = (pair.base.join("k-data/member.toml"), pair.base.join("l-data/member.toml"));
        let next_file = pair.base.join("k-data/member.key.next");
        let old = key_of(&k_toml);

        let r = call(&pair.k_dir, "membership.rotate", json!({})).unwrap();
        crate::serve::dts::assert_variant("MembershipRotateProposed", &r);
        let new: PublicKey = serde_json::from_value(r["key"].clone()).unwrap();
        assert_eq!((&r["name"], &r["authority"], &r["id"]), (&json!("box"), &json!("lap"), &json!(1)));
        assert!(next_file.exists());
        assert_eq!(key_of(&k_toml).public_key(), old.public_key(), "the old key until the rotate is in");
        let again = call(&pair.k_dir, "membership.rotate", json!({})).unwrap();
        assert_eq!((&again["key"], &again["id"]), (&r["key"], &json!(1)), "the pending key, the same proposal");
        let got = call(&pair.k_dir, "member.get", json!({})).unwrap();
        assert_eq!((&got["key"], &got["next"]["key"]), (&json!(old.public_key()), &json!(new)));
        assert_eq!(call(&pair.k_dir, "membership.get", json!({})).unwrap()["next"]["fingerprint"], json!(new.fingerprint()));
        let pending = call(&pair.l_dir, "membership.pending", json!({})).unwrap();
        assert_eq!(pending.as_array().unwrap().len(), 1, "{pending}");
        assert_eq!((&pending[0]["from"], &pending[0]["op"]["kind"], &pending[0]["op"]["new"]), (&json!("box"), &json!("rotate"), &json!(new)));
        crate::serve::dts::assert_matches("FleetChangeRotate", &pending[0]["op"]);

        // Over K's link to L: another member's key, a bad countersignature.
        let g = test_support::read(&l_toml).unwrap().log.membership().unwrap().genesis;
        let mut lap = api_to(&pair.k_dir, "lap");
        let lap_key = key_of(&l_toml).public_key();
        let theirs = Change::Rotate { name: "lap".into(), old: lap_key, new: pk(9), by_old: countersign_rotate(&g, "lap", &pk(9), &old) };
        let e = lap.call("fleet.propose", json!({"op": theirs})).unwrap().error.unwrap();
        assert!(e.code == "invalid" && e.message.contains("rotates only its own key"), "{e:?}");
        let bad = Change::Rotate { name: "box".into(), old: old.public_key(), new: pk(9), by_old: countersign_rotate(&g, "box", &pk(9), &SigningKey::from_seed([9; 32])) };
        let e = lap.call("fleet.propose", json!({"op": bad})).unwrap().error.unwrap();
        assert!(e.code == "invalid" && e.message.contains("isn't countersigned"), "{e:?}");
        drop(lap);
        assert_eq!(call(&pair.l_dir, "membership.pending", json!({})).unwrap().as_array().unwrap().len(), 1);

        let dials = pair.dialed.count.load(std::sync::atomic::Ordering::SeqCst);
        let approved = call(&pair.l_dir, "membership.approve", json!({"id": 1})).unwrap();
        crate::serve::dts::assert_matches("MembershipApproved", &approved);
        assert_eq!((&approved["change"], &approved["name"], &approved["pushed"]), (&json!("rotate `box`'s key"), &json!("box"), &json!(["box"])));
        // The push reached K: it took the log and promoted its next key.
        assert_eq!(key_of(&k_toml).public_key(), new);
        assert!(!next_file.exists());
        for toml in [&l_toml, &k_toml] {
            let m = test_support::read(toml).unwrap().log.membership().unwrap().clone();
            assert_eq!((m.key_of("box"), m.revoked.contains(&old.public_key())), (Some(new), true));
        }
        let got = call(&pair.k_dir, "member.get", json!({})).unwrap();
        assert_eq!((&got["key"], &got["next"]), (&json!(new), &Value::Null));

        // K ended its links once it answered the sync: the redial proves
        // the new key, and K proposes over it.
        pair.wait_box("box redialed", |r| connected(r) && pair.dialed.count.load(std::sync::atomic::Ordering::SeqCst) > dials);
        wait_for("lap connected on K", Duration::from_secs(10), || lap_on_k(&pair).is_some_and(|r| connected(&r)));
        // A later rotate is another one: a new next key, a new proposal.
        let later = call(&pair.k_dir, "membership.rotate", json!({})).unwrap();
        assert_eq!(later["id"], 2);
        assert_ne!(later["key"], json!(new));
        call(&pair.l_dir, "membership.deny", json!({"id": 2})).unwrap();
        let r = call(&pair.k_dir, "machines.rm", json!({"name": "zed"})).unwrap();
        assert_eq!((&r["type"], &r["id"]), (&json!("proposed"), &json!(3)), "{r}");
        pair.finish();
    }

    /// `lap` the authority, `box` a member, `new` introduced by box (capped).
    #[test]
    fn rotation_is_planned_by_the_log() {
        let (lap, bx) = (SigningKey::from_seed([1; 32]), SigningKey::from_seed([2; 32]));
        let mut s = test_support::fleet("lap", &lap, "box", &[("box", bx.public_key())]);
        let g = s.log.membership().unwrap().genesis;
        s.introduce(Introduction::sign(&g, "new", pk(5), "box", &bx)).unwrap();
        let Ok(RotatePlan::Sign(op)) = plan_rotate(&s, "lap", &lap, &pk(9)) else { panic!("the authority signs") };
        assert!(matches!(&op, Op::Rotate { name, old, new, .. } if name == "lap" && *old == lap.public_key() && *new == pk(9)));
        let Ok(RotatePlan::Propose { authority, change }) = plan_rotate(&s, "box", &bx, &pk(9)) else { panic!("a member proposes") };
        assert_eq!((authority.as_str(), change.subject()), ("lap", "box"));
        // A capped member, one not in the log, one in no fleet.
        let e = can_rotate(Some(&s), "new", &pk(5)).unwrap_err();
        assert!(e.code == "denied" && e.message.contains("introduced by `box`"), "{}", e.message);
        assert_eq!(can_rotate(Some(&s), "box", &pk(6)).unwrap_err().code, "denied");
        assert_eq!(can_rotate(None, "box", &bx.public_key()).unwrap_err().code, "failed");
        // A next key the log knows.
        assert_eq!(plan_rotate(&s, "box", &bx, &lap.public_key()).err().unwrap().code, "invalid");
    }

    /// `lap` the authority, `box` the hub, `pi` a member, `new` introduced by
    /// box (capped).
    #[test]
    fn a_hub_move_is_planned_by_the_log() {
        let (lap, bx) = (SigningKey::from_seed([1; 32]), SigningKey::from_seed([2; 32]));
        let mut s = test_support::fleet("lap", &lap, "box", &[("box", bx.public_key()), ("pi", pk(3))]);
        let g = s.log.membership().unwrap().genesis;
        s.introduce(Introduction::sign(&g, "new", pk(5), "box", &bx)).unwrap();
        let Ok(RotatePlan::Sign(op)) = plan_hub(Some(&s), "lap", &lap.public_key(), "pi") else { panic!("the authority signs") };
        assert_eq!(op, Op::Hub { name: "pi".into() });
        let Ok(RotatePlan::Propose { authority, change }) = plan_hub(Some(&s), "box", &bx.public_key(), "lap") else { panic!("a member proposes") };
        assert_eq!((authority.as_str(), change), ("lap", Change::Hub { name: "lap".into() }));
        // The hub already, an introduced name, an unknown one: `invalid`.
        for name in ["box", "new", "zed"] {
            assert_eq!(plan_hub(Some(&s), "lap", &lap.public_key(), name).err().unwrap().code, "invalid", "{name}");
        }
        // Asked by a capped member, by a key the log doesn't list, in no fleet.
        let e = plan_hub(Some(&s), "new", &pk(5), "pi").err().unwrap();
        assert!(e.code == "denied" && e.message.contains("introduced by `box`"), "{}", e.message);
        assert_eq!(plan_hub(Some(&s), "box", &pk(6), "pi").err().unwrap().code, "denied");
        assert_eq!(plan_hub(None, "box", &bx.public_key(), "pi").err().unwrap().code, "failed");
    }

    /// The `hub` field of the `member.toml` at `toml`.
    fn hub_field(toml: &Path) -> String {
        crate::serve::fleet::member::load(toml).unwrap().unwrap().hub
    }

    /// `lap` (L) the authority, `box` (K) the hub. `machine hub lap` on L is
    /// signed at once, pushed to K, then followed: both rewrite `hub` and
    /// the links redial. `machine hub box` on K is proposed to L, approved
    /// there, and the role moves back the same way. Naming the hub, or a
    /// name not in the log, is `invalid`.
    #[test]
    fn the_hub_role_moves_and_back() {
        let pair = pair("changes-hub", |k, l| test_support::fleet("lap", &key_of(l), "box", &[("box", key_of(k).public_key())]));
        up(&pair);
        let (k_toml, l_toml) = (pair.base.join("k-data/member.toml"), pair.base.join("l-data/member.toml"));
        let dials = || pair.dialed.count.load(std::sync::atomic::Ordering::SeqCst);
        let redialed = |before: usize| {
            pair.wait_box("box redialed", |r| connected(r) && dials() > before);
            wait_for("lap connected on K", Duration::from_secs(10), || lap_on_k(&pair).is_some_and(|r| connected(&r)));
        };
        for (dir, name) in [(&pair.l_dir, "box"), (&pair.k_dir, "box"), (&pair.l_dir, "zed")] {
            assert_eq!(call(dir, "membership.hub", json!({"name": name})).unwrap_err().0, "invalid", "{name}");
        }
        assert_eq!(call(&pair.l_dir, "membership.hub", json!({"name": "lap", "later": 1})).unwrap_err().0, "invalid");

        let before = dials();
        let r = call(&pair.l_dir, "membership.hub", json!({"name": "lap"})).unwrap();
        crate::serve::dts::assert_variant("MembershipHubSigned", &r);
        assert_eq!(r, json!({"type": "signed", "name": "lap", "change": "hub `lap`", "pushed": ["box"], "unpushed": [], "removed": null, "error": null}));
        assert_eq!((hub_field(&l_toml), hub_field(&k_toml)), ("self".to_string(), "lap".to_string()), "the push reached K before the links ended");
        for toml in [&l_toml, &k_toml] {
            assert_eq!(test_support::read(toml).unwrap().log.membership().unwrap().hub, "lap");
        }
        redialed(before);
        assert_eq!(call(&pair.l_dir, "membership.hub", json!({"name": "lap"})).unwrap_err().0, "invalid", "the hub already");

        // Both ends cycle (K once its answer is out, `AFTER_ANSWER` later):
        // the link L redialed first may end once more. A retry collapses
        // with a pending proposal.
        let mut r = Value::Null;
        wait_for("K's proposal reaches L", Duration::from_secs(10), || match call(&pair.k_dir, "membership.hub", json!({"name": "box"})) {
            Ok(v) => {
                r = v;
                true
            }
            Err((code, message)) => {
                assert_eq!(code, "failed", "{message}");
                false
            }
        });
        crate::serve::dts::assert_variant("MembershipHubProposed", &r);
        assert_eq!(r, json!({"type": "proposed", "name": "box", "authority": "lap", "id": 1, "error": null}));
        // The repeat may still land in that last cycle: retried the same way.
        let mut again = Value::Null;
        wait_for("K's repeat reaches L", Duration::from_secs(10), || match call(&pair.k_dir, "membership.hub", json!({"name": "box"})) {
            Ok(v) => {
                again = v;
                true
            }
            Err((code, message)) => {
                assert_eq!(code, "failed", "{message}");
                false
            }
        });
        assert_eq!(again["id"], 1, "pending: the same id");
        let pending = call(&pair.l_dir, "membership.pending", json!({})).unwrap();
        assert_eq!(pending.as_array().unwrap().len(), 1, "{pending}");
        assert_eq!((&pending[0]["from"], &pending[0]["op"]), (&json!("box"), &json!({"kind": "hub", "name": "box"})));
        crate::serve::dts::assert_matches("FleetChangeHub", &pending[0]["op"]);
        assert_eq!(hub_field(&l_toml), "self", "never signed on the request alone");

        let before = dials();
        let approved = call(&pair.l_dir, "membership.approve", json!({"id": 1})).unwrap();
        crate::serve::dts::assert_matches("MembershipApproved", &approved);
        assert_eq!((&approved["change"], &approved["name"], &approved["pushed"]), (&json!("hub `box`"), &json!("box"), &json!(["box"])));
        assert_eq!((hub_field(&l_toml), hub_field(&k_toml)), ("box".to_string(), "self".to_string()));
        assert_eq!(call(&pair.l_dir, "membership.pending", json!({})).unwrap(), json!([]));
        redialed(before);
        pair.finish();
    }

    /// Three daemons: K (`box`, the hub), L (`lap`, the authority) and P
    /// (`pi`), L and P each linked to K only. K's introduction of `new` is
    /// proposed to L and approved there; L pushes the `add` to K, which
    /// sends it on to P (not back to L). A config L signs is installed on K
    /// once L's push is answered, and on P from K.
    #[test]
    fn a_sync_floods_to_a_third_host() {
        let base = scratch("changes-flood");
        let (k_dir, l_dir, p_dir) = (base.join("k"), base.join("l"), base.join("p"));
        let (k_opts, _) = daemon_opts(&base, "k");
        let (mut l_opts, _) = daemon_opts(&base, "l");
        let (mut p_opts, _) = daemon_opts(&base, "p");
        let k_toml = k_opts.member.clone().unwrap();
        let (l_toml, p_toml) = (l_opts.member.clone().unwrap(), p_opts.member.clone().unwrap());
        let members = [("box", key_of(&k_toml).public_key()), ("pi", key_of(&p_toml).public_key())];
        let stored = test_support::fleet("lap", &key_of(&l_toml), "box", &members);
        write_fleet(&stored, &[(&k_toml, "box", "self"), (&l_toml, "lap", "box"), (&p_toml, "pi", "box")]);
        let mut dialed = Vec::new();
        for (opts, registry) in [(&mut l_opts, base.join("l-data/machines.toml")), (&mut p_opts, base.join("p-data/machines.toml"))] {
            crate::serve::machines::update(&registry, |e| e.push(Entry::ssh("box", "me@box"))).unwrap();
            let (transport, d) = peer_transport(&k_dir, &k_toml);
            opts.link_transport = Some(transport);
            dialed.push(d);
        }
        let mut daemons = vec![spawn_daemon(&k_dir, k_opts)];
        drop(connect(&k_dir));
        for (dir, opts) in [(&l_dir, l_opts), (&p_dir, p_opts)] {
            daemons.push(spawn_daemon(dir, opts));
            drop(connect(dir));
        }
        let on_k = |name: &str| {
            let rows = connect(&k_dir).call("machines.list", Value::Null).unwrap().result.unwrap();
            named(&rows, name).is_some_and(|r| connected(&r))
        };
        wait_for("lap and pi connected on K", Duration::from_secs(10), || on_k("lap") && on_k("pi"));

        // K's introduction of `new`, proposed to L over K's link to it.
        let g = stored.log.membership().unwrap().genesis;
        let i = Introduction::sign(&g, "new", pk(8), "box", &key_of(&k_toml));
        let mut lap = api_to(&k_dir, "lap");
        let r = lap.call("fleet.propose", json!({"op": {"kind": "add", "introduction": i}})).unwrap();
        assert!(r.error.is_none(), "{r:?}");
        let id = r.result.unwrap()["id"].clone();
        assert!(test_support::read(&p_toml).unwrap().log.membership().unwrap().key_of("new").is_none());
        let approved = call(&l_dir, "membership.approve", json!({"id": id})).unwrap();
        assert_eq!((&approved["change"], &approved["pushed"], &approved["removed"]), (&json!("add `new`"), &json!(["box"]), &Value::Null));
        wait_for("pi takes the add through box", Duration::from_secs(10), || {
            test_support::read(&p_toml).unwrap().log.membership().unwrap().key_of("new") == Some(pk(8))
        });
        assert_eq!(test_support::read(&k_toml).unwrap().log.head(), test_support::read(&l_toml).unwrap().log.head());

        // A config L signs: K installs it from L before answering, then
        // sends the log on; P pulls the bundle from K (never linked to L),
        // which has it by then.
        let root = base.join("cfg");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("devsandboxes.toml"), "[sandbox.a]\n").unwrap();
        assert!(verb_answered(&l_dir, "start", json!({"root": root, "env": {}, "instance": "nope"})));
        let (v, hash) = test_support::read(&l_toml).unwrap().log.membership().unwrap().config.get("cfg").copied().unwrap();
        let (k_data, p_data) = (base.join("k-data"), base.join("p-data"));
        assert_eq!(mesh::verify(&k_data, "cfg", Some((v, hash))).map_err(|e| format!("{e:#}")), Ok(()), "K installed before answering");
        wait_for("pi installs cfg from box", Duration::from_secs(20), || mesh::verify(&p_data, "cfg", Some((v, hash))).is_ok());

        drop(lap);
        for (dir, daemon) in [&p_dir, &l_dir, &k_dir].into_iter().zip(daemons.into_iter().rev()) {
            assert!(connect(dir).call("shutdown", json!({})).unwrap().error.is_none());
            assert_eq!(daemon.join().unwrap().unwrap(), Exit::Shutdown);
        }
        for d in dialed {
            d.cut();
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// K (`box`, the hub), L (`lap`, the authority) and P (`pi`), L and P
    /// each linked to K only. P is down while L signs `machine hub lap`
    /// (pushed to K alone); P's daemon starts later, behind: its link's
    /// proof carries the entry, P adopts it (`hub = "lap"`), the link ends
    /// since P's `Hello` named `box`, and the redial, saying `lap`, comes up
    /// (K, saying `lap` too, takes it: the same fleet).
    #[test]
    fn a_member_down_during_a_hub_move_follows_at_its_next_proof() {
        let base = scratch("changes-hub-late");
        let (k_dir, l_dir, p_dir) = (base.join("k"), base.join("l"), base.join("p"));
        let (k_opts, _) = daemon_opts(&base, "k");
        let (mut l_opts, _) = daemon_opts(&base, "l");
        let (mut p_opts, _) = daemon_opts(&base, "p");
        let k_toml = k_opts.member.clone().unwrap();
        let (l_toml, p_toml) = (l_opts.member.clone().unwrap(), p_opts.member.clone().unwrap());
        let members = [("box", key_of(&k_toml).public_key()), ("pi", key_of(&p_toml).public_key())];
        let stored = test_support::fleet("lap", &key_of(&l_toml), "box", &members);
        write_fleet(&stored, &[(&k_toml, "box", "self"), (&l_toml, "lap", "box"), (&p_toml, "pi", "box")]);
        let mut dialed = Vec::new();
        for (opts, registry) in [(&mut l_opts, base.join("l-data/machines.toml")), (&mut p_opts, base.join("p-data/machines.toml"))] {
            crate::serve::machines::update(&registry, |e| e.push(Entry::ssh("box", "me@box"))).unwrap();
            let (transport, d) = peer_transport(&k_dir, &k_toml);
            opts.link_transport = Some(transport);
            dialed.push(d);
        }
        let on = |dir: &Path, name: &str| {
            let rows = connect(dir).call("machines.list", Value::Null).unwrap().result.unwrap();
            named(&rows, name).is_some_and(|r| connected(&r))
        };
        let mut daemons = vec![spawn_daemon(&k_dir, k_opts)];
        drop(connect(&k_dir));
        daemons.push(spawn_daemon(&l_dir, l_opts));
        drop(connect(&l_dir));
        wait_for("lap connected on K", Duration::from_secs(10), || on(&k_dir, "lap"));

        let r = call(&l_dir, "membership.hub", json!({"name": "lap"})).unwrap();
        assert_eq!((&r["type"], &r["pushed"]), (&json!("signed"), &json!(["box"])), "{r}");
        assert_eq!(hub_field(&k_toml), "lap", "K followed");
        assert_eq!(hub_field(&p_toml), "box", "P, down, missed it");

        daemons.push(spawn_daemon(&p_dir, p_opts));
        drop(connect(&p_dir));
        wait_for("box connected on P after a redial", Duration::from_secs(20), || {
            dialed[1].count.load(std::sync::atomic::Ordering::SeqCst) >= 2 && on(&p_dir, "box") && on(&k_dir, "pi")
        });
        assert_eq!(hub_field(&p_toml), "lap", "P's hub follows the log it took");
        assert_eq!(test_support::read(&p_toml).unwrap().log.head(), test_support::read(&l_toml).unwrap().log.head());
        assert_eq!(call(&p_dir, "member.get", json!({})).unwrap()["hub"], "lap");

        for (dir, daemon) in [&p_dir, &l_dir, &k_dir].into_iter().zip(daemons.into_iter().rev()) {
            assert!(connect(dir).call("shutdown", json!({})).unwrap().error.is_none());
            assert_eq!(daemon.join().unwrap().unwrap(), Exit::Shutdown);
        }
        for d in dialed {
            d.cut();
        }
        let _ = std::fs::remove_dir_all(&base);
    }
}
