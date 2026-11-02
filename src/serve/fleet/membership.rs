//! The fleet's membership log (docs/architecture/daemon/fleet.md,
//! *Membership log*; fleet plan step 10a, decisions 45, 46, 50, 51): a hash
//! chain of entries the authority signs, each one [`Op`] (`genesis`, `add`,
//! `revoke`, `rotate`, `hub`), that [`apply`] verifies into a
//! [`Membership`]: the members and their keys, the hub, the revoked keys and
//! the [`Head`]. [`compare`] says how a peer's head stands to ours.
//!
//! Introductions (decision 50) sit outside the chain: a member's signed word
//! for a key the authority hasn't added yet, checked against the log
//! ([`check_introduction`]) and held until the log settles it.
//!
//! `membership.json`, next to `member.toml` ([`path`]), holds both
//! ([`Stored`]); written by the daemon only, through [`update`], and only
//! state that verifies is loaded or saved. The `membership.*` calls over it
//! are `super::membership_api`'s.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::serve::fleet::identity::{PublicKey, Signature, Signer, Statement, hex32};
use crate::serve::fleet::member::{self, check_member_name};

/// A log entry's [`Statement::kind`].
pub const KIND: &str = "membership";
/// An introduction's [`Statement::kind`].
pub const INTRODUCTION_KIND: &str = "introduction";
/// The domain of a `rotate`'s countersignature by the old key
/// ([`countersign_rotate`]).
pub const ROTATE_DOMAIN: &str = "devsandbox/membership-rotate/v1";

/// The `prev` of the genesis entry, and an empty log's head hash.
const ZERO: [u8; 32] = [0; 32];

/// `field` length-prefixed (u32 big-endian) onto `out`, so no two field
/// lists encode alike.
fn put(out: &mut Vec<u8>, field: &[u8]) {
    let len = u32::try_from(field.len()).expect("a membership field under 4 GiB");
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(field);
}

fn short(hash: &[u8; 32]) -> String {
    hex::encode(&hash[..6])
}

/// One change to the membership. On the wire (serde) tagged by `op`:
/// `{"op": "add", "name": "box", "key": "ed25519:…"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    /// The first entry, only there: the fleet's authority (a member with
    /// `key`, which signs this and every later entry) and its hub, which
    /// may be a member the next entries add. `id` is random
    /// ([`Op::genesis`]; 32 hex digits on the wire): signing is
    /// deterministic, so without it two fleets one authority makes with
    /// the same hub would have the same genesis, the same fleet to a
    /// member that held the first one's log.
    Genesis {
        #[serde(with = "hex16")]
        id: [u8; 16],
        authority: String,
        key: PublicKey,
        hub: String,
    },
    Add { name: String, key: PublicKey },
    /// `name`, whose key is `key`, leaves; `key` is revoked for good.
    Revoke { name: String, key: PublicKey },
    /// `name`'s key goes from `old` to `new`; `by_old` is `old`'s
    /// signature over the change ([`countersign_rotate`]), so the
    /// authority's key alone can't swap a member's. `old` is revoked.
    Rotate { name: String, old: PublicKey, new: PublicKey, by_old: Signature },
    /// The hub is now the member `name`.
    Hub { name: String },
}

impl Op {
    /// A new fleet's genesis, its `id` from the OS RNG.
    pub fn genesis(authority: &str, key: PublicKey, hub: &str) -> Result<Op> {
        let mut id = [0u8; 16];
        getrandom::fill(&mut id).map_err(|e| anyhow::anyhow!("cannot read the OS random source: {e}"))?;
        Ok(Op::genesis_with_id(id, authority, key, hub))
    }

    /// [`Op::genesis`] with a given `id` (tests: a deterministic fleet).
    pub fn genesis_with_id(id: [u8; 16], authority: &str, key: PublicKey, hub: &str) -> Op {
        Op::Genesis { id, authority: authority.to_string(), key, hub: hub.to_string() }
    }

    /// What the entry's payload encodes after `prev`: a tag, then each
    /// field, length-prefixed, keys and signatures raw.
    fn encode_into(&self, out: &mut Vec<u8>) {
        match self {
            Op::Genesis { id, authority, key, hub } => {
                for f in [&b"genesis"[..], id, authority.as_bytes(), key.as_bytes(), hub.as_bytes()] {
                    put(out, f);
                }
            }
            Op::Add { name, key } => {
                for f in [&b"add"[..], name.as_bytes(), key.as_bytes()] {
                    put(out, f);
                }
            }
            Op::Revoke { name, key } => {
                for f in [&b"revoke"[..], name.as_bytes(), key.as_bytes()] {
                    put(out, f);
                }
            }
            Op::Rotate { name, old, new, by_old } => {
                let sig = by_old.to_bytes();
                for f in [&b"rotate"[..], name.as_bytes(), old.as_bytes(), new.as_bytes(), &sig] {
                    put(out, f);
                }
            }
            Op::Hub { name } => {
                for f in [&b"hub"[..], name.as_bytes()] {
                    put(out, f);
                }
            }
        }
    }
}

impl fmt::Display for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Op::Genesis { id, authority, hub, .. } => {
                write!(f, "genesis of fleet {} (authority `{authority}`, hub `{hub}`)", hex::encode(&id[..4]))
            }
            Op::Add { name, .. } => write!(f, "add `{name}`"),
            Op::Revoke { name, .. } => write!(f, "revoke `{name}`"),
            Op::Rotate { name, .. } => write!(f, "rotate `{name}`'s key"),
            Op::Hub { name } => write!(f, "hub `{name}`"),
        }
    }
}

/// An entry's payload: `prev` then `op`, length-prefixed.
fn encode(prev: &[u8; 32], op: &Op) -> Vec<u8> {
    let mut out = Vec::new();
    put(&mut out, prev);
    op.encode_into(&mut out);
    out
}

/// What a `rotate`'s `by_old` signs, in [`ROTATE_DOMAIN`]: the fleet's
/// genesis hash, the name and the new key, length-prefixed. The genesis
/// binds it to one fleet.
fn rotate_message(genesis: &[u8; 32], name: &str, new: &PublicKey) -> Vec<u8> {
    let mut out = Vec::new();
    for f in [&genesis[..], name.as_bytes(), new.as_bytes()] {
        put(&mut out, f);
    }
    out
}

/// `old`'s countersignature of `name` moving to `new` in the fleet of
/// genesis hash `genesis` ([`Membership::genesis`]): a `rotate`'s `by_old`.
#[cfg_attr(not(test), allow(dead_code))] // `machine key rotate` (10a.6)
pub fn countersign_rotate(genesis: &[u8; 32], name: &str, new: &PublicKey, old: &impl Signer) -> Signature {
    old.sign(ROTATE_DOMAIN, &rotate_message(genesis, name, new))
}

/// One link of the chain: `op`, the hash of the entry before (`prev`,
/// zeros for genesis), and the authority's [`Statement`] of kind
/// [`KIND`] over both ([`encode`]) whose `version` is the entry's sequence
/// number (0 for genesis). Self-contained: [`Statement::verify_payload`]
/// checks `op` and `prev` are what was signed. On the wire the op's fields
/// sit beside `prev` and `statement`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    #[serde(flatten)]
    pub op: Op,
    #[serde(with = "hex32")]
    pub prev: [u8; 32],
    pub statement: Statement,
}

impl Entry {
    /// Entry `seq` after the one hashing to `prev`, signed by `key` as
    /// `authority`. Unchecked: [`Log::append`] checks it applies.
    pub fn sign(seq: u64, prev: [u8; 32], op: Op, authority: &str, key: &impl Signer) -> Entry {
        let statement = Statement::sign(authority, key, KIND, &encode(&prev, &op), seq);
        Entry { op, prev, statement }
    }

    pub fn seq(&self) -> u64 {
        self.statement.version
    }

    /// sha256 of the statement's signed bytes and its signature: covers
    /// the op and `prev` (through the payload hash) and who signed.
    pub fn hash(&self) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(self.statement.signed_bytes());
        h.update(self.statement.sig.to_bytes());
        h.finalize().into()
    }
}

/// Where a log stands: its length, its last entry's hash and its genesis
/// entry's hash (which fleet; both zeros when empty), so heads alone tell
/// another fleet's log from a fork of ours. On the wire `{"len": 3,
/// "hash": "<64 hex>", "genesis": "<64 hex>"}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Head {
    pub len: u64,
    #[serde(with = "hex32")]
    pub hash: [u8; 32],
    #[serde(with = "hex32")]
    pub genesis: [u8; 32],
}

impl Head {
    pub const EMPTY: Head = Head { len: 0, hash: ZERO, genesis: ZERO };
}

impl fmt::Display for Head {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} entries, head {}, fleet {}", self.len, short(&self.hash), short(&self.genesis))
    }
}

/// `[u8; 16]` as 32 lowercase hex digits.
mod hex16 {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8; 16], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 16], D::Error> {
        let text = String::deserialize(d)?;
        if text.bytes().any(|b| b.is_ascii_uppercase()) {
            return Err(serde::de::Error::custom("hex digits are lowercase"));
        }
        let mut out = [0u8; 16];
        hex::decode_to_slice(&text, &mut out).map_err(|e| serde::de::Error::custom(format!("expected 32 hex digits: {e}")))?;
        Ok(out)
    }
}

/// What a verified log says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Membership {
    /// The authority's name and current key (a `rotate` of it changes the
    /// key that signs later entries).
    pub authority: (String, PublicKey),
    /// The hub's member name.
    pub hub: String,
    pub members: BTreeMap<String, PublicKey>,
    /// Keys revoked or rotated away: never a member's again.
    pub revoked: BTreeSet<PublicKey>,
    pub head: Head,
    /// The genesis entry's hash: which fleet this is. Introductions and
    /// rotations are bound to it.
    pub genesis: [u8; 32],
}

impl Membership {
    pub fn key_of(&self, name: &str) -> Option<PublicKey> {
        self.members.get(name).copied()
    }

    pub fn name_of(&self, key: &PublicKey) -> Option<&str> {
        self.members.iter().find(|(_, k)| *k == key).map(|(n, _)| n.as_str())
    }

    pub fn is_member_key(&self, key: &PublicKey) -> bool {
        self.name_of(key).is_some()
    }

    /// The hub's key; `None` while genesis's hub isn't added yet.
    #[cfg_attr(not(test), allow(dead_code))] // the hub-only calls by key (10a.5)
    pub fn hub_key(&self) -> Option<PublicKey> {
        self.key_of(&self.hub)
    }

    pub fn is_authority(&self, key: &PublicKey) -> bool {
        self.authority.1 == *key
    }

    /// Whether the log has settled `i`, so it's no longer held: its name or
    /// key is in the log (a member's or revoked; added, or decided
    /// otherwise), or its introducer is no longer a member with the key
    /// that signed it (revoked, or rotated away).
    pub fn settles(&self, i: &Introduction) -> bool {
        self.members.contains_key(&i.name)
            || self.revoked.contains(&i.key)
            || self.is_member_key(&i.key)
            || self.key_of(&i.introducer) != Some(i.statement.key)
    }

    /// `intros` without those the log has settled ([`Membership::settles`]).
    pub fn drop_settled(&self, intros: &mut Vec<Introduction>) {
        intros.retain(|i| !self.settles(i));
    }

    /// `op` applied (the rules but the signature's).
    fn apply_op(&mut self, op: &Op) -> Result<()> {
        match op {
            Op::Genesis { .. } => bail!("a second genesis"),
            Op::Add { name, key } => {
                check_member_name(name).with_context(|| format!("member name `{name}`"))?;
                if self.members.contains_key(name) {
                    bail!("`{name}` is already a member");
                }
                if self.revoked.contains(key) {
                    bail!("{key} is revoked");
                }
                if let Some(other) = self.name_of(key) {
                    bail!("{key} is already `{other}`'s key");
                }
                self.members.insert(name.clone(), *key);
            }
            Op::Revoke { name, key } => {
                let Some(current) = self.key_of(name) else { bail!("`{name}` is not a member") };
                if current != *key {
                    bail!("`{name}`'s key is {current}, not {key}");
                }
                if *name == self.authority.0 {
                    bail!("`{name}` is the authority: it can't be revoked (rotate its key instead)");
                }
                if *name == self.hub {
                    bail!("`{name}` is the hub: name another hub first");
                }
                self.members.remove(name);
                self.revoked.insert(*key);
            }
            Op::Rotate { name, old, new, by_old } => {
                let Some(current) = self.key_of(name) else { bail!("`{name}` is not a member") };
                if current != *old {
                    bail!("`{name}`'s key is {current}, not {old}");
                }
                old.verify(ROTATE_DOMAIN, &rotate_message(&self.genesis, name, new), by_old)
                    .with_context(|| format!("`{name}`'s new key isn't countersigned by its old one"))?;
                if self.revoked.contains(new) {
                    bail!("{new} is revoked");
                }
                if let Some(other) = self.name_of(new) {
                    bail!("{new} is already `{other}`'s key");
                }
                self.members.insert(name.clone(), *new);
                self.revoked.insert(*old);
                if *name == self.authority.0 {
                    self.authority.1 = *new;
                }
            }
            Op::Hub { name } => {
                if !self.members.contains_key(name) {
                    bail!("`{name}` is not a member: it can't be the hub");
                }
                self.hub = name.clone();
            }
        }
        Ok(())
    }
}

/// The membership after `entry`, given the one `before` it (`None`: the
/// log is empty, `entry` must be its genesis).
fn next(before: Option<&Membership>, entry: &Entry) -> Result<Membership> {
    let (seq, prev) = before.map_or((0, ZERO), |m| (m.head.len, m.head.hash));
    let s = &entry.statement;
    let at = || format!("membership entry {seq} ({})", entry.op);
    if s.kind != KIND {
        bail!("{}: a `{}` statement, not `{KIND}`", at(), s.kind);
    }
    if s.version != seq {
        bail!("{}: numbered {}, expected {seq} (entries missing or out of order)", at(), s.version);
    }
    if entry.prev != prev {
        bail!("{}: doesn't follow entry {} (its previous-entry hash differs)", at(), seq.wrapping_sub(1));
    }
    s.verify_payload(&encode(&entry.prev, &entry.op)).with_context(at)?;
    let mut m = match before {
        None => {
            let Op::Genesis { authority, key, hub, .. } = &entry.op else {
                bail!("{}: the first entry must be `genesis`", at());
            };
            check_member_name(authority).with_context(|| format!("{}: authority `{authority}`", at()))?;
            check_member_name(hub).with_context(|| format!("{}: hub `{hub}`", at()))?;
            if s.key != *key || s.signer != *authority {
                bail!("{}: signed by `{}` ({}), not the authority it names", at(), s.signer, s.key);
            }
            Membership {
                authority: (authority.clone(), *key),
                hub: hub.clone(),
                members: BTreeMap::from([(authority.clone(), *key)]),
                revoked: BTreeSet::new(),
                head: Head::EMPTY,
                genesis: entry.hash(),
            }
        }
        Some(m) => {
            if s.key != m.authority.1 || s.signer != m.authority.0 {
                bail!("{}: signed by `{}` ({}), not the authority `{}` ({})", at(), s.signer, s.key, m.authority.0, m.authority.1);
            }
            let mut m = m.clone();
            m.apply_op(&entry.op).with_context(at)?;
            m
        }
    };
    m.head = Head { len: seq + 1, hash: entry.hash(), genesis: m.genesis };
    Ok(m)
}

/// Every entry verified in order into the membership: numbered from 0,
/// each following the one before, signed by the authority as of that
/// entry, its op allowed ([`Op`]'s rules).
pub fn apply(entries: &[Entry]) -> Result<Membership> {
    let mut m: Option<Membership> = None;
    for entry in entries {
        m = Some(next(m.as_ref(), entry)?);
    }
    m.context("an empty membership log (no genesis)")
}

/// A verified log: its entries and what they say. Changed only through
/// [`Log::append`] and [`Log::extend`], which keep it verified.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Log {
    entries: Vec<Entry>,
    membership: Option<Membership>,
}

impl Log {
    /// `entries` verified ([`apply`]); none is the empty log.
    pub fn new(entries: Vec<Entry>) -> Result<Log> {
        let membership = if entries.is_empty() { None } else { Some(apply(&entries)?) };
        Ok(Log { entries, membership })
    }

    #[cfg_attr(not(test), allow(dead_code))] // the link's head comparison (10a.4)
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// `None` for the empty log (in no fleet's log).
    pub fn membership(&self) -> Option<&Membership> {
        self.membership.as_ref()
    }

    pub fn head(&self) -> Head {
        self.membership.as_ref().map_or(Head::EMPTY, |m| m.head)
    }

    /// `op` as the next entry, signed by `key` as `authority`; refused (and
    /// the log unchanged) unless it applies.
    pub fn append(&mut self, op: Op, authority: &str, key: &impl Signer) -> Result<Entry> {
        let head = self.head();
        let entry = Entry::sign(head.len, head.hash, op, authority, key);
        let m = next(self.membership.as_ref(), &entry)?;
        self.entries.push(entry.clone());
        self.membership = Some(m);
        Ok(entry)
    }

    /// A peer's entries continuing ours (a leading run we already have is
    /// skipped), verified; on any error the log is unchanged. Entries that
    /// branch off ours are a fork (another fleet's log when they differ at
    /// the genesis; past it, [`compare`] on the peer's head tells which);
    /// ones that start past our end leave a gap.
    pub fn extend(&mut self, newer: Vec<Entry>) -> Result<()> {
        let len = self.entries.len() as u64;
        let mut newer = newer.into_iter().peekable();
        while let Some(e) = newer.next_if(|e| e.seq() < len) {
            if self.entries.get(e.seq() as usize) != Some(&e) {
                bail!("{}", fork_at(e.seq()));
            }
        }
        let newer: Vec<Entry> = newer.collect();
        let Some(first) = newer.first() else { return Ok(()) };
        if first.seq() > len {
            bail!("membership entries from {} don't continue ours ({len} entries): entries {len}..{} are missing", first.seq(), first.seq());
        }
        if first.prev != self.head().hash {
            bail!("{}", fork_at(len));
        }
        let mut m = self.membership.clone();
        for e in &newer {
            m = Some(next(m.as_ref(), e)?);
        }
        self.entries.extend(newer);
        self.membership = m;
        Ok(())
    }

    /// Our entries from `len` on: what a peer whose log has `len` lacks.
    pub fn entries_after(&self, len: u64) -> &[Entry] {
        let from = usize::try_from(len).unwrap_or(usize::MAX).min(self.entries.len());
        &self.entries[from..]
    }
}

fn fork_at(seq: u64) -> String {
    if seq == 0 {
        "these membership entries are another fleet's (another genesis)".into()
    } else {
        format!("the membership logs fork at entry {seq}: these entries don't continue ours")
    }
}

/// How a peer's log stands to ours ([`compare`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compare {
    Same,
    /// Theirs is longer: we lack its later entries ([`Log::extend`] when
    /// they arrive confirms they continue ours).
    Behind,
    /// Ours continues theirs: they lack our later entries
    /// ([`Log::entries_after`]).
    Ahead,
    /// The logs differ: `another_fleet` when their genesis isn't ours,
    /// else one authority signed two histories.
    Fork { another_fleet: bool },
}

impl fmt::Display for Compare {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Compare::Same => "the same membership log",
            Compare::Behind => "their membership log is ahead of ours: we lack its later entries",
            Compare::Ahead => "our membership log is ahead of theirs: they lack our later entries",
            Compare::Fork { another_fleet: true } => "their membership log is another fleet's (another genesis)",
            Compare::Fork { another_fleet: false } => "the membership logs fork: one authority signed two histories",
        })
    }
}

/// `theirs` against our log, on heads alone: equal is [`Compare::Same`];
/// two non-empty logs with different genesis hashes are another fleet's
/// ([`Compare::Fork`] with `another_fleet`); otherwise longer is
/// [`Compare::Behind`], shorter is [`Compare::Ahead`] when our entry at
/// its end hashes as its head (an empty log is behind every log), and the
/// rest is a fork.
pub fn compare(ours: &Log, theirs: Head) -> Compare {
    const FORK: Compare = Compare::Fork { another_fleet: false };
    let our = ours.head();
    if our == theirs {
        return Compare::Same;
    }
    if our.len > 0 && theirs.len > 0 && our.genesis != theirs.genesis {
        return Compare::Fork { another_fleet: true };
    }
    if theirs.len > our.len {
        return Compare::Behind;
    }
    if theirs.len == our.len {
        return FORK;
    }
    if theirs.len == 0 {
        return if theirs == Head::EMPTY { Compare::Ahead } else { FORK };
    }
    match ours.entries.get(theirs.len as usize - 1) {
        Some(e) if e.hash() == theirs.hash => Compare::Ahead,
        _ => FORK,
    }
}

/// A member's word for a key the authority hasn't added (decision 46):
/// `introducer` says `name` is `key`, in the fleet of one genesis. Its
/// [`Statement`] (kind [`INTRODUCTION_KIND`], by the introducer's key)
/// signs the genesis hash, the name and the key, length-prefixed; its
/// version is 0, since an introduction is checked against the log, not
/// against an introducer's earlier ones.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Introduction {
    pub name: String,
    pub key: PublicKey,
    pub introducer: String,
    pub statement: Statement,
}

impl Introduction {
    fn payload(genesis: &[u8; 32], name: &str, key: &PublicKey) -> Vec<u8> {
        let mut out = Vec::new();
        for f in [&genesis[..], name.as_bytes(), key.as_bytes()] {
            put(&mut out, f);
        }
        out
    }

    /// `introducer`, with `signer`, introducing `name` as `key` in the
    /// fleet of genesis hash `genesis`. Unchecked: [`check_introduction`] is.
    pub fn sign(genesis: &[u8; 32], name: &str, key: PublicKey, introducer: &str, signer: &impl Signer) -> Introduction {
        let statement = Statement::sign(introducer, signer, INTRODUCTION_KIND, &Introduction::payload(genesis, name, &key), 0);
        Introduction { name: name.to_string(), key, introducer: introducer.to_string(), statement }
    }
}

/// `i` holds in `m`: signed (as an introduction, for this fleet) by its
/// introducer's key in the log, the introducer a member of the log (an
/// introduced member is capped and introduces no one, decision 51), and
/// neither its name nor its key in the log already.
pub fn check_introduction(m: &Membership, i: &Introduction) -> Result<()> {
    let s = &i.statement;
    if s.kind != INTRODUCTION_KIND || s.signer != i.introducer {
        bail!("not an introduction by `{}`", i.introducer);
    }
    let Some(key) = m.key_of(&i.introducer) else {
        bail!("`{}` is not a member of the log: only a member it has added introduces others", i.introducer);
    };
    if s.key != key {
        bail!("signed with {}, not `{}`'s key {key}", s.key, i.introducer);
    }
    check_member_name(&i.name).with_context(|| format!("member name `{}`", i.name))?;
    if m.members.contains_key(&i.name) {
        bail!("`{}` is already a member", i.name);
    }
    if m.revoked.contains(&i.key) {
        bail!("{} is revoked", i.key);
    }
    if let Some(other) = m.name_of(&i.key) {
        bail!("{} is already `{other}`'s key", i.key);
    }
    s.verify_payload(&Introduction::payload(&m.genesis, &i.name, &i.key))
        .context("not signed for this fleet (another fleet's introduction, or tampered)")
}

/// This host's `hub` in `member.toml`, from the log's (`own`: this host's
/// member name).
pub fn hub_for(m: &Membership, own: &str) -> member::Hub {
    if m.hub == own { member::Hub::This } else { member::Hub::Other(m.hub.clone()) }
}

/// `membership.json`'s contents, verified: the log and the introductions
/// held next to it, each one checking against the log, one per name and
/// per key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stored {
    pub log: Log,
    pub introductions: Vec<Introduction>,
}

/// [`Stored`] as JSON: the file's, and the `membership.*` calls' (unchecked
/// until [`Stored::from_wire`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Wire {
    pub entries: Vec<Entry>,
    #[serde(default)]
    pub introductions: Vec<Introduction>,
}

impl Stored {
    /// `wire` verified: a log that doesn't [`apply`] or an introduction
    /// that doesn't check refuses the whole of it.
    pub fn from_wire(wire: Wire) -> Result<Stored> {
        let stored = Stored { log: Log::new(wire.entries)?, introductions: wire.introductions };
        stored.check()?;
        Ok(stored)
    }

    pub fn to_wire(&self) -> Wire {
        Wire { entries: self.log.entries.clone(), introductions: self.introductions.clone() }
    }

    /// `i` held (once), if it checks against the log and no other held
    /// one names its name or key; refused, nothing changes.
    pub fn introduce(&mut self, i: Introduction) -> Result<()> {
        let m = self.log.membership().context("in no fleet's membership log: nothing to introduce into")?;
        check_introduction(m, &i)?;
        if self.introductions.contains(&i) {
            return Ok(());
        }
        let mut next = self.introductions.clone();
        next.push(i);
        let candidate = Stored { log: self.log.clone(), introductions: next };
        candidate.check()?;
        self.introductions = candidate.introductions;
        Ok(())
    }

    /// The introductions the log has settled dropped.
    fn settle(&mut self) {
        if let Some(m) = &self.log.membership {
            m.drop_settled(&mut self.introductions);
        }
    }

    /// Every introduction checks against the log, one per name and key.
    fn check(&self) -> Result<()> {
        if self.introductions.is_empty() {
            return Ok(());
        }
        let m = self.log.membership().context("introductions with no membership log")?;
        let (mut names, mut keys) = (BTreeSet::new(), BTreeSet::new());
        for i in &self.introductions {
            check_introduction(m, i).with_context(|| format!("introduction of `{}` by `{}`", i.name, i.introducer))?;
            if !names.insert(&i.name) || !keys.insert(i.key) {
                bail!("two introductions of `{}` or of {}", i.name, i.key);
            }
        }
        Ok(())
    }
}

/// `membership.json` next to the `member.toml` at `member_toml` (both next
/// to `state.toml`), as `member::key_path` is: a test daemon's member file
/// carries its fleet identity with it.
pub fn path(member_toml: &Path) -> PathBuf {
    member_toml.with_file_name("membership.json")
}

/// `text` as [`Stored`], verified ([`Stored::from_wire`]).
pub fn parse(text: &str) -> Result<Stored> {
    Stored::from_wire(serde_json::from_str(text)?)
}

/// `stored` as the file's JSON; refused unless it verifies.
pub fn serialize(stored: &Stored) -> Result<String> {
    stored.check()?;
    Ok(serde_json::to_string_pretty(&stored.to_wire()).context("cannot serialize membership.json")? + "\n")
}

/// The file at `path`; missing is `None` (in no fleet's log). A file that
/// doesn't verify is an error, never dropped.
pub fn load(path: &Path) -> Result<Option<Stored>> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text).map(Some).with_context(|| format!("invalid {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// Read-modify-write of the file at `path` under its lock
/// (`serve::locked_file`; a missing file is the empty log): `f` changes it,
/// the introductions the log has settled are dropped, and it's saved only
/// if it changed and verifies. A file that doesn't verify fails the update
/// rather than being overwritten.
pub fn update<T>(path: &Path, f: impl FnOnce(&mut Stored) -> T) -> Result<T> {
    crate::serve::locked_file::update(path, |p| Ok(load(p)?.unwrap_or_default()), serialize, |s| {
        let out = f(s);
        s.settle();
        out
    })
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::serve::fleet::identity::SigningKey;

    fn key(n: u8) -> SigningKey {
        SigningKey::from_seed([n; 32])
    }

    fn pk(n: u8) -> PublicKey {
        key(n).public_key()
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dsv-membership-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// A genesis by `lap` (key 1) with hub `hub`, fleet id `[id; 16]`.
    fn genesis(id: u8, hub: &str) -> Op {
        Op::genesis_with_id([id; 16], "lap", pk(1), hub)
    }

    /// `lap` (key 1) the authority, `box` (key 2) the hub, `pi` (key 3);
    /// fleet id `[1; 16]`.
    fn fleet() -> Log {
        let mut log = Log::default();
        log.append(genesis(1, "box"), "lap", &key(1)).unwrap();
        log.append(Op::Add { name: "box".into(), key: pk(2) }, "lap", &key(1)).unwrap();
        log.append(Op::Add { name: "pi".into(), key: pk(3) }, "lap", &key(1)).unwrap();
        log
    }

    fn refused(log: &Log, op: Op) -> String {
        let mut l = log.clone();
        let e = format!("{:#}", l.append(op, "lap", &key(1)).unwrap_err());
        assert_eq!(&l, log, "a refused append changes nothing");
        e
    }

    #[test]
    fn a_valid_chain_applies() {
        let mut log = fleet();
        let m = log.membership().unwrap().clone();
        assert_eq!(m.authority, ("lap".to_string(), pk(1)));
        assert_eq!(m.hub, "box");
        assert_eq!(m.members, BTreeMap::from([("lap".into(), pk(1)), ("box".into(), pk(2)), ("pi".into(), pk(3))]));
        assert!(m.revoked.is_empty());
        assert_eq!(m.head, Head { len: 3, hash: log.entries()[2].hash(), genesis: log.entries()[0].hash() });
        assert_eq!(m.genesis, log.entries()[0].hash());
        assert_eq!((m.key_of("pi"), m.name_of(&pk(2)), m.hub_key()), (Some(pk(3)), Some("box"), Some(pk(2))));
        assert!(m.is_member_key(&pk(3)) && !m.is_member_key(&pk(4)));
        assert!(m.is_authority(&pk(1)) && !m.is_authority(&pk(2)));
        assert_eq!(apply(log.entries()).unwrap(), m);
        assert_eq!(Log::new(log.entries().to_vec()).unwrap(), log);
        assert_eq!((hub_for(&m, "box"), hub_for(&m, "lap")), (member::Hub::This, member::Hub::Other("box".into())));

        log.append(Op::Hub { name: "pi".into() }, "lap", &key(1)).unwrap();
        log.append(Op::Revoke { name: "box".into(), key: pk(2) }, "lap", &key(1)).unwrap();
        let m = log.membership().unwrap();
        assert_eq!((m.hub.as_str(), m.key_of("box"), m.head.len), ("pi", None, 5));
        assert!(m.revoked.contains(&pk(2)));
        // A revoked name may come back with a new key.
        log.append(Op::Add { name: "box".into(), key: pk(4) }, "lap", &key(1)).unwrap();
        assert_eq!(log.membership().unwrap().key_of("box"), Some(pk(4)));
        // Genesis's hub need not be added yet: no hub key until it is.
        let mut early = Log::default();
        early.append(genesis(1, "box"), "lap", &key(1)).unwrap();
        assert_eq!(early.membership().unwrap().hub_key(), None);
    }

    #[test]
    fn each_rule_refuses() {
        let log = fleet();
        let g = log.membership().unwrap().genesis;
        let add = |name: &str, k: u8| Op::Add { name: name.into(), key: pk(k) };
        let mut revoked = log.clone();
        revoked.append(Op::Revoke { name: "pi".into(), key: pk(3) }, "lap", &key(1)).unwrap();
        let cases: Vec<(&Log, Op, &str)> = vec![
            (&log, genesis(1, "lap"), "a second genesis"),
            (&log, add("pi", 9), "already a member"),
            (&log, add("new", 3), "already `pi`'s key"),
            (&revoked, add("new", 3), "revoked"),
            (&log, add("self", 9), "reserved"),
            (&log, add("a/b", 9), "member name"),
            (&log, Op::Revoke { name: "nope".into(), key: pk(3) }, "not a member"),
            (&log, Op::Revoke { name: "pi".into(), key: pk(2) }, "not ed25519"),
            (&log, Op::Revoke { name: "lap".into(), key: pk(1) }, "is the authority"),
            (&log, Op::Revoke { name: "box".into(), key: pk(2) }, "is the hub"),
            (&log, Op::Rotate { name: "nope".into(), old: pk(3), new: pk(9), by_old: countersign_rotate(&g, "nope", &pk(9), &key(3)) }, "not a member"),
            (&log, Op::Rotate { name: "pi".into(), old: pk(2), new: pk(9), by_old: countersign_rotate(&g, "pi", &pk(9), &key(2)) }, "`pi`'s key is"),
            (&log, Op::Rotate { name: "pi".into(), old: pk(3), new: pk(9), by_old: countersign_rotate(&g, "pi", &pk(8), &key(3)) }, "isn't countersigned"),
            (&log, Op::Rotate { name: "pi".into(), old: pk(3), new: pk(9), by_old: countersign_rotate(&g, "pi", &pk(9), &key(1)) }, "isn't countersigned"),
            (&log, Op::Rotate { name: "pi".into(), old: pk(3), new: pk(9), by_old: countersign_rotate(&[7; 32], "pi", &pk(9), &key(3)) }, "isn't countersigned"),
            (&log, Op::Rotate { name: "pi".into(), old: pk(3), new: pk(2), by_old: countersign_rotate(&g, "pi", &pk(2), &key(3)) }, "already `box`'s key"),
            (&log, Op::Hub { name: "nope".into() }, "can't be the hub"),
        ];
        for (l, op, want) in cases {
            let e = refused(l, op.clone());
            assert!(e.contains(want), "{op}: {e}");
            assert!(e.contains(&format!("membership entry {}", l.head().len)), "{e}");
        }
        // A key rotated away is revoked: never anyone's again.
        let mut rotated = log.clone();
        rotated.append(Op::Rotate { name: "pi".into(), old: pk(3), new: pk(9), by_old: countersign_rotate(&g, "pi", &pk(9), &key(3)) }, "lap", &key(1)).unwrap();
        assert!(refused(&rotated, add("new", 3)).contains("revoked"));
        let back = Op::Rotate { name: "pi".into(), old: pk(9), new: pk(3), by_old: countersign_rotate(&g, "pi", &pk(3), &key(9)) };
        assert!(refused(&rotated, back).contains("revoked"));

        // The first entry: genesis only, signed by the key and name it names.
        let empty = Log::default();
        assert!(refused(&empty, add("box", 2)).contains("must be `genesis`"));
        let mut l = Log::default();
        let e = l.append(Op::genesis_with_id([1; 16], "lap", pk(2), "lap"), "lap", &key(1)).unwrap_err();
        assert!(format!("{e:#}").contains("not the authority it names"), "{e:#}");
        let e = l.append(genesis(1, "lap"), "box", &key(1)).unwrap_err();
        assert!(format!("{e:#}").contains("not the authority it names"), "{e:#}");
        let e = l.append(genesis(1, "self"), "lap", &key(1)).unwrap_err();
        assert!(format!("{e:#}").contains("hub `self`"), "{e:#}");
        assert_eq!(l, Log::default());
        assert!(format!("{:#}", apply(&[]).unwrap_err()).contains("no genesis"));
    }

    #[test]
    fn tampering_with_any_field_is_refused() {
        let log = fleet();
        let entries = log.entries().to_vec();
        let mutations: Vec<(&str, Box<dyn Fn(&mut Entry)>)> = vec![
            ("op name", Box::new(|e| match &mut e.op {
                Op::Genesis { authority: n, .. } | Op::Add { name: n, .. } | Op::Revoke { name: n, .. } | Op::Rotate { name: n, .. } | Op::Hub { name: n } => n.push('x'),
            })),
            ("op key", Box::new(|e| match &mut e.op {
                Op::Genesis { key: k, .. } | Op::Add { key: k, .. } | Op::Revoke { key: k, .. } => *k = pk(9),
                _ => unreachable!(),
            })),
            ("genesis id", Box::new(|e| match &mut e.op {
                Op::Genesis { id, .. } => id[15] ^= 1,
                _ => e.prev[0] ^= 1,
            })),
            ("op kind", Box::new(|e| if let Op::Add { name, key } = e.op.clone() { e.op = Op::Revoke { name, key } } else { e.op = Op::Hub { name: "lap".into() } })),
            ("prev", Box::new(|e| e.prev[31] ^= 1)),
            ("signer", Box::new(|e| e.statement.signer = "box".into())),
            ("key", Box::new(|e| e.statement.key = pk(2))),
            ("kind", Box::new(|e| e.statement.kind = INTRODUCTION_KIND.into())),
            ("payload", Box::new(|e| e.statement.payload_sha256[0] ^= 1)),
            ("version", Box::new(|e| e.statement.version += 1)),
            ("sig", Box::new(|e| {
                let mut b = e.statement.sig.to_bytes();
                b[5] ^= 1;
                e.statement.sig = Signature::from_bytes(b);
            })),
        ];
        for i in 0..entries.len() {
            for (what, change) in &mutations {
                let mut t = entries.clone();
                change(&mut t[i]);
                assert!(apply(&t).is_err(), "entry {i}: {what}");
            }
        }
        // Any character of the file: it either doesn't parse or doesn't verify.
        let text = serialize(&Stored { log, introductions: vec![] }).unwrap();
        let bytes = text.as_bytes();
        let mut tried = 0;
        for (at, &b) in bytes.iter().enumerate().step_by(5) {
            let swap = match b {
                b'0'..=b'8' | b'a'..=b'e' => b + 1,
                b'9' => b'0',
                b'f'..=b'z' => b'a',
                _ => continue,
            };
            let mut t = bytes.to_vec();
            t[at] = swap;
            assert!(parse(std::str::from_utf8(&t).unwrap()).is_err(), "byte {at}: {}", String::from_utf8_lossy(&t));
            tried += 1;
        }
        assert!(tried > 200, "{tried}");
    }

    #[test]
    fn reordered_or_missing_entries_are_refused() {
        let mut log = fleet();
        log.append(Op::Hub { name: "pi".into() }, "lap", &key(1)).unwrap();
        let e = log.entries().to_vec();
        let msg = |v: Vec<Entry>| format!("{:#}", apply(&v).unwrap_err());
        assert!(msg(vec![e[0].clone(), e[2].clone(), e[1].clone(), e[3].clone()]).contains("out of order"));
        assert!(msg(vec![e[0].clone(), e[1].clone(), e[3].clone()]).contains("numbered 3, expected 2"));
        assert!(msg(e[1..].to_vec()).contains("numbered 1, expected 0"));
        assert!(msg(vec![e[0].clone(), e[0].clone()]).contains("numbered 0, expected 1"));
        // Renumbered to fit: the signature (and the chain) give it away.
        let mut skipped = e[2].clone();
        skipped.statement.version = 1;
        assert!(apply(&[e[0].clone(), skipped]).is_err());
    }

    #[test]
    fn only_the_authority_signs() {
        let log = fleet();
        let h = log.head();
        let by_box = Entry::sign(h.len, h.hash, Op::Add { name: "x".into(), key: pk(9) }, "box", &key(2));
        let mut l = log.clone();
        let e = format!("{:#}", l.extend(vec![by_box]).unwrap_err());
        assert!(e.contains("not the authority `lap`"), "{e}");
        let in_its_name = Entry::sign(h.len, h.hash, Op::Add { name: "x".into(), key: pk(9) }, "lap", &key(2));
        assert!(format!("{:#}", l.extend(vec![in_its_name]).unwrap_err()).contains("not the authority"));
        assert_eq!(l, log);
    }

    #[test]
    fn a_rotated_authority_signs_with_its_new_key() {
        let mut log = fleet();
        let g = log.membership().unwrap().genesis;
        let by_old = countersign_rotate(&g, "lap", &pk(9), &key(1));
        log.append(Op::Rotate { name: "lap".into(), old: pk(1), new: pk(9), by_old }, "lap", &key(1)).unwrap();
        let m = log.membership().unwrap();
        assert_eq!(m.authority, ("lap".to_string(), pk(9)));
        assert!(m.is_authority(&pk(9)) && m.revoked.contains(&pk(1)));
        let e = log.clone().append(Op::Add { name: "x".into(), key: pk(5) }, "lap", &key(1)).unwrap_err();
        assert!(format!("{e:#}").contains("not the authority"), "{e:#}");
        log.append(Op::Add { name: "x".into(), key: pk(5) }, "lap", &key(9)).unwrap();
        assert_eq!(apply(log.entries()).unwrap().key_of("x"), Some(pk(5)));
    }

    #[test]
    fn compare_says_same_behind_ahead_or_fork() {
        let log = fleet();
        let mut longer = log.clone();
        longer.append(Op::Hub { name: "pi".into() }, "lap", &key(1)).unwrap();
        let mut other = log.clone();
        other.append(Op::Add { name: "x".into(), key: pk(9) }, "lap", &key(1)).unwrap();
        assert_eq!(compare(&log, log.head()), Compare::Same);
        assert_eq!(compare(&log, longer.head()), Compare::Behind);
        assert_eq!(compare(&longer, log.head()), Compare::Ahead);
        let fork = Compare::Fork { another_fleet: false };
        assert_eq!(compare(&longer, other.head()), fork, "same length, another entry");
        other.append(Op::Hub { name: "x".into() }, "lap", &key(1)).unwrap();
        assert_eq!(compare(&other, longer.head()), fork, "shorter, not a prefix");
        assert_eq!(compare(&Log::default(), log.head()), Compare::Behind);
        assert_eq!(compare(&log, Head::EMPTY), Compare::Ahead);
        assert_eq!(compare(&Log::default(), Head::EMPTY), Compare::Same);
        assert_eq!(compare(&log, Head { hash: [1; 32], ..Head::EMPTY }), fork);
        assert!(fork.to_string().contains("two histories"));

        // The same authority, key and hub, made again: another fleet id,
        // another genesis, another fleet, whatever the lengths.
        let mut again = Log::default();
        again.append(genesis(2, "box"), "lap", &key(1)).unwrap();
        assert_ne!(again.head().genesis, log.head().genesis);
        let other_fleet = Compare::Fork { another_fleet: true };
        assert_eq!(compare(&log, again.head()), other_fleet, "shorter");
        assert_eq!(compare(&again, log.head()), other_fleet, "longer");
        again.append(Op::Add { name: "box".into(), key: pk(2) }, "lap", &key(1)).unwrap();
        again.append(Op::Add { name: "pi".into(), key: pk(3) }, "lap", &key(1)).unwrap();
        assert_eq!(compare(&log, again.head()), other_fleet, "same length, same ops");
        assert!(other_fleet.to_string().contains("another fleet"));
        // An empty log has no fleet yet: it's behind any.
        assert_eq!(compare(&Log::default(), again.head()), Compare::Behind);
    }

    #[test]
    fn genesis_draws_a_fresh_fleet_id() {
        let (a, b) = (Op::genesis("lap", pk(1), "box").unwrap(), Op::genesis("lap", pk(1), "box").unwrap());
        let (Op::Genesis { id: ia, .. }, Op::Genesis { id: ib, .. }) = (&a, &b) else { panic!() };
        assert_ne!(ia, ib);
        assert_ne!(*ia, [0; 16]);
        let (mut la, mut lb) = (Log::default(), Log::default());
        la.append(a, "lap", &key(1)).unwrap();
        lb.append(b, "lap", &key(1)).unwrap();
        assert_ne!(la.head().genesis, lb.head().genesis);
        assert_eq!(compare(&la, lb.head()), Compare::Fork { another_fleet: true });
    }

    #[test]
    fn extend_takes_what_continues_ours_and_refuses_forks() {
        let mut ahead = fleet();
        ahead.append(Op::Hub { name: "pi".into() }, "lap", &key(1)).unwrap();
        ahead.append(Op::Add { name: "x".into(), key: pk(9) }, "lap", &key(1)).unwrap();
        let mut behind = fleet();
        behind.extend(ahead.entries_after(behind.head().len).to_vec()).unwrap();
        assert_eq!(behind, ahead);
        // From scratch, with what we have already, or nothing new.
        let mut fresh = Log::default();
        fresh.extend(ahead.entries_after(0).to_vec()).unwrap();
        assert_eq!(fresh, ahead);
        let mut overlap = fleet();
        overlap.extend(ahead.entries_after(1).to_vec()).unwrap();
        assert_eq!(overlap, ahead);
        overlap.extend(vec![]).unwrap();
        overlap.extend(ahead.entries().to_vec()).unwrap();
        assert_eq!(overlap, ahead);
        assert!(ahead.entries_after(99).is_empty());

        // A gap, a fork, another fleet: refused, the log unchanged.
        let mut l = fleet();
        let e = format!("{:#}", l.extend(ahead.entries_after(4).to_vec()).unwrap_err());
        assert!(e.contains("missing"), "{e}");
        assert_eq!(l, fleet());
        let mut l = fleet();
        l.append(Op::Hub { name: "pi".into() }, "lap", &key(1)).unwrap();
        let before = l.clone();
        let mut fork = fleet();
        fork.append(Op::Add { name: "y".into(), key: pk(8) }, "lap", &key(1)).unwrap();
        fork.append(Op::Hub { name: "y".into() }, "lap", &key(1)).unwrap();
        let e = format!("{:#}", l.extend(fork.entries_after(3).to_vec()).unwrap_err());
        assert!(e.contains("fork at entry 3"), "an entry we have differs: {e}");
        let e = format!("{:#}", l.extend(fork.entries_after(4).to_vec()).unwrap_err());
        assert!(e.contains("fork at entry 4"), "doesn't follow our head: {e}");
        // The same authority and hub made again: another fleet's entries.
        let mut two = Log::default();
        two.append(genesis(2, "box"), "lap", &key(1)).unwrap();
        two.append(Op::Add { name: "box".into(), key: pk(2) }, "lap", &key(1)).unwrap();
        let e = format!("{:#}", l.extend(two.entries().to_vec()).unwrap_err());
        assert!(e.contains("another fleet's") && e.contains("another genesis"), "{e}");
        // An entry that continues ours but doesn't verify.
        let h = l.head();
        let bad = Entry::sign(h.len, h.hash, Op::Hub { name: "nope".into() }, "lap", &key(1));
        assert!(l.extend(vec![bad]).is_err());
        assert_eq!(l, before);
    }

    /// `fleet()` with `box` introducing `new` (key 5).
    fn introduced() -> (Log, Introduction) {
        let log = fleet();
        let g = log.membership().unwrap().genesis;
        (log, Introduction::sign(&g, "new", pk(5), "box", &key(2)))
    }

    #[test]
    fn introductions_check_against_the_log() {
        let (log, i) = introduced();
        let m = log.membership().unwrap();
        check_introduction(m, &i).unwrap();
        let g = m.genesis;
        let err = |i: &Introduction| format!("{:#}", check_introduction(m, i).unwrap_err());
        assert!(err(&Introduction::sign(&g, "new", pk(5), "stranger", &key(8))).contains("not a member of the log"));
        // An introduced member is capped: it introduces no one.
        assert!(err(&Introduction::sign(&g, "other", pk(6), "new", &key(5))).contains("not a member of the log"));
        assert!(err(&Introduction::sign(&g, "new", pk(5), "box", &key(3))).contains("not `box`'s key"));
        assert!(err(&Introduction::sign(&g, "pi", pk(5), "box", &key(2))).contains("already a member"));
        assert!(err(&Introduction::sign(&g, "new", pk(3), "box", &key(2))).contains("already `pi`'s key"));
        // Replayed into another fleet: the same authority, key and hub made
        // again, only the fleet id differs.
        let mut two = Log::default();
        two.append(genesis(2, "box"), "lap", &key(1)).unwrap();
        two.append(Op::Add { name: "box".into(), key: pk(2) }, "lap", &key(1)).unwrap();
        assert!(format!("{:#}", check_introduction(two.membership().unwrap(), &i).unwrap_err()).contains("this fleet"));
        // Tampered.
        let renamed = Introduction { name: "newer".into(), ..i.clone() };
        assert!(err(&renamed).contains("this fleet"));
        let rekeyed = Introduction { key: pk(6), ..i.clone() };
        assert!(err(&rekeyed).contains("this fleet"));
        let mut versioned = i.clone();
        versioned.statement.version = 1;
        assert!(check_introduction(m, &versioned).is_err());
        let as_entry = Introduction { statement: Statement { kind: KIND.into(), ..i.statement.clone() }, ..i.clone() };
        assert!(err(&as_entry).contains("not an introduction"));
        // A revoked key.
        let mut l = log.clone();
        l.append(Op::Add { name: "gone".into(), key: pk(6) }, "lap", &key(1)).unwrap();
        l.append(Op::Revoke { name: "gone".into(), key: pk(6) }, "lap", &key(1)).unwrap();
        let again = Introduction::sign(&g, "back", pk(6), "box", &key(2));
        assert!(format!("{:#}", check_introduction(l.membership().unwrap(), &again).unwrap_err()).contains("revoked"));
    }

    #[test]
    fn the_log_settles_introductions() {
        let (log, i) = introduced();
        let g = log.membership().unwrap().genesis;
        let by_pi = Introduction::sign(&g, "pi2", pk(6), "pi", &key(3));
        let mut held = vec![i.clone(), by_pi.clone()];
        log.membership().unwrap().drop_settled(&mut held);
        assert_eq!(held.len(), 2, "nothing settled yet");
        // Added (same name and key): dropped.
        let mut added = log.clone();
        added.append(Op::Add { name: "new".into(), key: pk(5) }, "lap", &key(1)).unwrap();
        added.membership().unwrap().drop_settled(&mut held);
        assert_eq!(held, [by_pi.clone()]);
        // The introducer revoked: dropped.
        let mut held = vec![i.clone(), by_pi.clone()];
        let mut l = log.clone();
        l.append(Op::Revoke { name: "pi".into(), key: pk(3) }, "lap", &key(1)).unwrap();
        l.membership().unwrap().drop_settled(&mut held);
        assert_eq!(held, [i.clone()]);
        // The introducer's key rotated away, or the name added with another key.
        let mut l = log.clone();
        let by_old = countersign_rotate(&g, "box", &pk(9), &key(2));
        l.append(Op::Rotate { name: "box".into(), old: pk(2), new: pk(9), by_old }, "lap", &key(1)).unwrap();
        l.append(Op::Add { name: "pi2".into(), key: pk(7) }, "lap", &key(1)).unwrap();
        let mut held = vec![i, by_pi];
        l.membership().unwrap().drop_settled(&mut held);
        assert!(held.is_empty());
    }

    #[test]
    fn the_wire_forms() {
        let log = fleet();
        let e = &log.entries()[1];
        let wire = serde_json::to_value(e).unwrap();
        assert_eq!((&wire["op"], &wire["name"], &wire["key"]), (&json!("add"), &json!("box"), &json!(pk(2).to_string())));
        assert_eq!(wire["prev"], hex::encode(log.entries()[0].hash()));
        assert_eq!(wire["statement"]["kind"], "membership");
        assert_eq!(wire["statement"]["version"], 1);
        assert_eq!(serde_json::from_value::<Entry>(wire).unwrap(), *e);
        let g = serde_json::to_value(&log.entries()[0]).unwrap();
        assert_eq!((&g["op"], &g["authority"], &g["hub"], &g["prev"]), (&json!("genesis"), &json!("lap"), &json!("box"), &json!("0".repeat(64))));
        assert_eq!(g["id"], "01".repeat(16));
        assert_eq!(serde_json::from_value::<Entry>(g.clone()).unwrap(), log.entries()[0]);
        let mut bad = g.clone();
        bad["id"] = json!("01".repeat(15));
        assert!(serde_json::from_value::<Entry>(bad.clone()).is_err(), "short id");
        bad["id"] = json!("0A".repeat(16));
        assert!(serde_json::from_value::<Entry>(bad).is_err(), "uppercase id");
        let rot = Op::Rotate { name: "pi".into(), old: pk(3), new: pk(9), by_old: countersign_rotate(&[0; 32], "pi", &pk(9), &key(3)) };
        let w = serde_json::to_value(&rot).unwrap();
        assert_eq!(w.as_object().unwrap().keys().collect::<Vec<_>>(), ["by_old", "name", "new", "old", "op"]);
        assert_eq!(serde_json::from_value::<Op>(w).unwrap(), rot);
        assert_eq!(serde_json::to_value(Op::Hub { name: "pi".into() }).unwrap(), json!({"op": "hub", "name": "pi"}));
        assert!(serde_json::from_value::<Op>(json!({"op": "promote", "name": "pi"})).is_err());
        let head = log.head();
        let head_wire = json!({"len": 3, "hash": hex::encode(head.hash), "genesis": hex::encode(log.entries()[0].hash())});
        assert_eq!(serde_json::to_value(head).unwrap(), head_wire);
        assert_eq!(serde_json::from_value::<Head>(head_wire).unwrap(), head);
        let (_, i) = introduced();
        let w = serde_json::to_value(&i).unwrap();
        assert_eq!((&w["name"], &w["introducer"], &w["statement"]["kind"]), (&json!("new"), &json!("box"), &json!("introduction")));
        assert_eq!(serde_json::from_value::<Introduction>(w).unwrap(), i);
    }

    #[test]
    fn the_file_round_trips_and_refuses_what_doesnt_verify() {
        assert_eq!(path(Path::new("/xdg/devsandbox/member.toml")), PathBuf::from("/xdg/devsandbox/membership.json"));
        let dir = scratch("store");
        let path = dir.join("membership.json");
        assert!(load(&path).unwrap().is_none(), "missing: in no fleet's log");
        // Introductions need a log.
        let (log, i) = introduced();
        let e = update(&path, |s| s.introduce(i.clone())).unwrap().unwrap_err();
        assert!(format!("{e:#}").contains("no fleet"), "{e:#}");
        assert!(!path.exists(), "nothing changed, nothing written");

        update(&path, |s| {
            s.log.extend(log.entries().to_vec()).unwrap();
            s.introduce(i.clone()).unwrap();
            s.introduce(i.clone()).unwrap();
        })
        .unwrap();
        let stored = load(&path).unwrap().unwrap();
        assert_eq!((stored.log.clone(), stored.introductions.clone()), (log.clone(), vec![i.clone()]), "held once");
        assert!(dir.join("membership.lock").exists(), "written through locked_file");
        let text = std::fs::read_to_string(&path).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v.as_object().unwrap().keys().collect::<Vec<_>>(), ["entries", "introductions"]);

        // A conflicting introduction is refused; the file stays as it was.
        let g = log.membership().unwrap().genesis;
        let dup = Introduction::sign(&g, "new", pk(6), "pi", &key(3));
        let e = update(&path, |s| s.introduce(dup)).unwrap().unwrap_err();
        assert!(format!("{e:#}").contains("two introductions"), "{e:#}");
        assert_eq!(load(&path).unwrap().unwrap().introductions, [i.clone()]);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);

        // Once the log adds it, a save drops the introduction.
        update(&path, |s| {
            s.log.append(Op::Add { name: "new".into(), key: pk(5) }, "lap", &key(1)).unwrap();
        })
        .unwrap();
        let stored = load(&path).unwrap().unwrap();
        assert!(stored.introductions.is_empty());
        assert_eq!(stored.log.membership().unwrap().key_of("new"), Some(pk(5)));

        // A tampered file is refused, not dropped, and never overwritten.
        let text = std::fs::read_to_string(&path).unwrap();
        let tampered = text.replacen("\"pi\"", "\"px\"", 1);
        assert_ne!(tampered, text);
        std::fs::write(&path, &tampered).unwrap();
        let e = format!("{:#}", load(&path).unwrap_err());
        assert!(e.contains("invalid") && e.contains("membership.json"), "{e}");
        assert!(update(&path, |s| s.introductions.clear()).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), tampered);
        // So is a held introduction that doesn't check.
        let mut v: Value = serde_json::from_str(&text).unwrap();
        v["introductions"] = json!([serde_json::to_value(Introduction::sign(&g, "z", pk(8), "nobody", &key(8))).unwrap()]);
        std::fs::write(&path, v.to_string()).unwrap();
        assert!(format!("{:#}", load(&path).unwrap_err()).contains("not a member of the log"));
        std::fs::write(&path, "{\"entries\": [], \"later\": 1}").unwrap();
        assert!(load(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
