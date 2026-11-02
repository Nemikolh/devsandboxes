//! Membership changes proposed to the authority (docs/architecture/daemon/fleet.md,
//! *Approvals*; fleet plan step 10a.6, decision 49): a member's `machine rm`
//! or the introduction its `machine add` signed reaches the authority as
//! `fleet.propose {op}` and waits in `proposals.json` (next to
//! `membership.json`, [`path`]) until the user runs `devsandbox machine
//! approve|deny <id>` there. The authority's key never signs on a remote
//! request alone.
//!
//! [`check`] (what a [`Change`] would sign, against the log) and
//! [`Proposals::queue`] (one pending entry per change) are pure; the file is
//! written by the daemon only, through [`update`].

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::membership::{Introduction, Membership, Op, check_introduction};

/// A proposed change. On the wire tagged by `kind`: `{"kind": "revoke",
/// "name": "pi"}`, `{"kind": "add", "introduction": {…}}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Change {
    /// A held introduction promoted to an `add` entry.
    Add { introduction: Introduction },
    /// `name` out of the fleet: a `revoke` of its key.
    Revoke { name: String },
}

impl Change {
    /// The same change as `other`: a revoke of the same name, an add of the
    /// same name and key.
    fn same(&self, other: &Change) -> bool {
        match (self, other) {
            (Change::Revoke { name: a }, Change::Revoke { name: b }) => a == b,
            (Change::Add { introduction: a }, Change::Add { introduction: b }) => a.name == b.name && a.key == b.key,
            _ => false,
        }
    }
}

impl fmt::Display for Change {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Change::Add { introduction: i } => {
                write!(f, "add {} to the fleet (introduced by {}, key {})", i.name, i.introducer, i.key.fingerprint())
            }
            Change::Revoke { name } => write!(f, "remove {name} from the fleet"),
        }
    }
}

/// The entry `change` makes the authority sign, checked against `m` (pure):
/// a revoke of a member's current key (not the authority's, not the hub's),
/// or an add of an introduction that holds ([`check_introduction`]: its
/// introducer a member of the log, its name and key new). Why not, else.
pub fn check(m: &Membership, change: &Change) -> Result<Op, String> {
    match change {
        Change::Revoke { name } => {
            if *name == m.authority.0 {
                return Err(format!("`{name}` is the fleet's authority: it can't be removed"));
            }
            if *name == m.hub {
                return Err(format!("`{name}` is the hub: name another hub first (fleet plan step 11)"));
            }
            let key = m.key_of(name).ok_or_else(|| format!("`{name}` is not a member of the fleet's membership log"))?;
            Ok(Op::Revoke { name: name.clone(), key })
        }
        Change::Add { introduction } => {
            check_introduction(m, introduction).map_err(|e| format!("introduction of `{}` by `{}`: {e:#}", introduction.name, introduction.introducer))?;
            Ok(Op::Add { name: introduction.name.clone(), key: introduction.key })
        }
    }
}

/// One pending proposal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Proposal {
    pub id: u64,
    /// The member that proposed it (its proven name).
    pub from: String,
    pub op: Change,
    /// When it came (unix seconds).
    pub at: u64,
}

/// `proposals.json`: the pending proposals and the next id.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Proposals {
    #[serde(default)]
    pub next: u64,
    #[serde(default)]
    pub pending: Vec<Proposal>,
}

impl Proposals {
    /// `op` from `from` queued at `at`: its id, and whether it's new. A
    /// pending proposal of the same change takes it (its id answered).
    pub fn queue(&mut self, from: &str, op: Change, at: u64) -> (u64, bool) {
        if let Some(p) = self.pending.iter().find(|p| p.op.same(&op)) {
            return (p.id, false);
        }
        self.next = self.next.max(1);
        let id = self.next;
        self.next += 1;
        self.pending.push(Proposal { id, from: from.to_string(), op, at });
        (id, true)
    }

    pub fn get(&self, id: u64) -> Option<&Proposal> {
        self.pending.iter().find(|p| p.id == id)
    }

    /// Proposal `id` dropped; whether there was one.
    pub fn take(&mut self, id: u64) -> Option<Proposal> {
        let at = self.pending.iter().position(|p| p.id == id)?;
        Some(self.pending.remove(at))
    }
}

/// `proposals.json` next to the `member.toml` at `member_toml`, as
/// `membership.json` is.
pub fn path(member_toml: &Path) -> PathBuf {
    member_toml.with_file_name("proposals.json")
}

/// The file at `path`; missing is none pending.
pub fn load(path: &Path) -> Result<Proposals> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).with_context(|| format!("invalid {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Proposals::default()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// Read-modify-write of the file at `path` under its lock
/// (`serve::locked_file`), saved only if `f` changed it.
pub fn update<T>(path: &Path, f: impl FnOnce(&mut Proposals) -> T) -> Result<T> {
    let serialize = |p: &Proposals| Ok(serde_json::to_string_pretty(p).context("cannot serialize proposals.json")? + "\n");
    crate::serve::locked_file::update(path, load, serialize, f)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::serve::fleet::identity::{PublicKey, Signer, SigningKey};
    use crate::serve::fleet::membership_api::test_support::fleet;

    fn key(n: u8) -> SigningKey {
        SigningKey::from_seed([n; 32])
    }

    fn pk(n: u8) -> PublicKey {
        key(n).public_key()
    }

    /// `lap` (1) the authority, `box` (2) the hub, `pi` (3).
    fn three() -> Membership {
        fleet("lap", &key(1), "box", &[("box", pk(2)), ("pi", pk(3))]).log.membership().unwrap().clone()
    }

    fn revoke(name: &str) -> Change {
        Change::Revoke { name: name.into() }
    }

    #[test]
    fn a_change_checks_against_the_log() {
        let m = three();
        assert_eq!(check(&m, &revoke("pi")), Ok(Op::Revoke { name: "pi".into(), key: pk(3) }));
        assert!(check(&m, &revoke("lap")).unwrap_err().contains("the fleet's authority"));
        assert!(check(&m, &revoke("box")).unwrap_err().contains("is the hub: name another hub first"));
        assert!(check(&m, &revoke("zed")).unwrap_err().contains("not a member"));
        let by_pi = Introduction::sign(&m.genesis, "new", pk(5), "pi", &key(3));
        assert_eq!(check(&m, &Change::Add { introduction: by_pi }), Ok(Op::Add { name: "new".into(), key: pk(5) }));
        // An introducer that isn't a member (a capped one), a name taken.
        let by_new = Introduction::sign(&m.genesis, "other", pk(6), "new", &key(5));
        assert!(check(&m, &Change::Add { introduction: by_new }).unwrap_err().contains("not a member of the log"));
        let taken = Introduction::sign(&m.genesis, "pi", pk(6), "box", &key(2));
        assert!(check(&m, &Change::Add { introduction: taken }).unwrap_err().contains("already a member"));
    }

    #[test]
    fn the_queue_collapses_a_pending_change() {
        let m = three();
        let mut q = Proposals::default();
        assert_eq!(q.queue("pi", revoke("pi"), 10), (1, true));
        assert_eq!(q.queue("box", revoke("pi"), 11), (1, false), "the same change: its id");
        let intro = Introduction::sign(&m.genesis, "new", pk(5), "pi", &key(3));
        assert_eq!(q.queue("pi", Change::Add { introduction: intro.clone() }, 12), (2, true));
        // The same name and key, introduced by another: the same add.
        let again = Introduction::sign(&m.genesis, "new", pk(5), "box", &key(2));
        assert_eq!(q.queue("box", Change::Add { introduction: again }, 13), (2, false));
        assert_eq!(q.pending.iter().map(|p| (p.id, p.from.as_str(), p.at)).collect::<Vec<_>>(), [(1, "pi", 10), (2, "pi", 12)]);
        assert_eq!(q.take(1).unwrap().op, revoke("pi"));
        assert!(q.take(1).is_none() && q.get(1).is_none());
        assert_eq!(q.queue("pi", revoke("pi"), 14), (3, true), "ids aren't reused");
    }

    #[test]
    fn the_file_round_trips() {
        let dir = std::env::temp_dir().join(format!("dsv-proposals-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = path(&dir.join("member.toml"));
        assert_eq!(file, dir.join("proposals.json"));
        assert_eq!(load(&file).unwrap(), Proposals::default());
        let (id, new) = update(&file, |q| q.queue("pi", revoke("pi"), 7)).unwrap();
        assert_eq!((id, new), (1, true));
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(v, json!({"next": 2, "pending": [{"id": 1, "from": "pi", "op": {"kind": "revoke", "name": "pi"}, "at": 7}]}));
        assert_eq!(load(&file).unwrap().pending.len(), 1);
        std::fs::write(&file, "{\"later\": 1}").unwrap();
        assert!(load(&file).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
