//! Agent grants (docs/architecture/daemon/fleet.md, *AGENT*; fleet plan
//! decisions 65-67): whom this host's ssh-agent reaches, kept in
//! `member.toml` as `[[agent]]` tables (`member::Grant`, a member's name and
//! key), written by the daemon only (`machines.set {agent}`, `machines.add
//! {agent}`) through [`grant`] and [`withdraw`].
//!
//! A grant is by key, not by name: [`granted`] holds for the key it was
//! made with and every key the membership log's `rotate` entries move it to
//! under that name, while that key is the name's current one (in the log,
//! or a held introduction's). A `revoke` ends it; a member added again
//! under the same name with a new key isn't granted.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::Result;

use crate::serve::fleet::identity::PublicKey;
use crate::serve::fleet::member::{self, Grant};
use crate::serve::fleet::membership::{Op, Stored};
use crate::serve::fleet::proof;

/// `name`'s grant in `grants` set to `key` (one per name, by name order).
/// Whether it changed.
pub fn set(grants: &mut Vec<Grant>, name: &str, key: PublicKey) -> bool {
    let next = Grant { name: name.to_string(), key };
    if grants.iter().any(|g| *g == next) {
        return false;
    }
    grants.retain(|g| g.name != name);
    grants.push(next);
    grants.sort_by(|a, b| a.name.cmp(&b.name));
    true
}

/// `name`'s grant out of `grants`. Whether there was one.
pub fn unset(grants: &mut Vec<Grant>, name: &str) -> bool {
    let before = grants.len();
    grants.retain(|g| g.name != name);
    grants.len() != before
}

/// `key` followed through the log's `rotate` entries for `name`, in order.
fn rotated(stored: &Stored, name: &str, key: PublicKey) -> PublicKey {
    stored.log.entries().iter().fold(key, |cur, e| match &e.op {
        Op::Rotate { name: n, old, new, .. } if n == name && *old == cur => *new,
        _ => cur,
    })
}

/// Whether `grants` grant member `name` proving `proven`: `proven` is
/// `name`'s current key in `stored` (`proof::resolve`: the log's, else a
/// held introduction's) and a grant for `name` reaches it through the log's
/// rotations of that name.
pub fn granted(grants: &[Grant], stored: &Stored, name: &str, proven: &PublicKey) -> bool {
    if proof::resolve(stored, name).is_none_or(|(current, _)| current != *proven) {
        return false;
    }
    grants.iter().any(|g| g.name == name && rotated(stored, name, g.key) == *proven)
}

/// The names `grants` grant now: each whose current key in `stored` is
/// [`granted`]. What `machines.list`, the snapshots and this host's link
/// report say.
pub fn granted_names(grants: &[Grant], stored: &Stored) -> BTreeSet<String> {
    grants
        .iter()
        .filter(|g| proof::resolve(stored, &g.name).is_some_and(|(key, _)| granted(grants, stored, &g.name, &key)))
        .map(|g| g.name.clone())
        .collect()
}

/// [`granted_names`] of `member_toml`'s grants against the membership log
/// beside it; none when either is missing or doesn't load.
pub fn load_granted(member_toml: &Path) -> BTreeSet<String> {
    let grants = member::load(member_toml).ok().flatten().map(|m| m.agent).unwrap_or_default();
    if grants.is_empty() {
        return BTreeSet::new();
    }
    match crate::serve::fleet::membership_api::load(member_toml) {
        Ok(Some(stored)) => granted_names(&grants, &stored),
        _ => BTreeSet::new(),
    }
}

/// `name` granted as `key` in `member_toml` (locked, atomic:
/// `member::update`). Whether the file changed.
pub fn grant(member_toml: &Path, name: &str, key: PublicKey) -> Result<bool> {
    member::update(member_toml, |m| set(&mut m.agent, name, key))
}

/// `name`'s grant withdrawn from `member_toml`. Whether there was one.
pub fn withdraw(member_toml: &Path, name: &str) -> Result<bool> {
    member::update(member_toml, |m| unset(&mut m.agent, name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve::fleet::identity::{Signer, SigningKey};
    use crate::serve::fleet::membership::{Introduction, countersign_rotate};
    use crate::serve::fleet::membership_api::test_support;

    fn k(n: u8) -> SigningKey {
        SigningKey::from_seed([n; 32])
    }

    /// `lap` (1) the authority and hub, `box` (2), `pi` (3).
    fn stored() -> Stored {
        test_support::fleet("lap", &k(1), "lap", &[("box", k(2).public_key()), ("pi", k(3).public_key())])
    }

    fn append(s: &mut Stored, op: Op) {
        s.log.append(op, "lap", &k(1)).unwrap();
    }

    fn rotate(s: &mut Stored, name: &str, old: u8, new: u8) {
        let genesis = s.log.membership().unwrap().genesis;
        let by_old = countersign_rotate(&genesis, name, &k(new).public_key(), &k(old));
        append(s, Op::Rotate { name: name.into(), old: k(old).public_key(), new: k(new).public_key(), by_old });
    }

    fn grant_of(name: &str, n: u8) -> Grant {
        Grant { name: name.into(), key: k(n).public_key() }
    }

    #[test]
    fn a_grant_is_by_key_and_follows_rotations() {
        let mut s = stored();
        let grants = [grant_of("box", 2)];
        assert!(granted(&grants, &s, "box", &k(2).public_key()), "plain");
        assert!(!granted(&grants, &s, "pi", &k(3).public_key()), "another member");
        assert!(!granted(&grants, &s, "box", &k(3).public_key()), "box's name, another key");
        assert!(!granted(&[grant_of("box", 9)], &s, "box", &k(2).public_key()), "granted another key");
        assert!(!granted(&[grant_of("pi", 2)], &s, "box", &k(2).public_key()), "the key under another name");
        assert_eq!(granted_names(&grants, &s), BTreeSet::from(["box".to_string()]));

        rotate(&mut s, "box", 2, 4);
        assert!(granted(&grants, &s, "box", &k(4).public_key()), "rotated once");
        assert!(!granted(&grants, &s, "box", &k(2).public_key()), "the old key no longer");
        rotate(&mut s, "box", 4, 5);
        assert!(granted(&grants, &s, "box", &k(5).public_key()), "rotated twice");
        assert!(granted(&[grant_of("box", 4)], &s, "box", &k(5).public_key()), "granted midway");
        assert!(!granted(&[grant_of("box", 5)], &s, "pi", &k(3).public_key()));
        assert_eq!(granted_names(&grants, &s), BTreeSet::from(["box".to_string()]));

        // Revoked: gone. Added again under the name with a new key: no grant.
        append(&mut s, Op::Revoke { name: "box".into(), key: k(5).public_key() });
        assert!(!granted(&grants, &s, "box", &k(5).public_key()), "revoked");
        assert!(granted_names(&grants, &s).is_empty());
        append(&mut s, Op::Add { name: "box".into(), key: k(6).public_key() });
        assert!(!granted(&grants, &s, "box", &k(6).public_key()), "revoke then re-add doesn't inherit");
        assert!(granted_names(&grants, &s).is_empty());
        assert!(granted(&[grant_of("box", 6)], &s, "box", &k(6).public_key()), "granted afresh");
    }

    #[test]
    fn an_introduced_member_is_granted_by_its_introductions_key() {
        let mut s = stored();
        let genesis = s.log.membership().unwrap().genesis;
        s.introduce(Introduction::sign(&genesis, "w", k(8).public_key(), "box", &k(2))).unwrap();
        assert!(granted(&[grant_of("w", 8)], &s, "w", &k(8).public_key()));
        assert!(!granted(&[grant_of("w", 9)], &s, "w", &k(8).public_key()));
        assert!(!granted(&[grant_of("x", 8)], &s, "x", &k(8).public_key()), "no such member");
        assert_eq!(granted_names(&[grant_of("w", 8), grant_of("x", 8)], &s), BTreeSet::from(["w".to_string()]));
        assert!(!granted(&[grant_of("box", 2)], &Stored::default(), "box", &k(2).public_key()), "no log");
    }

    #[test]
    fn set_replaces_by_name_and_unset_withdraws() {
        let mut g = vec![];
        assert!(set(&mut g, "pi", k(3).public_key()));
        assert!(set(&mut g, "box", k(2).public_key()));
        assert!(!set(&mut g, "box", k(2).public_key()), "the same");
        assert!(set(&mut g, "box", k(4).public_key()), "replaced");
        assert_eq!(g, [grant_of("box", 4), grant_of("pi", 3)], "one per name, by name");
        assert!(unset(&mut g, "box"));
        assert!(!unset(&mut g, "box"));
        assert_eq!(g, [grant_of("pi", 3)]);
    }

    #[test]
    fn grants_round_trip_through_member_toml() {
        let dir = std::env::temp_dir().join(format!("dsv-grants-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("member.toml");
        assert!(load_granted(&path).is_empty(), "no file");
        assert!(grant(&path, "box", k(2).public_key()).unwrap());
        assert!(!grant(&path, "box", k(2).public_key()).unwrap());
        assert!(grant(&path, "pi", k(3).public_key()).unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(&format!("[[agent]]\nname = \"box\"\nkey = \"{}\"\n", k(2).public_key())), "{text}");
        let m = member::load(&path).unwrap().unwrap();
        assert_eq!(m.agent, [grant_of("box", 2), grant_of("pi", 3)]);
        assert_eq!(member::parse(&member::serialize(&m).unwrap()).unwrap(), m);
        assert!(load_granted(&path).is_empty(), "no log beside it");
        test_support::write(&path, &stored());
        assert_eq!(load_granted(&path), BTreeSet::from(["box".to_string(), "pi".to_string()]));
        assert!(withdraw(&path, "box").unwrap());
        assert!(!withdraw(&path, "box").unwrap());
        assert_eq!(load_granted(&path), BTreeSet::from(["pi".to_string()]));
        // Checked: names valid, one grant each.
        let key = k(2).public_key();
        assert!(member::parse(&format!("[[agent]]\nname = \"a/b\"\nkey = \"{key}\"\n")).is_err());
        assert!(member::parse(&format!("[[agent]]\nname = \"b\"\nkey = \"{key}\"\n\n[[agent]]\nname = \"b\"\nkey = \"{key}\"\n")).is_err());
        assert!(member::parse("[[agent]]\nname = \"b\"\nkey = \"nope\"\n").is_err());
        assert!(!member::serialize(&member::Member::default()).unwrap().contains("agent"), "none: left out");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
