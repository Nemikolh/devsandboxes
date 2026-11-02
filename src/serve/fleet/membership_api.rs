//! The `membership.*` methods (docs/architecture/daemon/fleet.md,
//! *Membership log*; docs/reference/api.md): `membership.get`, `adopt` and
//! `sign` over this host's `membership.json` (`super::membership`). Local
//! only, like `member.*`: no link's trust table admits them
//! (`super::lent::admit`); the add-time probe reaches them on the machine
//! as a local client there (its ssh login is the user, decision 2).
//!
//! [`adopt_into`], [`can_sign`] and [`sign_into`] are the pure rules, which
//! `machines.add` also runs over both ends' logs before writing anything
//! (`machines::join::logs`); [`call`] does the I/O. An adopt writes
//! `member.toml`'s `hub` from the log it accepted (`member::set_fleet`): the
//! only way a hub is set.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::identity::{FileKeyStore, KeyStore, PublicKey, Signer, SigningKey};
use super::member::{self, Hub, check_member_name};
use super::membership::{self, Compare, Head, Introduction, Op, Stored, Wire};
use crate::serve::api::{ApiError, parse};

type Answer = Result<Value, ApiError>;

/// A key as `summary` shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyInfo {
    pub key: PublicKey,
    pub fingerprint: String,
}

/// A held introduction as `summary` shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntroInfo {
    pub name: String,
    pub introducer: String,
    /// The introduced key's fingerprint.
    pub fingerprint: String,
}

/// What a stored log says, for reading: all empty (`head` the empty log's)
/// in no fleet's log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub authority: Option<String>,
    pub hub: Option<String>,
    pub members: BTreeMap<String, KeyInfo>,
    pub introductions: Vec<IntroInfo>,
    pub head: Head,
}

/// `membership.get`'s answer (and `membership.adopt`'s, after it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MembershipInfo {
    /// The log and the introductions held, as stored; `None` in no fleet's
    /// log.
    pub stored: Option<Wire>,
    pub summary: Summary,
}

impl MembershipInfo {
    pub fn of(stored: Option<&Stored>) -> MembershipInfo {
        let m = stored.and_then(|s| s.log.membership());
        let key_info = |key: &PublicKey| KeyInfo { key: *key, fingerprint: key.fingerprint() };
        let summary = Summary {
            authority: m.map(|m| m.authority.0.clone()),
            hub: m.map(|m| m.hub.clone()),
            members: m.map(|m| m.members.iter().map(|(n, k)| (n.clone(), key_info(k))).collect()).unwrap_or_default(),
            introductions: stored
                .map(|s| {
                    s.introductions
                        .iter()
                        .map(|i| IntroInfo { name: i.name.clone(), introducer: i.introducer.clone(), fingerprint: i.key.fingerprint() })
                        .collect()
                })
                .unwrap_or_default(),
            head: m.map_or(Head::EMPTY, |m| m.head),
        };
        MembershipInfo { stored: stored.filter(|s| s.log.membership().is_some()).map(Stored::to_wire), summary }
    }

    /// `stored` verified (`None`: the empty log).
    pub fn verified(&self) -> anyhow::Result<Stored> {
        Stored::from_wire(self.stored.clone().unwrap_or_default())
    }
}

/// A host as the rules see it: its member name and key.
#[derive(Debug, Clone, Copy)]
pub struct Own<'a> {
    pub name: &'a str,
    pub key: PublicKey,
}

/// `stored` lists `name` as `key`: a member of its log, or held as an
/// introduction.
pub fn lists(stored: &Stored, name: &str, key: &PublicKey) -> bool {
    stored.log.membership().is_some_and(|m| m.key_of(name) == Some(*key))
        || stored.introductions.iter().any(|i| i.name == name && i.key == *key)
}

/// `ours` after taking `theirs` (pure): no log of ours takes theirs; ours
/// behind theirs takes its later entries; ours the same or ahead keeps ours;
/// another fleet's or a fork is `denied`. Their introductions the result
/// hasn't settled are held too (one that doesn't check refuses the whole).
/// The result must list `own` (a member, or introduced): a host never
/// takes a fleet that doesn't contain it.
pub fn adopt_into(ours: &Stored, theirs: &Stored, own: &Own) -> Result<Stored, ApiError> {
    if theirs.log.membership().is_none() {
        return Err(ApiError::invalid("no membership log to adopt"));
    }
    let mut next = ours.clone();
    if ours.log.membership().is_none() {
        next.log = theirs.log.clone();
    } else {
        match membership::compare(&ours.log, theirs.log.head()) {
            Compare::Behind => {
                let newer = theirs.log.entries_after(ours.log.head().len).to_vec();
                next.log.extend(newer).map_err(|e| ApiError::denied(format!("{e:#}")))?;
            }
            Compare::Same | Compare::Ahead => {}
            fork @ Compare::Fork { .. } => return Err(ApiError::denied(format!("{fork}: not adopted"))),
        }
    }
    let m = next.log.membership().expect("a log taken or kept").clone();
    m.drop_settled(&mut next.introductions);
    for i in &theirs.introductions {
        if m.settles(i) {
            continue;
        }
        next.introduce(i.clone())
            .map_err(|e| ApiError::denied(format!("introduction of `{}` by `{}`: {e:#}", i.name, i.introducer)))?;
    }
    if !lists(&next, own.name, &own.key) {
        return Err(ApiError::denied(format!(
            "this membership log doesn't list this host (`{}`, {}): a host takes only a fleet that contains it",
            own.name,
            own.key.fingerprint()
        )));
    }
    Ok(next)
}

/// What `own` signs to let a host in ([`can_sign`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signs {
    /// The authority: an `add` entry.
    Add,
    /// Another member of the log: an introduction (decision 46).
    Introduction,
    /// Already in, under this very key (added or introduced): nothing to
    /// sign, so an add that failed after the target signed can be retried.
    Already,
}

/// Whether `own`, holding `ours`, may let `name` (`key`) into its fleet,
/// and how (pure): the authority adds, a member of the log introduces; a
/// host only introduced (capped, decision 51) or in no fleet's log is
/// `denied`. `name` already in under `key` is [`Signs::Already`]; a name or
/// key the log or a held introduction has otherwise is `invalid`.
pub fn can_sign(ours: &Stored, own: &Own, name: &str, key: &PublicKey) -> Result<Signs, ApiError> {
    let Some(m) = ours.log.membership() else {
        return Err(ApiError::denied(format!("`{}` is in no fleet's membership log: it lets no one in", own.name)));
    };
    check_member_name(name).map_err(|e| ApiError::invalid(format!("member name `{name}`: {e:#}")))?;
    if m.key_of(name) == Some(*key) || ours.introductions.iter().any(|i| i.name == name && i.key == *key) {
        return Ok(Signs::Already);
    }
    if m.members.contains_key(name) || ours.introductions.iter().any(|i| i.name == name) {
        return Err(ApiError::invalid(format!("`{name}` is already in `{}`'s fleet", m.hub)));
    }
    if m.is_member_key(key) || m.revoked.contains(key) || ours.introductions.iter().any(|i| i.key == *key) {
        return Err(ApiError::invalid(format!("key {} is already known to `{}`'s fleet", key.fingerprint(), m.hub)));
    }
    if m.authority.0 == own.name && m.is_authority(&own.key) {
        return Ok(Signs::Add);
    }
    if m.key_of(own.name) == Some(own.key) {
        return Ok(Signs::Introduction);
    }
    if let Some(i) = ours.introductions.iter().find(|i| i.name == own.name && i.key == own.key) {
        return Err(ApiError::denied(format!(
            "`{}` is introduced by `{}`, not added by the authority `{}`: a capped member introduces no one",
            own.name, i.introducer, m.authority.0
        )));
    }
    Err(ApiError::denied(format!("`{}` (this key) isn't a member of its fleet's membership log", own.name)))
}

/// `ours` with `name` (`key`) let in by `own` signing with `signer`
/// (`own.key`'s): [`can_sign`]'s `add` appended, or its introduction held.
pub fn sign_into(ours: &Stored, own: &Own, signer: &impl Signer, name: &str, key: &PublicKey) -> Result<Stored, ApiError> {
    let signs = can_sign(ours, own, name, key)?;
    let mut next = ours.clone();
    let invalid = |e: anyhow::Error| ApiError::invalid(format!("{e:#}"));
    match signs {
        Signs::Add => {
            next.log.append(Op::Add { name: name.to_string(), key: *key }, own.name, signer).map_err(invalid)?;
        }
        Signs::Introduction => {
            let genesis = next.log.membership().expect("can_sign checked").genesis;
            next.introduce(Introduction::sign(&genesis, name, *key, own.name, signer)).map_err(invalid)?;
        }
        Signs::Already => {}
    }
    Ok(next)
}

/// This host's member name (`member.toml`, else the hostname's) and key
/// (`member.key`, made on first use).
pub fn own(member_toml: &Path, hostname: &str) -> Result<(String, SigningKey), ApiError> {
    let m = member::load(member_toml).map_err(ApiError::internal)?.unwrap_or_default();
    let name = m.name_or(hostname).map_err(|e| ApiError::failed(format!("{e:#}")))?;
    let key = FileKeyStore::new(member::key_path(member_toml)).signing_key().map_err(ApiError::internal)?;
    Ok((name, key))
}

/// This host's stored log (`None`: in no fleet's log).
pub fn load(member_toml: &Path) -> Result<Option<Stored>, ApiError> {
    membership::load(&membership::path(member_toml)).map_err(ApiError::internal)
}

/// `membership.adopt`: `theirs` taken ([`adopt_into`]) and stored, then
/// `member.toml`'s `hub` (and the name the log knows this host by) written
/// from the log. The answer, and the hub the daemon follows.
pub fn adopt(member_toml: &Path, hostname: &str, theirs: &Stored) -> Result<(MembershipInfo, Hub), ApiError> {
    let (name, key) = own(member_toml, hostname)?;
    let own = Own { name: &name, key: key.public_key() };
    let after = membership::update(&membership::path(member_toml), |cur| -> Result<Stored, ApiError> {
        *cur = adopt_into(cur, theirs, &own)?;
        Ok(cur.clone())
    })
    .map_err(ApiError::internal)??;
    let hub = membership::hub_for(after.log.membership().expect("adopted"), &name);
    member::set_fleet(member_toml, Some(&name), &hub).map_err(ApiError::internal)?;
    Ok((MembershipInfo::of(Some(&after)), hub))
}

/// `membership.sign`: `name` (`key`) let in here ([`sign_into`]), stored;
/// the whole of it answered for the joining host to adopt.
pub fn sign(member_toml: &Path, hostname: &str, name: &str, key: &PublicKey) -> Result<Wire, ApiError> {
    let (own_name, signer) = own(member_toml, hostname)?;
    let own = Own { name: &own_name, key: signer.public_key() };
    let after = membership::update(&membership::path(member_toml), |cur| -> Result<Stored, ApiError> {
        *cur = sign_into(cur, &own, &signer, name, key)?;
        Ok(cur.clone())
    })
    .map_err(ApiError::internal)??;
    Ok(after.to_wire())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SignParams {
    name: String,
    key: PublicKey,
}

fn to_value(x: impl Serialize) -> Answer {
    serde_json::to_value(x).map_err(|e| ApiError::internal(e.into()))
}

/// Run `method` if it's a `membership.*` one (`None` otherwise): its answer,
/// and the hub an adopt wrote, for the daemon to follow.
pub fn call(method: &str, params: Value, member_toml: &Path, hostname: &str) -> Option<(Answer, Option<Hub>)> {
    let answer = match method {
        "membership.get" => (load(member_toml).and_then(|s| to_value(MembershipInfo::of(s.as_ref()))), None),
        "membership.adopt" => {
            let adopted = parse::<Wire>(params)
                .and_then(|w| Stored::from_wire(w).map_err(|e| ApiError::invalid(format!("{e:#}"))))
                .and_then(|theirs| adopt(member_toml, hostname, &theirs));
            match adopted {
                Ok((info, hub)) => (to_value(info), Some(hub)),
                Err(e) => (Err(e), None),
            }
        }
        "membership.sign" => (parse::<SignParams>(params).and_then(|p| sign(member_toml, hostname, &p.name, &p.key)).and_then(to_value), None),
        _ => return None,
    };
    Some(answer)
}

/// Test fleets: logs signed with fixed keys, or with a test daemon's own
/// `member.key`.
#[cfg(test)]
pub mod test_support {
    use std::path::Path;

    use super::*;
    use crate::serve::fleet::membership::Log;

    /// The signing key of the member whose `member.toml` is `member_toml`
    /// (made now if it has none: its daemon reads the same one).
    pub fn key_of(member_toml: &Path) -> SigningKey {
        FileKeyStore::new(member::key_path(member_toml)).signing_key().unwrap()
    }

    /// A fleet whose authority is `authority` (signing with `key`), hub
    /// `hub`, then `members` added in order.
    pub fn fleet(authority: &str, key: &SigningKey, hub: &str, members: &[(&str, PublicKey)]) -> Stored {
        let mut log = Log::default();
        log.append(Op::genesis(authority, key.public_key(), hub).unwrap(), authority, key).unwrap();
        for (name, k) in members {
            log.append(Op::Add { name: name.to_string(), key: *k }, authority, key).unwrap();
        }
        Stored { log, introductions: vec![] }
    }

    /// `stored` written as the `membership.json` of `member_toml`'s member.
    pub fn write(member_toml: &Path, stored: &Stored) {
        membership::update(&membership::path(member_toml), |s| *s = stored.clone()).unwrap();
    }

    /// A fleet of `members` (each a name and its `member.toml`, the first
    /// the authority), hub `hub`, written as every member's
    /// `membership.json`: each `member.toml` gets its name (its other
    /// fields kept), each member its `member.key`, as a fleet-creating
    /// `machine add` leaves them. The log, for a test to extend.
    pub fn fleet_of(hub: &str, members: &[(&str, &Path)]) -> Stored {
        for (name, toml) in members {
            let mut m = member::load(toml).unwrap().unwrap_or_default();
            m.name = name.to_string();
            member::write_for_test(toml, &m);
        }
        let (authority, first) = members[0];
        let added: Vec<(&str, PublicKey)> = members[1..].iter().map(|(n, t)| (*n, key_of(t).public_key())).collect();
        let stored = fleet(authority, &key_of(first), hub, &added);
        for (_, toml) in members {
            write(toml, &stored);
        }
        stored
    }

    /// The `membership.json` of `member_toml`'s member.
    pub fn read(member_toml: &Path) -> Option<Stored> {
        membership::load(&membership::path(member_toml)).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::test_support::fleet;
    use super::*;

    fn key(n: u8) -> SigningKey {
        SigningKey::from_seed([n; 32])
    }

    fn pk(n: u8) -> PublicKey {
        key(n).public_key()
    }

    fn own(name: &str, n: u8) -> Own<'_> {
        Own { name, key: pk(n) }
    }

    /// `lap` (1) the authority, `box` (2) the hub, `pi` (3) a member.
    fn three() -> Stored {
        fleet("lap", &key(1), "box", &[("box", pk(2)), ("pi", pk(3))])
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("dsv-membership-api-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn the_authority_adds_a_member_introduces_a_capped_one_refuses() {
        let s = three();
        // The authority: an `add` entry.
        assert_eq!(can_sign(&s, &own("lap", 1), "new", &pk(5)), Ok(Signs::Add));
        let added = sign_into(&s, &own("lap", 1), &key(1), "new", &pk(5)).unwrap();
        assert_eq!(added.log.membership().unwrap().key_of("new"), Some(pk(5)));
        // Another member: an introduction by it.
        assert_eq!(can_sign(&s, &own("pi", 3), "new", &pk(5)), Ok(Signs::Introduction));
        let introduced = sign_into(&s, &own("pi", 3), &key(3), "new", &pk(5)).unwrap();
        assert_eq!(introduced.log, s.log);
        assert_eq!((introduced.introductions[0].name.as_str(), introduced.introductions[0].introducer.as_str()), ("new", "pi"));
        assert!(lists(&introduced, "new", &pk(5)) && !lists(&introduced, "new", &pk(6)));
        // The introduced one is capped: it introduces no one.
        let e = can_sign(&introduced, &own("new", 5), "other", &pk(6)).unwrap_err();
        assert_eq!(e.code, "denied");
        assert!(e.message.contains("introduced by `pi`") && e.message.contains("capped"), "{}", e.message);
        // A name or key already known, here or held.
        for (name, k) in [("pi", 9), ("x", 3), ("new", 9), ("x", 5)] {
            let e = can_sign(&introduced, &own("lap", 1), name, &pk(k)).unwrap_err();
            assert_eq!(e.code, "invalid", "{name} {k}: {}", e.message);
        }
        assert_eq!(can_sign(&s, &own("lap", 1), "self", &pk(5)).unwrap_err().code, "invalid");
        // Already in under that key, added or introduced: a retried add signs nothing.
        assert_eq!(can_sign(&added, &own("lap", 1), "new", &pk(5)), Ok(Signs::Already));
        assert_eq!(sign_into(&introduced, &own("pi", 3), &key(3), "new", &pk(5)).unwrap(), introduced);
        // In no fleet's log, or not in its own (a stale name or another key).
        assert_eq!(can_sign(&Stored::default(), &own("lap", 1), "new", &pk(5)).unwrap_err().code, "denied");
        assert_eq!(can_sign(&s, &own("pi", 9), "new", &pk(5)).unwrap_err().code, "denied");
        assert_eq!(can_sign(&s, &own("zed", 9), "new", &pk(5)).unwrap_err().code, "denied");
    }

    #[test]
    fn adopt_takes_extends_keeps_or_refuses() {
        let s = three();
        // No log: taken, if it lists this host.
        assert_eq!(adopt_into(&Stored::default(), &s, &own("pi", 3)).unwrap(), s);
        let e = adopt_into(&Stored::default(), &s, &own("zed", 9)).unwrap_err();
        assert!(e.code == "denied" && e.message.contains("doesn't list this host"), "{}", e.message);
        assert_eq!(adopt_into(&Stored::default(), &s, &own("pi", 9)).unwrap_err().code, "denied", "its name, another key");
        assert_eq!(adopt_into(&s, &Stored::default(), &own("pi", 3)).unwrap_err().code, "invalid", "nothing to adopt");
        // Behind: extended. Same or ahead: ours kept.
        let longer = sign_into(&s, &own("lap", 1), &key(1), "new", &pk(5)).unwrap();
        assert_eq!(adopt_into(&s, &longer, &own("pi", 3)).unwrap(), longer);
        assert_eq!(adopt_into(&longer, &s, &own("pi", 3)).unwrap(), longer);
        assert_eq!(adopt_into(&s, &s, &own("pi", 3)).unwrap(), s);
        // Their introductions are held; one the log settled is dropped.
        let intro = sign_into(&s, &own("pi", 3), &key(3), "new", &pk(5)).unwrap();
        let held = adopt_into(&s, &intro, &own("lap", 1)).unwrap();
        assert_eq!(held.introductions, intro.introductions);
        assert_eq!(adopt_into(&Stored::default(), &intro, &own("new", 5)).unwrap(), intro, "listed as introduced");
        assert_eq!(adopt_into(&longer, &intro, &own("lap", 1)).unwrap(), longer, "added since: settled");
        assert_eq!(adopt_into(&intro, &longer, &own("lap", 1)).unwrap(), longer, "ours settled by theirs");
        // Another fleet; a fork.
        let other = fleet("lap", &key(1), "box", &[("box", pk(2)), ("pi", pk(3))]);
        let e = adopt_into(&s, &other, &own("pi", 3)).unwrap_err();
        assert!(e.code == "denied" && e.message.contains("another fleet"), "{}", e.message);
        let fork = sign_into(&s, &own("lap", 1), &key(1), "other", &pk(6)).unwrap();
        let e = adopt_into(&longer, &fork, &own("pi", 3)).unwrap_err();
        assert!(e.code == "denied" && e.message.contains("fork"), "{}", e.message);
        // A conflicting introduction refuses the whole.
        let rival = sign_into(&s, &own("box", 2), &key(2), "new", &pk(6)).unwrap();
        let e = adopt_into(&intro, &rival, &own("lap", 1)).unwrap_err();
        assert!(e.code == "denied" && e.message.contains("introduction of `new` by `box`"), "{}", e.message);
    }

    #[test]
    fn the_calls_go_through_the_files() {
        let dir = scratch("calls");
        let member_toml = dir.join("member.toml");
        member::write_for_test(&member_toml, &member::Member { name: "pi".into(), ..Default::default() });
        let mine = test_support::key_of(&member_toml);
        let call = |m: &str, p: Value| super::call(m, p, &member_toml, "host").unwrap();
        // Nothing stored: an empty summary.
        let (got, hub) = call("membership.get", Value::Null);
        assert_eq!(hub, None);
        let head = serde_json::to_value(Head::EMPTY).unwrap();
        assert_eq!(got.unwrap(), json!({"stored": null, "summary": {"authority": null, "hub": null, "members": {}, "introductions": [], "head": head}}));
        let (e, _) = call("membership.sign", json!({"name": "new", "key": pk(5)}));
        assert_eq!(e.unwrap_err().code, "denied", "in no fleet");

        // Adopted: stored, the hub written from it, the name pinned.
        let s = fleet("lap", &key(1), "box", &[("box", pk(2)), ("pi", mine.public_key())]);
        let (got, hub) = call("membership.adopt", serde_json::to_value(s.to_wire()).unwrap());
        let got = got.unwrap();
        assert_eq!(hub, Some(Hub::Other("box".into())));
        assert_eq!((&got["summary"]["authority"], &got["summary"]["hub"]), (&json!("lap"), &json!("box")));
        assert_eq!(got["summary"]["members"]["pi"]["fingerprint"], mine.public_key().fingerprint());
        assert_eq!(test_support::read(&member_toml).unwrap(), s);
        assert_eq!(member::load(&member_toml).unwrap().unwrap().hub(), Hub::Other("box".into()));
        assert_eq!(call("membership.get", Value::Null).0.unwrap(), got);

        // A member signs an introduction, stores it and answers the whole.
        let (signed, _) = call("membership.sign", json!({"name": "new", "key": pk(5)}));
        let signed: Wire = serde_json::from_value(signed.unwrap()).unwrap();
        assert_eq!((signed.introductions.len(), signed.introductions[0].introducer.as_str()), (1, "pi"));
        assert_eq!(test_support::read(&member_toml).unwrap().to_wire(), signed);
        let (again, _) = call("membership.sign", json!({"name": "new", "key": pk(5)}));
        assert_eq!(serde_json::from_value::<Wire>(again.unwrap()).unwrap(), signed, "already introduced: unchanged");
        let (e, _) = call("membership.sign", json!({"name": "new", "key": pk(6)}));
        assert_eq!(e.unwrap_err().code, "invalid", "the name under another key");

        // Refused: another fleet's log, one without this host, a bad one.
        let other = fleet("lap", &key(1), "box", &[("box", pk(2)), ("pi", mine.public_key())]);
        let (e, hub) = call("membership.adopt", serde_json::to_value(other.to_wire()).unwrap());
        assert_eq!((e.unwrap_err().code, hub), ("denied", None));
        let without = fleet("lap", &key(1), "lap", &[]);
        assert_eq!(call("membership.adopt", serde_json::to_value(without.to_wire()).unwrap()).0.unwrap_err().code, "denied");
        let mut bad = serde_json::to_value(s.to_wire()).unwrap();
        bad["entries"][1]["name"] = json!("bix");
        assert_eq!(call("membership.adopt", bad).0.unwrap_err().code, "invalid");
        assert_eq!(call("membership.adopt", json!({"entries": [], "later": 1})).0.unwrap_err().code, "invalid");
        assert_eq!(test_support::read(&member_toml).unwrap().to_wire(), signed, "unchanged");
        assert!(super::call("membership.nope", Value::Null, &member_toml, "host").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_wire_matches_the_typings() {
        use crate::serve::dts::{assert_matches, prop_type};
        let s = sign_into(&three(), &own("pi", 3), &key(3), "new", &pk(5)).unwrap();
        let info = serde_json::to_value(MembershipInfo::of(Some(&s))).unwrap();
        assert_matches("MembershipInfo", &info);
        assert_matches("MembershipSummary", &info["summary"]);
        assert_matches("MembershipKey", &info["summary"]["members"]["pi"]);
        assert_matches("MembershipIntroduced", &info["summary"]["introductions"][0]);
        assert_matches("MembershipHead", &info["summary"]["head"]);
        assert_matches("MembershipStored", &info["stored"]);
        assert_matches("MembershipIntroduction", &info["stored"]["introductions"][0]);
        assert_matches("MembershipStatement", &info["stored"]["introductions"][0]["statement"]);
        for e in info["stored"]["entries"].as_array().unwrap() {
            let ty = match e["op"].as_str().unwrap() {
                "genesis" => "MembershipGenesis",
                _ => "MembershipAdd",
            };
            assert_matches(ty, e);
        }
        for (method, result) in [("membership.get", "MembershipInfo"), ("membership.adopt", "MembershipInfo"), ("membership.sign", "MembershipStored")] {
            let ty = prop_type("ApiMethods", method).unwrap_or_else(|| panic!("ApiMethods lacks `{method}`"));
            assert!(ty.contains(&format!("result: {result} ")), "{method}: {ty}");
        }
    }
}
