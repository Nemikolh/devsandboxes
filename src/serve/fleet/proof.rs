//! A peer link's proof of both members' keys (docs/architecture/daemon/fleet.md,
//! *Peer links*; fleet plan step 10a.4, decisions 44-46, 50-52): run by
//! `link::opener_with` / `link::answerer_with` right after the `Hello`s,
//! on the raw byte pipe before the mux, when their `Endpoint` carries an
//! [`Auth`].
//!
//! Each message is a `u32` big-endian length, then a tag byte (`0` a
//! message, `1` a refusal whose body says why in UTF-8), then the body,
//! within a bound per round. The opener writes each round's message first
//! and then reads the answerer's; the answerer reads first, then writes, so
//! neither end ever blocks writing while the other does too. An end that
//! refuses writes its refusal in place of its next message; the peer
//! reports it as the peer's refusal.
//!
//! 1. `hello`: the member name, a 32-byte nonce from the OS RNG, the
//!    membership log's [`Head`]. The name must be the one this end expects
//!    (the dialer's entry name, `fleet-link`'s `--peer`); the heads are
//!    compared ([`membership::compare`]): another fleet's or a fork refuses
//!    the link, logged.
//! 2. `sync`: what the peer lacks, as [`Wire`] JSON: the entries past its
//!    head when ours is ahead, and every introduction held. The end behind
//!    extends a copy of its log ([`Log::extend`](membership::Log::extend):
//!    each entry authority-signed and chained to its head, so any member
//!    can carry them) and holds the introductions that check; one that
//!    doesn't is logged and dropped. Then it resolves the peer's name to a
//!    key: a member's in the log, else a held introduction's (the peer is
//!    capped, `Proven::introduced_by`). An unknown name is refused.
//! 3. `proof`: the end's public key and its signature, in [`DOMAIN`], over
//!    the [`transcript`] (both names, nonces, `Hello`s and heads) and its
//!    own role. The key must be the one the name resolved to (a revoked one
//!    says so), the signature must verify.
//! 4. `confirm`: the opener's verdict on the answerer's proof (empty, or a
//!    refusal), so both ends agree the link is proven before either serves.
//!
//! What the end behind received comes back as [`Proven::newer`], for the
//! caller to store through its daemon (`membership.adopt`).

use std::io::{Read, Write};
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};

use super::identity::{FileKeyStore, PublicKey, Signature, Signer, SigningKey};
use super::member;
use super::membership::{self, Compare, Head, Stored, Wire};
use super::membership_api::lists;
use super::update;

/// The domain a link proof's signature is made in.
pub const DOMAIN: &str = "devsandbox/link/v1";

/// The bound of a `hello`, `proof` or `confirm` message's body.
const SMALL: usize = 4096;
/// The bound of a `sync` message's body (entries and introductions as JSON).
const SYNC_LIMIT: usize = 8 << 20;

const MESSAGE: u8 = 0;
const REFUSAL: u8 = 1;

/// What every refusal for a missing identity ends with (decision 52).
const MAKE_AGAIN: &str = "a fleet link proves both members' keys against the fleet's membership log: make the fleet again with `devsandbox machine add`";

/// This member as a link proves it: its name, its key, its stored log.
#[derive(Debug, Clone)]
pub struct Identity {
    pub name: String,
    pub key: SigningKey,
    pub stored: Stored,
}

impl Identity {
    /// This host's identity, read now (each link reads it at its start):
    /// the name `member.toml` at `member_toml` gives (else `hostname`'s),
    /// `member.key` and `membership.json` next to it. No key, no log, or a
    /// log that doesn't list this host under that name and key is an error
    /// naming the fix.
    pub fn load(member_toml: &Path, hostname: &str) -> Result<Identity> {
        let name = member::load(member_toml)?.unwrap_or_default().name_or(hostname)?;
        let key_path = member::key_path(member_toml);
        let key = FileKeyStore::new(key_path.clone())
            .load()?
            .with_context(|| format!("this host has no member key ({}): {MAKE_AGAIN}", key_path.display()))?;
        let log_path = membership::path(member_toml);
        let stored = membership::load(&log_path)?
            .filter(|s| s.log.membership().is_some())
            .with_context(|| format!("this host is in no fleet's membership log ({}): {MAKE_AGAIN}", log_path.display()))?;
        if !lists(&stored, &name, &key.public_key()) {
            bail!(
                "this host's membership log doesn't list it (`{name}`, {}): {MAKE_AGAIN}",
                key.public_key().fingerprint()
            );
        }
        Ok(Identity { name, key, stored })
    }
}

/// What one end of a link proves, and whom it expects.
pub struct Auth {
    pub identity: Identity,
    /// The peer's member name: the dialer's registry entry, `fleet-link`'s
    /// `--peer`.
    pub expect: String,
    /// Where refusals worth a line go (a fork, a dropped introduction).
    pub log: fn(&str),
}

/// Which end of the link this is (`link`'s roles).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Opener,
    Answerer,
}

impl Side {
    fn tag(self) -> &'static [u8] {
        match self {
            Side::Opener => b"opener",
            Side::Answerer => b"answerer",
        }
    }

    fn other(self) -> Side {
        match self {
            Side::Opener => Side::Answerer,
            Side::Answerer => Side::Opener,
        }
    }
}

/// A peer that proved its key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proven {
    pub name: String,
    pub key: PublicKey,
    /// Who introduced it, when the log doesn't list it yet (capped,
    /// decision 46).
    pub introduced_by: Option<String>,
    /// Our stored log and introductions with what the peer carried, when
    /// that changed them: for the daemon to adopt.
    pub newer: Option<Stored>,
}

/// One end's `hello` message.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Hello {
    name: String,
    nonce: [u8; 32],
    head: Head,
}

/// `field` length-prefixed (u32 big-endian) onto `out`.
fn put(out: &mut Vec<u8>, field: &[u8]) {
    let len = u32::try_from(field.len()).expect("a link proof field under 4 GiB");
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(field);
}

fn put_head(out: &mut Vec<u8>, head: &Head) {
    let mut h = Vec::with_capacity(72);
    h.extend_from_slice(&head.len.to_be_bytes());
    h.extend_from_slice(&head.hash);
    h.extend_from_slice(&head.genesis);
    put(out, &h);
}

/// Reads [`put`]'s fields back, refusing anything short or left over.
struct Fields<'a>(&'a [u8]);

impl<'a> Fields<'a> {
    fn next(&mut self) -> Result<&'a [u8]> {
        let (len, rest) = self.0.split_at_checked(4).context("a truncated field")?;
        let len = u32::from_be_bytes(len.try_into().expect("4 bytes")) as usize;
        let (field, rest) = rest.split_at_checked(len).context("a truncated field")?;
        self.0 = rest;
        Ok(field)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        self.next()?.try_into().map_err(|_| anyhow!("a field of the wrong size"))
    }

    fn end(&self) -> Result<()> {
        if !self.0.is_empty() {
            bail!("trailing bytes");
        }
        Ok(())
    }
}

impl Hello {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put(&mut out, self.name.as_bytes());
        put(&mut out, &self.nonce);
        put_head(&mut out, &self.head);
        out
    }

    fn decode(body: &[u8]) -> Result<Hello> {
        let mut f = Fields(body);
        let name = String::from_utf8(f.next()?.to_vec()).context("a name that isn't UTF-8")?;
        let nonce = f.array()?;
        let head: [u8; 72] = f.array()?;
        f.end()?;
        let head = Head {
            len: u64::from_be_bytes(head[..8].try_into().expect("8 bytes")),
            hash: head[8..40].try_into().expect("32 bytes"),
            genesis: head[40..].try_into().expect("32 bytes"),
        };
        Ok(Hello { name, nonce, head })
    }
}

/// sha256 of a `Hello` frame's hash string and caps.
fn hello_digest(hello: &str, caps: u32) -> [u8; 32] {
    let mut out = Vec::new();
    put(&mut out, hello.as_bytes());
    put(&mut out, &caps.to_be_bytes());
    Sha256::digest(out).into()
}

/// What both ends sign: the opener's then the answerer's name, nonce,
/// `Hello` digest (its hash string and caps) and head, each
/// length-prefixed.
fn transcript(opener: (&Hello, [u8; 32]), answerer: (&Hello, [u8; 32])) -> Vec<u8> {
    let mut out = Vec::new();
    for (h, digest) in [opener, answerer] {
        put(&mut out, h.name.as_bytes());
        put(&mut out, &h.nonce);
        put(&mut out, &digest);
        put_head(&mut out, &h.head);
    }
    out
}

/// What `side` signs: the transcript and its own role, so neither end's
/// signature serves as the other's.
fn signed(transcript: &[u8], side: Side) -> Vec<u8> {
    let mut out = transcript.to_vec();
    put(&mut out, side.tag());
    out
}

fn write_raw(w: &mut impl Write, tag: u8, body: &[u8]) -> Result<()> {
    let len = u32::try_from(body.len() + 1).context("a link proof message too long")?;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(&[tag])?;
    w.write_all(body)?;
    w.flush()?;
    Ok(())
}

/// One message, bounded by `limit`: its body, or the peer's refusal as an
/// error.
fn read_raw(r: &mut impl Read, limit: usize, peer: &str) -> Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).with_context(|| format!("`{peer}` closed the link during the key proof"))?;
    let len = u32::from_be_bytes(len) as usize;
    if len == 0 || len > limit + 1 {
        bail!("`{peer}` sent a key proof message of {len} bytes (at most {})", limit + 1);
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).with_context(|| format!("`{peer}` closed the link during the key proof"))?;
    match buf[0] {
        MESSAGE => Ok(buf.split_off(1)),
        REFUSAL => bail!("`{peer}` refused the link: {}", String::from_utf8_lossy(&buf[1..])),
        tag => bail!("`{peer}` sent a key proof message tagged {tag}"),
    }
}

/// The ordered exchange of one round's messages (module doc).
struct Chan<'a, R, W> {
    side: Side,
    r: &'a mut R,
    w: &'a mut W,
    peer: &'a str,
}

impl<R: Read, W: Write> Chan<'_, R, W> {
    /// Write ours (or our refusal, which ends the proof with `Err(why)`).
    fn send(&mut self, ours: Result<Vec<u8>, String>) -> Result<()> {
        match ours {
            Ok(body) => write_raw(self.w, MESSAGE, &body),
            Err(why) => {
                let _ = write_raw(self.w, REFUSAL, why.as_bytes());
                Err(anyhow!("{why}"))
            }
        }
    }

    /// One round: the opener writes then reads, the answerer reads then
    /// writes.
    fn exchange(&mut self, ours: Result<Vec<u8>, String>, limit: usize) -> Result<Vec<u8>> {
        match self.side {
            Side::Opener => {
                self.send(ours)?;
                read_raw(self.r, limit, self.peer)
            }
            Side::Answerer => {
                let theirs = read_raw(self.r, limit, self.peer)?;
                self.send(ours)?;
                Ok(theirs)
            }
        }
    }
}

/// What `stored` sends a peer at `theirs` ([`Compare`] says how ours stands
/// to it): the entries it lacks when ours is ahead, and our introductions.
fn sync_for(stored: &Stored, cmp: Compare, theirs: Head) -> Wire {
    let entries = match cmp {
        Compare::Ahead => stored.log.entries_after(theirs.len).to_vec(),
        _ => Vec::new(),
    };
    Wire { entries, introductions: stored.introductions.clone() }
}

/// `stored` with what a peer at `theirs` sent (`wire`): its entries
/// continuing ours (an error when they don't, or don't reach its head
/// while ours is behind), then its introductions the result doesn't settle,
/// each held if it checks (else `log`ged and dropped).
fn merge(stored: &Stored, cmp: Compare, theirs: Head, wire: Wire, peer: &str, log: fn(&str)) -> Result<Stored> {
    let mut merged = stored.clone();
    if !wire.entries.is_empty() {
        merged.log.extend(wire.entries).with_context(|| format!("`{peer}`'s membership entries"))?;
    }
    if cmp == Compare::Behind && merged.log.head() != theirs {
        bail!("`{peer}`'s membership entries don't reach its head ({theirs})");
    }
    let m = merged.log.membership().context("no membership log")?.clone();
    m.drop_settled(&mut merged.introductions);
    for i in wire.introductions {
        if m.settles(&i) {
            continue;
        }
        if let Err(e) = merged.introduce(i.clone()) {
            log(&format!("fleet link with `{peer}`: its introduction of `{}` by `{}` is dropped: {e:#}", i.name, i.introducer));
        }
    }
    Ok(merged)
}

/// `name`'s key in `stored`: a member's in the log, else a held
/// introduction's (with its introducer).
fn resolve(stored: &Stored, name: &str) -> Option<(PublicKey, Option<String>)> {
    let m = stored.log.membership()?;
    if let Some(key) = m.key_of(name) {
        return Some((key, None));
    }
    stored.introductions.iter().find(|i| i.name == name).map(|i| (i.key, Some(i.introducer.clone())))
}

fn encode_proof(key: &PublicKey, sig: &Signature) -> Vec<u8> {
    let mut out = Vec::new();
    put(&mut out, key.as_bytes());
    put(&mut out, &sig.to_bytes());
    out
}

fn decode_proof(body: &[u8]) -> Result<(PublicKey, Signature)> {
    let mut f = Fields(body);
    let key = PublicKey::from_bytes(f.array()?)?;
    let sig = Signature::from_bytes(f.array()?);
    f.end()?;
    Ok((key, sig))
}

/// The peer's proof checked: its key is the one its name resolved to (a
/// revoked one, or another, refused) and it signed `message` with it.
fn check_proof(body: &[u8], expected: &PublicKey, stored: &Stored, peer: &str, message: &[u8]) -> Result<PublicKey, String> {
    let (key, sig) = decode_proof(body).map_err(|e| format!("`{peer}`'s key proof is malformed: {e:#}"))?;
    if key != *expected {
        let revoked = stored.log.membership().is_some_and(|m| m.revoked.contains(&key));
        return Err(if revoked {
            format!("`{peer}` proved key {}, which is revoked", key.fingerprint())
        } else {
            format!("`{peer}` proved key {}, not its key in this fleet's membership log ({})", key.fingerprint(), expected.fingerprint())
        });
    }
    key.verify(DOMAIN, message, &sig).map_err(|_| format!("`{peer}`'s key proof doesn't verify: a bad signature"))?;
    Ok(key)
}

/// Both keys proven over `r`/`w` from `side` (module doc): `ours` this
/// end's `Hello` (its hash string and caps), `theirs` the peer's. An error
/// says why the link is refused (by this end or the peer).
pub fn prove(side: Side, r: &mut impl Read, w: &mut impl Write, auth: &Auth, ours: (&str, u32), theirs: (&str, u32)) -> Result<Proven> {
    let id = &auth.identity;
    let peer = auth.expect.as_str();
    let mut chan = Chan { side, r, w, peer };
    let mut nonce = [0u8; 32];
    getrandom::fill(&mut nonce).map_err(|e| anyhow!("cannot read the OS random source: {e}"))?;
    let mine = Hello { name: id.name.clone(), nonce, head: id.stored.log.head() };

    // 1. hello.
    let body = chan.exchange(Ok(mine.encode()), SMALL)?;
    let them = Hello::decode(&body).map_err(|e| anyhow!("`{peer}`'s key proof hello is malformed: {e:#}"))?;
    let cmp = membership::compare(&id.stored.log, them.head);
    let checked = if them.name != peer {
        Err(format!("the peer calls itself `{}`, not `{peer}`", them.name))
    } else if let Compare::Fork { another_fleet } = cmp {
        let what = if another_fleet { "another fleet's (another genesis)" } else { "a fork: one authority signed two histories" };
        let why = format!("`{peer}`'s membership log is {what} (ours: {}; theirs: {})", id.stored.log.head(), them.head);
        (auth.log)(&format!("fleet link with `{peer}` refused: {why}"));
        Err(why)
    } else {
        serde_json::to_vec(&sync_for(&id.stored, cmp, them.head)).map_err(|e| format!("cannot encode the membership sync: {e}"))
    };

    // 2. sync.
    let body = chan.exchange(checked, SYNC_LIMIT)?;
    let resolved = serde_json::from_slice::<Wire>(&body)
        .map_err(|e| format!("`{peer}`'s membership sync is malformed: {e}"))
        .and_then(|wire| merge(&id.stored, cmp, them.head, wire, peer, auth.log).map_err(|e| format!("{e:#}")))
        .and_then(|merged| match resolve(&merged, peer) {
            Some(found) => Ok((merged, found)),
            None => Err(format!("`{peer}` is neither a member of this fleet's membership log nor introduced to it")),
        });

    // 3. proof.
    let (o, a) = match side {
        Side::Opener => ((&mine, hello_digest(ours.0, ours.1)), (&them, hello_digest(theirs.0, theirs.1))),
        Side::Answerer => ((&them, hello_digest(theirs.0, theirs.1)), (&mine, hello_digest(ours.0, ours.1))),
    };
    let t = transcript(o, a);
    let proof = encode_proof(&id.key.public_key(), &id.key.sign(DOMAIN, &signed(&t, side)));
    let (merged, (expected, introduced_by)) = match side {
        Side::Opener => {
            let resolved = resolved;
            let theirs_proof = chan.exchange(resolved.as_ref().map(|_| proof).map_err(Clone::clone), SMALL)?;
            let (merged, found) = resolved.expect("refused above otherwise");
            // 4. confirm: our verdict on theirs.
            let verdict = check_proof(&theirs_proof, &found.0, &merged, peer, &signed(&t, side.other()));
            chan.send(verdict.map(|_| Vec::new()))?;
            (merged, found)
        }
        Side::Answerer => {
            let theirs_proof = read_raw(chan.r, SMALL, peer)?;
            let verdict = resolved.and_then(|(merged, found)| {
                check_proof(&theirs_proof, &found.0, &merged, peer, &signed(&t, side.other())).map(|_| (merged, found))
            });
            chan.send(verdict.as_ref().map(|_| proof).map_err(Clone::clone))?;
            read_raw(chan.r, SMALL, peer)?;
            verdict.expect("refused above otherwise")
        }
    };
    let newer = (merged != id.stored).then_some(merged);
    Ok(Proven { name: peer.to_string(), key: expected, introduced_by, newer })
}

/// Why a link ends after `newer` was adopted: the hub its log names isn't
/// the one this end's `Hello` (`hello`) said, so both ends redial saying
/// it. `None` when it is.
pub fn hub_moved(hello: &str, newer: &Stored) -> Option<String> {
    let hub = &newer.log.membership()?.hub;
    (update::hub_of(hello) != Some(hub.as_str()))
        .then(|| format!("took newer membership entries: the fleet's hub is now `{hub}`; the link ends so both ends redial saying it"))
}

#[cfg(test)]
pub mod tests {
    use std::os::unix::net::UnixStream;

    use super::*;
    use crate::serve::fleet::membership::{Introduction, Log, Op};

    fn key(n: u8) -> SigningKey {
        SigningKey::from_seed([n; 32])
    }

    fn pk(n: u8) -> PublicKey {
        key(n).public_key()
    }

    /// `lap` (key 1) the authority, `box` (key 2) the hub; fleet id `id`.
    fn two(id: u8) -> Stored {
        let mut log = Log::default();
        log.append(Op::genesis_with_id([id; 16], "lap", pk(1), "box"), "lap", &key(1)).unwrap();
        log.append(Op::Add { name: "box".into(), key: pk(2) }, "lap", &key(1)).unwrap();
        Stored { log, introductions: vec![] }
    }

    fn with(mut s: Stored, op: Op) -> Stored {
        s.log.append(op, "lap", &key(1)).unwrap();
        s
    }

    fn auth(name: &str, k: u8, stored: Stored, expect: &str) -> Auth {
        Auth { identity: Identity { name: name.into(), key: key(k), stored }, expect: expect.into(), log: |_| {} }
    }

    const HELLO_O: (&str, u32) = ("o hello", 1);
    const HELLO_A: (&str, u32) = ("a hello", 2);

    /// The opener (`o`) and the answerer (`a`) proving over a socket pair.
    pub fn run(o: Auth, a: Auth) -> (Result<Proven>, Result<Proven>) {
        let (os, as_) = UnixStream::pair().unwrap();
        let opener = std::thread::spawn(move || {
            let (mut r, mut w) = (os.try_clone().unwrap(), os);
            let out = prove(Side::Opener, &mut r, &mut w, &o, HELLO_O, HELLO_A);
            let _ = w.shutdown(std::net::Shutdown::Both);
            out
        });
        let (mut r, mut w) = (as_.try_clone().unwrap(), as_);
        let answered = prove(Side::Answerer, &mut r, &mut w, &a, HELLO_A, HELLO_O);
        let _ = w.shutdown(std::net::Shutdown::Both);
        (opener.join().unwrap(), answered)
    }

    fn err(r: Result<Proven>) -> String {
        format!("{:#}", r.expect_err("refused"))
    }

    #[test]
    fn both_ends_prove_their_keys() {
        let (o, a) = run(auth("box", 2, two(1), "lap"), auth("lap", 1, two(1), "box"));
        assert_eq!(o.unwrap(), Proven { name: "lap".into(), key: pk(1), introduced_by: None, newer: None });
        assert_eq!(a.unwrap(), Proven { name: "box".into(), key: pk(2), introduced_by: None, newer: None });
    }

    #[test]
    fn a_wrong_key_for_the_name_is_refused() {
        // `box` signs with key 9: the log says key 2.
        let (o, a) = run(auth("box", 9, two(1), "lap"), auth("lap", 1, two(1), "box"));
        let a = err(a);
        assert!(a.contains("`box` proved key") && a.contains("not its key"), "{a}");
        assert!(err(o).contains("`lap` refused the link: `box` proved key"));
        // The answerer's wrong key: the opener's confirm refuses it.
        let (o, a) = run(auth("box", 2, two(1), "lap"), auth("lap", 9, two(1), "box"));
        assert!(err(o).contains("`lap` proved key"));
        assert!(err(a).contains("`box` refused the link: `lap` proved key"));
    }

    #[test]
    fn an_unknown_or_unexpected_name_is_refused() {
        let (o, a) = run(auth("zed", 9, two(1), "lap"), auth("lap", 1, two(1), "zed"));
        assert!(err(a).contains("`zed` is neither a member"), "unknown");
        assert!(err(o).contains("refused the link"));
        let (o, a) = run(auth("box", 2, two(1), "lap"), auth("lap", 1, two(1), "pi"));
        assert!(err(a).contains("the peer calls itself `box`, not `pi`"));
        assert!(err(o).contains("`lap` refused the link"));
    }

    #[test]
    fn a_revoked_key_is_refused() {
        // `pi` (key 3) revoked, then added back with key 4; the old key proves.
        let s = with(with(with(two(1), Op::Add { name: "pi".into(), key: pk(3) }), Op::Revoke { name: "pi".into(), key: pk(3) }), Op::Add {
            name: "pi".into(),
            key: pk(4),
        });
        let (_, a) = run(auth("pi", 3, s.clone(), "lap"), auth("lap", 1, s.clone(), "pi"));
        assert!(err(a).contains("which is revoked"));
        // Revoked and gone: an unknown name.
        let gone = with(with(two(1), Op::Add { name: "pi".into(), key: pk(3) }), Op::Revoke { name: "pi".into(), key: pk(3) });
        let (_, a) = run(auth("pi", 3, gone.clone(), "lap"), auth("lap", 1, gone, "pi"));
        assert!(err(a).contains("neither a member"));
    }

    #[test]
    fn an_introduced_peer_is_proven_capped() {
        let s = two(1);
        let g = s.log.membership().unwrap().genesis;
        let mut new = s.clone();
        new.introduce(Introduction::sign(&g, "new", pk(5), "box", &key(2))).unwrap();
        // `lap` doesn't hold the introduction: `new` carries it.
        let (o, a) = run(auth("new", 5, new.clone(), "lap"), auth("lap", 1, s.clone(), "new"));
        let a = a.unwrap();
        assert_eq!((a.key, a.introduced_by.as_deref()), (pk(5), Some("box")));
        assert_eq!(a.newer, Some(new.clone()), "the introduction to hold");
        assert_eq!(o.unwrap().newer, None);
        // An introduction by a capped member doesn't check: dropped, the name unknown.
        let mut by_new = new.clone();
        by_new.introductions.push(Introduction::sign(&g, "x", pk(6), "new", &key(5)));
        let (_, a) = run(auth("x", 6, by_new, "lap"), auth("lap", 1, s, "x"));
        assert!(err(a).contains("`x` is neither a member"));
    }

    #[test]
    fn the_end_behind_gets_the_entries_it_lacks() {
        let s = two(1);
        let ahead = with(with(s.clone(), Op::Add { name: "pi".into(), key: pk(3) }), Op::Hub { name: "pi".into() });
        let (o, a) = run(auth("box", 2, s.clone(), "lap"), auth("lap", 1, ahead.clone(), "box"));
        assert_eq!(o.unwrap().newer, Some(ahead.clone()));
        assert_eq!(a.unwrap().newer, None);
        // The peer exists only in the newer entries: resolved after them.
        let (o, a) = run(auth("pi", 3, ahead.clone(), "lap"), auth("lap", 1, s.clone(), "pi"));
        assert_eq!(a.unwrap().newer, Some(ahead.clone()));
        assert_eq!(o.unwrap().newer, None);
        assert!(hub_moved(&update::hello_of(false, Some("box")), &ahead).unwrap().contains("hub is now `pi`"));
        assert_eq!(hub_moved(&update::hello_of(false, Some("pi")), &ahead), None);
    }

    #[test]
    fn a_fork_or_another_fleet_is_refused() {
        let s = two(1);
        let left = with(s.clone(), Op::Add { name: "pi".into(), key: pk(3) });
        let right = with(s.clone(), Op::Add { name: "zz".into(), key: pk(4) });
        let (o, a) = run(auth("box", 2, left, "lap"), auth("lap", 1, right, "box"));
        assert!(err(o).contains("a fork: one authority signed two histories"));
        assert!(err(a).contains("a fork"));
        let (o, a) = run(auth("box", 2, two(2), "lap"), auth("lap", 1, two(1), "box"));
        assert!(err(o).contains("another fleet's (another genesis)"));
        assert!(err(a).contains("another fleet's"));
    }

    /// An opener played by hand: the real `hello` and `sync`, then `proof`
    /// as `forge` makes it from the transcript the answerer signs.
    fn forged(forge: impl FnOnce(&[u8]) -> Vec<u8> + Send + 'static) -> String {
        let (os, as_) = UnixStream::pair().unwrap();
        let s = two(1);
        let opener = std::thread::spawn(move || {
            let (mut r, mut w) = (os.try_clone().unwrap(), os);
            let mine = Hello { name: "box".into(), nonce: [7; 32], head: two(1).log.head() };
            write_raw(&mut w, MESSAGE, &mine.encode()).unwrap();
            let them = Hello::decode(&read_raw(&mut r, SMALL, "lap").unwrap()).unwrap();
            write_raw(&mut w, MESSAGE, br#"{"entries":[]}"#).unwrap();
            read_raw(&mut r, SYNC_LIMIT, "lap").unwrap();
            let t = transcript((&mine, hello_digest(HELLO_O.0, HELLO_O.1)), (&them, hello_digest(HELLO_A.0, HELLO_A.1)));
            write_raw(&mut w, MESSAGE, &forge(&t)).unwrap();
            let _ = read_raw(&mut r, SMALL, "lap");
        });
        let (mut r, mut w) = (as_.try_clone().unwrap(), as_);
        let got = prove(Side::Answerer, &mut r, &mut w, &auth("lap", 1, s, "box"), HELLO_A, HELLO_O);
        drop((r, w));
        opener.join().unwrap();
        err(got)
    }

    #[test]
    fn a_tampered_proof_is_refused() {
        // The signature's bytes changed.
        let e = forged(|t| {
            let mut sig = key(2).sign(DOMAIN, &signed(t, Side::Opener)).to_bytes();
            sig[3] ^= 1;
            encode_proof(&pk(2), &Signature::from_bytes(sig))
        });
        assert!(e.contains("doesn't verify"), "{e}");
        // Another transcript (another nonce), the answerer's role, another domain.
        let e = forged(|t| {
            let mut t = t.to_vec();
            t[10] ^= 1;
            encode_proof(&pk(2), &key(2).sign(DOMAIN, &signed(&t, Side::Opener)))
        });
        assert!(e.contains("doesn't verify"), "{e}");
        let e = forged(|t| encode_proof(&pk(2), &key(2).sign(DOMAIN, &signed(t, Side::Answerer))));
        assert!(e.contains("doesn't verify"), "reflected: {e}");
        let e = forged(|t| encode_proof(&pk(2), &key(2).sign(membership::ROTATE_DOMAIN, &signed(t, Side::Opener))));
        assert!(e.contains("doesn't verify"), "{e}");
        // Malformed, oversize.
        assert!(forged(|_| b"junk".to_vec()).contains("malformed"));
        let e = forged(|_| vec![0; SMALL + 1]);
        assert!(e.contains("key proof message of"), "{e}");
    }

    #[test]
    fn identity_needs_a_key_and_a_log_listing_this_host() {
        use crate::serve::fleet::membership_api::test_support;
        let dir = std::env::temp_dir().join(format!("dsv-proof-identity-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let toml = dir.join("member.toml");
        member::write_for_test(&toml, &member::Member { name: "box".into(), ..Default::default() });
        let e = format!("{:#}", Identity::load(&toml, "host").unwrap_err());
        assert!(e.contains("no member key") && e.contains("machine add"), "{e}");
        let k = test_support::key_of(&toml);
        let e = format!("{:#}", Identity::load(&toml, "host").unwrap_err());
        assert!(e.contains("in no fleet's membership log") && e.contains("machine add"), "{e}");
        test_support::write(&toml, &test_support::fleet("lap", &key(1), "lap", &[("box", pk(9))]));
        let e = format!("{:#}", Identity::load(&toml, "host").unwrap_err());
        assert!(e.contains("doesn't list it"), "{e}");
        test_support::write(&toml, &test_support::fleet("lap", &key(1), "lap", &[("box", k.public_key())]));
        let id = Identity::load(&toml, "host").unwrap();
        assert_eq!((id.name.as_str(), id.key.public_key()), ("box", k.public_key()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
