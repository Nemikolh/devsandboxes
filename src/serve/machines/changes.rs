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
//!   authority as `fleet.propose` over a direct link ([`propose`]; routing
//!   comes with fleet plan step 10b), queued there (`fleet::proposals`)
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

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::api::{self, Ctx};
use super::leave::{self, After, Authority, Missed, Removed};
use super::service::Handle;
use crate::serve::api::{ApiError, parse};
use crate::serve::fleet::identity::{FileKeyStore, KeyStore, PublicKey, Signer, SigningKey};
use crate::serve::fleet::member::{self, Hub};
use crate::serve::fleet::membership::{Op, Stored, Wire, countersign_rotate};
use crate::serve::fleet::membership_api;
use crate::serve::fleet::proposals::{self, Change, Proposal};

type Answer = Result<Value, ApiError>;

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
/// `membership.pending|approve|deny|rotate`; `None` otherwise.
pub fn call(method: &str, params: Value, ctx: &Ctx) -> Option<Answer> {
    let answer = match method {
        "membership.rotate" => parse::<Empty>(params).and_then(|_| rotate(ctx)).and_then(to_value),
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
/// the adopt path; `hub` followed. What the store did: a `rotate` of this
/// host's own key promoted its next key (`Taken::rotated`), and the caller
/// ends the links once it has pushed.
fn sign(ctx: &Ctx, me: &str, mut stored: Stored, op: Op) -> Result<membership_api::Taken, ApiError> {
    let path = leave::member_toml(ctx)?;
    let (_, signer) = membership_api::own(path, &ctx.hostname)?;
    stored.log.append(op, me, &signer).map_err(|e| ApiError::invalid(format!("{e:#}")))?;
    let taken = membership_api::adopt(path, &ctx.hostname, &stored)?;
    (ctx.hub_changed)(taken.hub.clone());
    Ok(taken)
}

/// `stored` sent to every machine `links` holds but `except`
/// (`fleet.sync`): those it reached, and those it didn't, with why.
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
        match leave::call_on(links, &entry.name, "fleet.sync", params.clone()) {
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
    let stored = if stored.log.revoked_name(name) {
        stored
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
        sign(ctx, &me, stored, op)?.stored
    };
    let (pushed, unpushed) = ctx.links.as_ref().map(|l| push(l, &stored, None)).unwrap_or_default();
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
/// to an `add`, a member's `rotate` of its own key) signed, the log pushed.
fn sign_change(ctx: &Ctx, from: &str, change: &Change) -> Result<Signed, ApiError> {
    let (me, stored) = on_authority(ctx)?;
    let m = stored.log.membership().expect("on_authority checked");
    let op = proposals::check(m, from, change).map_err(ApiError::invalid)?;
    let what = op.to_string();
    let stored = sign(ctx, &me, stored, op)?.stored;
    let (pushed, unpushed) = ctx.links.as_ref().map(|l| push(l, &stored, None)).unwrap_or_default();
    Ok(Signed { name: change.subject().to_string(), change: what, pushed, unpushed, removed: None, error: None })
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

/// What `membership.rotate` does once [`can_rotate`] holds (pure): the
/// `rotate` of `me`'s key (`key`) to `next`, countersigned by `key`,
/// checked against the log; signed here on the authority, else proposed to
/// it.
pub enum RotatePlan {
    Sign(Op),
    Propose { authority: String, change: Change },
}

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
/// to the authority over a direct link ([`propose`], collapsing with a
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
            if taken.rotated {
                (ctx.key_rotated)();
            }
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

/// `change` proposed to the fleet's authority `authority` over this host's
/// direct link to it: its id there. `denied` naming it when there's no
/// direct link (routing comes with fleet plan step 10b), `failed` when the
/// link is down or the authority refuses.
pub fn propose(ctx: &Ctx, authority: &str, change: Change) -> Result<u64, ApiError> {
    let linked = ctx.links.as_ref().is_some_and(|l| l.entries().iter().any(|e| e.name == authority));
    if !linked {
        return Err(ApiError::denied(format!(
            "the fleet's authority `{authority}` isn't directly linked to this host: a proposal reaches it only over a \
             direct link until routing (fleet plan step 10b); run it on `{authority}`, or link to it"
        )));
    }
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
/// follows it). When it made this host's log newer: `hub` followed, and
/// the log sent on to every other machine this host holds, off the request
/// thread. When it promoted this host's next key (a rotate of it is in),
/// the log is sent on before answering, and every link ends once the
/// answer is out (`After::Rotated`), redialing to prove the new key.
fn sync(params: Value, from: &str, ctx: &Ctx) -> Result<(Value, Option<After>), ApiError> {
    let wire: Wire = parse(params)?;
    let theirs = Stored::from_wire(wire).map_err(|e| ApiError::invalid(format!("{e:#}")))?;
    let path = leave::member_toml(ctx)?;
    let taken = membership_api::sync(path, &ctx.hostname, &theirs)?;
    let head = taken.stored.log.head();
    let mut after = None;
    if taken.changed {
        if let Some(links) = ctx.links.clone() {
            if taken.rotated {
                // This host's key rotated: its links end once the answer is
                // out (`After::Rotated`), so the log goes on over them now,
                // before that.
                flood(&links, &taken.stored, from);
                after = Some(After::Rotated);
            } else {
                let (sender, stored) = (from.to_string(), taken.stored.clone());
                let thread = std::thread::Builder::new().name("serve-fleet-sync".into()).spawn(move || flood(&links, &stored, &sender));
                if let Err(e) = thread {
                    crate::serve::daemon::log(&format!("fleet.sync from {from}: cannot send it on: {e}"));
                }
            }
        } else if taken.rotated {
            after = Some(After::Rotated);
        }
        (ctx.hub_changed)(taken.hub);
    }
    Ok((to_value(Synced { changed: taken.changed, head })?, after))
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
        Change::Add { .. } | Change::Rotate { .. } => sign_change(ctx, &p.from, &p.op),
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
    use crate::serve::fleet::dialer::tests::{Pair, connect, connected, daemon_opts, lap_on_k, named, peer_transport, wait_for};
    use super::{Change, Op, RotatePlan, can_rotate, countersign_rotate, plan_rotate};
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
                "removed": {"name": "zed", "hub": "box", "leave": "unreachable", "error": "no link to it", "forgot": ["lap"], "missed": null},
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

    /// The fleet's authority (`auth`) runs nowhere here: a member's `machine
    /// rm` is refused naming it, and nothing is queued anywhere.
    #[test]
    fn a_proposal_needs_a_direct_link_to_the_authority() {
        let pair = pair("changes-unlinked", |k, l| {
            let auth = SigningKey::from_seed([9; 32]);
            test_support::fleet("auth", &auth, "box", &[("lap", key_of(l).public_key()), ("box", key_of(k).public_key()), ("zed", pk(7))])
        });
        up(&pair);
        for dir in [&pair.l_dir, &pair.k_dir] {
            let (code, message) = call(dir, "machines.rm", json!({"name": "zed"})).unwrap_err();
            assert_eq!(code, "denied", "{message}");
            assert!(message.contains("authority `auth` isn't directly linked") && message.contains("10b"), "{message}");
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

    /// Three daemons: K (`box`, the hub), L (`lap`, the authority) and P
    /// (`pi`), L and P each linked to K only. K's introduction of `new` is
    /// proposed to L and approved there; L pushes the `add` to K, which
    /// sends it on to P (not back to L).
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
}
