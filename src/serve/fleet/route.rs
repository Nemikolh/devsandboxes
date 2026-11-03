//! A routed stream's opening (docs/architecture/daemon/fleet.md, *Routed
//! streams*; fleet plan step 10b, decisions 53-58): the header a `ROUTE`
//! stream starts with and the challenge-response both ways that proves its
//! origin to its destination, and the destination back, over any byte
//! pipe, on [`proof`]'s framing (a `u32` length, a tag byte: a message or a
//! refusal, the body).
//!
//! - **The header** ([`Header`], [`write_header`], [`read_header`]): its
//!   destination, its origin, the inner channel (`API`, `PTY`, `TUNNEL` or
//!   `TRANSFER`: `AGENT` stays direct, decision 58, and a route carries no
//!   route), the path (the hops after the origin, ending in the
//!   destination) and a TTL, at most [`HEADER_LIMIT`] bytes. The origin
//!   sets the TTL to [`MAX_HOPS`]; each relay checks it ([`relay_header`])
//!   and forwards it one lower, then splices the rest unread.
//! - **The proof**: the destination sends a challenge (a fresh nonce); the
//!   origin answers with its own nonce, its key and its signature, in
//!   [`DOMAIN`], over the [`transcript`] (the fleet's genesis hash, origin,
//!   destination, channel, path, both nonces) and its role; the
//!   destination checks the origin's name resolves to that key
//!   ([`proof::resolve`]: a log member or a held introduction, revoked keys
//!   refused) and proves itself back the same way in its role; the origin
//!   checks that key is the destination's and answers its verdict.
//!
//! A relay or the destination refusing writes a refusal naming itself
//! ([`refuse`]) back toward the origin, which reads it as the stream's
//! refusal.
//!
//! A link end serves a `ROUTE` stream with [`serve`]: it relays it toward
//! its next hop, or answers it as its destination; [`open`] is the
//! origin's side over a link.

// `open` has no caller but tests yet: the origin's path choice is fleet
// plan step 10b.5.
#![cfg_attr(not(test), allow(dead_code))]

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

use crate::devsbd::proto::channel;
use crate::serve::endpoint;

use super::graph::MAX_HOPS;
use super::identity::{PublicKey, Signer, SigningKey};
use super::lent::Channel;
use super::member::check_member_name;
use super::membership::Stored;
use super::proof::{self, Bad, Chan, Fields, Frame, MESSAGE, REFUSAL, Side, put, read_frame, write_raw};

/// The domain a routed stream's proofs are signed in (apart from the link
/// proof's, `proof::DOMAIN`).
pub const DOMAIN: &str = "devsandbox/route/v1";

/// The bound of a header's body.
pub const HEADER_LIMIT: usize = 4096;
/// The bound of a challenge, a proof or a verdict.
const SMALL: usize = 4096;

/// How long each read of a routed stream's opening may wait, at every hop
/// (a header, a challenge, a proof, a verdict): a stalled origin or hop
/// doesn't hold a thread.
pub const OPENING: Duration = Duration::from_secs(10);

/// A routed stream's header (module doc).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub dest: String,
    pub origin: String,
    /// The channel served at the destination.
    pub channel: Channel,
    /// The hops after the origin, ending in `dest`.
    pub path: Vec<String>,
    pub ttl: u8,
}

/// What a relay does with a header ([`relay_header`]): hand it, one TTL
/// lower, to `next`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hop {
    pub next: String,
    pub header: Header,
}

/// A routed stream's origin, proven at its destination: enough to serve
/// `channel` for it as for a direct peer (decision 57).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Routed {
    pub origin: String,
    pub key: PublicKey,
    /// Its introducer, when it's held as an introduction (capped).
    pub introduced_by: Option<String>,
    pub channel: Channel,
}

fn channel_byte(c: Channel) -> u8 {
    match c {
        Channel::Api => channel::API,
        Channel::Pty => channel::PTY,
        Channel::Tunnel => channel::TUNNEL,
        Channel::Transfer => channel::TRANSFER,
        Channel::Agent => channel::AGENT,
        Channel::Route => channel::ROUTE,
    }
}

fn channel_of(b: u8) -> Option<Channel> {
    Some(match b {
        channel::API => Channel::Api,
        channel::PTY => Channel::Pty,
        channel::TUNNEL => Channel::Tunnel,
        channel::TRANSFER => Channel::Transfer,
        channel::AGENT => Channel::Agent,
        channel::ROUTE => Channel::Route,
        _ => return None,
    })
}

impl Header {
    /// The header `origin` opens a stream with over `path` (the hops after
    /// it, ending in the destination): the TTL [`MAX_HOPS`].
    pub fn new(origin: &str, channel: Channel, path: Vec<String>) -> Header {
        let dest = path.last().cloned().unwrap_or_default();
        Header { dest, origin: origin.to_string(), channel, path, ttl: MAX_HOPS as u8 }
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put(&mut out, self.dest.as_bytes());
        put(&mut out, self.origin.as_bytes());
        put(&mut out, &[channel_byte(self.channel)]);
        put(&mut out, &[self.ttl]);
        for hop in &self.path {
            put(&mut out, hop.as_bytes());
        }
        out
    }

    fn decode(body: &[u8]) -> Result<Header> {
        let text = |b: &[u8]| String::from_utf8(b.to_vec()).context("a name that isn't UTF-8");
        let mut f = Fields(body);
        let dest = text(f.next()?)?;
        let origin = text(f.next()?)?;
        let [c] = f.array()?;
        let channel = channel_of(c).with_context(|| format!("no channel {c}"))?;
        let [ttl] = f.array()?;
        let mut path = Vec::new();
        while !f.done() {
            path.push(text(f.next()?)?);
        }
        Ok(Header { dest, origin, channel, path, ttl })
    }

    /// What every hop checks: a routed channel, a TTL left, a path of at
    /// most [`MAX_HOPS`] ending in the destination, valid names, no loop.
    pub fn check(&self) -> Result<()> {
        match self.channel {
            Channel::Agent => bail!("`agent` isn't routed: an ssh agent reaches only a direct link's peer"),
            Channel::Route => bail!("a routed stream can't carry another"),
            _ => {}
        }
        if self.ttl == 0 {
            bail!("its TTL ran out");
        }
        if usize::from(self.ttl) > MAX_HOPS {
            bail!("its TTL is {} (at most {MAX_HOPS})", self.ttl);
        }
        if self.path.is_empty() {
            bail!("its path is empty");
        }
        if self.path.len() > MAX_HOPS {
            bail!("its path has {} hops (at most {MAX_HOPS})", self.path.len());
        }
        if self.path.last() != Some(&self.dest) {
            bail!("its path doesn't end in its destination `{}`", self.dest);
        }
        for name in std::iter::once(&self.origin).chain(&self.path) {
            check_member_name(name)?;
        }
        for (i, hop) in self.path.iter().enumerate() {
            if *hop == self.origin || self.path[..i].contains(hop) {
                bail!("its path loops through `{hop}`");
            }
        }
        Ok(())
    }

    fn what(&self) -> String {
        format!("the routed stream from `{}` to `{}`", self.origin, self.dest)
    }
}

/// `header` written as a stream's first message, checked first.
pub fn write_header(w: &mut impl Write, header: &Header) -> Result<()> {
    header.check().with_context(|| header.what())?;
    let body = header.encode();
    if body.len() > HEADER_LIMIT {
        bail!("{}: its header is {} bytes (at most {HEADER_LIMIT})", header.what(), body.len());
    }
    write_raw(w, MESSAGE, &body)
}

/// One message of a routed opening: its body, or a refusal as an error
/// (each refuser names itself in it, [`refuse`]). `peer` the member at the
/// other end of the route.
fn read_msg<R: Read>(r: &mut R, limit: usize, peer: &str) -> Result<Vec<u8>> {
    match read_frame(r, limit) {
        Ok(Frame::Message(body)) => Ok(body),
        Ok(Frame::Refusal(why)) => bail!("the routed stream with `{peer}` is refused: {why}"),
        Err(Bad::Closed(e)) => Err(anyhow::Error::new(e).context(format!("the routed stream with `{peer}` closed during its opening"))),
        Err(Bad::Size(len)) => {
            bail!("the routed stream with `{peer}` carried an opening message of {len} bytes (at most {})", limit + 1)
        }
        Err(Bad::Tag(tag)) => bail!("the routed stream with `{peer}` carried an opening message tagged {tag}"),
    }
}

/// A stream's header, read from the link peer `from` (the previous hop)
/// and checked ([`Header::check`]).
pub fn read_header(r: &mut impl Read, from: &str) -> Result<Header> {
    let body = match read_frame(r, HEADER_LIMIT) {
        Ok(Frame::Message(body)) => body,
        Ok(Frame::Refusal(why)) => bail!("`{from}` sent a refusal for a routed stream's header: {why}"),
        Err(Bad::Closed(e)) => return Err(anyhow::Error::new(e).context(format!("`{from}` closed a routed stream before its header"))),
        Err(Bad::Size(len)) => bail!("`{from}` sent a routed stream header of {len} bytes (at most {})", HEADER_LIMIT + 1),
        Err(Bad::Tag(tag)) => bail!("`{from}` sent a routed stream header tagged {tag}"),
    };
    let header = Header::decode(&body).with_context(|| format!("`{from}` sent a malformed routed stream header"))?;
    header.check().with_context(|| header.what())?;
    Ok(header)
}

/// What relay `me` forwards `header` as (decisions 55, 56): `me` is on its
/// path, not its end; the TTL covers the hops left; the next hop and the
/// destination are members `stored` knows (log or held introduction).
/// Whether the next hop is a direct link that's up is the caller's.
pub fn relay_header(header: &Header, me: &str, stored: &Stored) -> Result<Hop> {
    let refused = |why: String| anyhow!("{}: {why}", header.what());
    header.check().with_context(|| header.what())?;
    if header.origin == me {
        return Err(refused("it came back to its origin".into()));
    }
    let at = header.path.iter().position(|h| h == me).ok_or_else(|| refused(format!("`{me}` isn't on its path")))?;
    let Some(next) = header.path.get(at + 1) else {
        return Err(refused(format!("`{me}` is its destination, not a relay")));
    };
    let left = header.path.len() - at;
    if usize::from(header.ttl) < left {
        return Err(refused(format!("its TTL {} doesn't cover the {left} hops left", header.ttl)));
    }
    for name in [next, &header.dest] {
        if proof::resolve(stored, name).is_none() {
            return Err(refused(format!("`{name}` is neither a member of this fleet's membership log nor introduced to it")));
        }
    }
    Ok(Hop { next: next.clone(), header: Header { ttl: header.ttl - 1, ..header.clone() } })
}

/// That `header` came to `me` from link peer `from`, the hop before `me` on
/// its path (its origin when `me` is the first): a peer can't hand on a
/// stream as if it came through another, nor have it sent back to itself
/// (its next hop would then be the hop it came from, a loop). `me` not on
/// the path is [`relay_header`]'s refusal.
pub fn came_from(header: &Header, me: &str, from: &str) -> Result<()> {
    let Some(at) = header.path.iter().position(|h| h == me) else { return Ok(()) };
    let before = if at == 0 { &header.origin } else { &header.path[at - 1] };
    if before != from {
        bail!("{}: it came from `{from}`, not from `{before}`, the hop before `{me}`", header.what());
    }
    Ok(())
}

/// `why` written back toward the origin as `me`'s refusal (best effort:
/// the stream may be gone).
pub fn refuse(w: &mut impl Write, me: &str, why: &str) {
    let _ = write_raw(w, REFUSAL, format!("`{me}`: {why}").as_bytes());
}

/// What both ends sign: the fleet's genesis hash, origin, destination,
/// channel, path (its hops as one field) and both nonces (the origin's,
/// then the destination's), each length-prefixed. The TTL isn't in it: each
/// relay lowers it.
fn transcript(genesis: &[u8; 32], h: &Header, nonce_o: &[u8; 32], nonce_d: &[u8; 32]) -> Vec<u8> {
    let mut path = Vec::new();
    for hop in &h.path {
        put(&mut path, hop.as_bytes());
    }
    let mut out = Vec::new();
    put(&mut out, genesis);
    put(&mut out, h.origin.as_bytes());
    put(&mut out, h.dest.as_bytes());
    put(&mut out, &[channel_byte(h.channel)]);
    put(&mut out, &path);
    put(&mut out, nonce_o);
    put(&mut out, nonce_d);
    out
}

const ORIGIN: &[u8] = b"origin";
const DESTINATION: &[u8] = b"destination";

/// What a role signs: the transcript and the role, so neither end's
/// signature serves as the other's.
fn signed(transcript: &[u8], role: &[u8]) -> Vec<u8> {
    let mut out = transcript.to_vec();
    put(&mut out, role);
    out
}

fn nonce() -> Result<[u8; 32]> {
    let mut n = [0u8; 32];
    getrandom::fill(&mut n).map_err(|e| anyhow!("cannot read the OS random source: {e}"))?;
    Ok(n)
}

fn genesis(stored: &Stored) -> Result<[u8; 32], String> {
    stored.log.membership().map(|m| m.genesis).ok_or_else(|| "this host has no membership log".to_string())
}

fn resolved(stored: &Stored, name: &str) -> Result<(PublicKey, Option<String>), String> {
    proof::resolve(stored, name).ok_or_else(|| format!("`{name}` is neither a member of this fleet's membership log nor introduced to it"))
}

/// The origin's side, after it wrote `header` ([`write_header`]) on a
/// stream reaching the destination: answers the challenge with `key`'s
/// proof, then checks the destination's against `stored`. `Ok` once both
/// ends are proven: the inner channel's bytes follow.
pub fn origin_side(r: &mut impl Read, w: &mut impl Write, header: &Header, key: &SigningKey, stored: &Stored) -> Result<()> {
    let me = header.origin.as_str();
    let dest = header.dest.as_str();
    let mut chan = Chan { side: Side::Opener, r, w, peer: dest, read: read_msg };
    let challenge = (chan.read)(chan.r, SMALL, dest)?;
    let mut f = Fields(&challenge);
    let nonce_d: [u8; 32] = f.array().and_then(|n| f.end().map(|()| n)).with_context(|| format!("`{dest}`'s challenge is malformed"))?;
    let nonce_o = nonce()?;
    let checked = genesis(stored).and_then(|g| resolved(stored, dest).map(|found| (g, found)));
    let ours = checked.as_ref().map_err(|why| format!("`{me}`: {why}")).map(|(g, _)| {
        let t = transcript(g, header, &nonce_o, &nonce_d);
        let mut out = Vec::new();
        put(&mut out, &nonce_o);
        out.extend(proof::encode_proof(&key.public_key(), &key.sign(DOMAIN, &signed(&t, ORIGIN))));
        out
    });
    let theirs = chan.exchange(ours, SMALL)?;
    let (g, (expected, _)) = checked.expect("refused above otherwise");
    let t = transcript(&g, header, &nonce_o, &nonce_d);
    let verdict = proof::check_proof(&theirs, &expected, stored, dest, DOMAIN, &signed(&t, DESTINATION));
    chan.send(verdict.map(|_| Vec::new()).map_err(|why| format!("`{me}`: {why}")))
}

/// The destination's side, after it read `header` ([`read_header`]):
/// challenges the origin, checks its proof against `stored`, proves itself
/// back with `key` and takes the origin's verdict. `me` is this member's
/// name. A refusal is written back to the origin and returned.
pub fn dest_side(r: &mut impl Read, w: &mut impl Write, header: &Header, me: &str, key: &SigningKey, stored: &Stored) -> Result<Routed> {
    let origin = header.origin.as_str();
    let mut chan = Chan { side: Side::Answerer, r, w, peer: origin, read: read_msg };
    let nonce_d = nonce()?;
    let checked = if header.dest != me {
        Err(format!("{} isn't for `{me}`", header.what()))
    } else {
        genesis(stored).and_then(|g| resolved(stored, origin).map(|found| (g, found)))
    };
    let mut challenge = Vec::new();
    put(&mut challenge, &nonce_d);
    chan.send(checked.as_ref().map(|_| challenge).map_err(|why| format!("`{me}`: {why}")))?;
    let (g, (expected, introduced_by)) = checked.expect("refused above otherwise");

    let body = (chan.read)(chan.r, SMALL, origin)?;
    let verdict = (|| -> Result<Vec<u8>, String> {
        let mut f = Fields(&body);
        let nonce_o: [u8; 32] = f.array().map_err(|e| format!("`{origin}`'s key proof is malformed: {e:#}"))?;
        let t = transcript(&g, header, &nonce_o, &nonce_d);
        proof::check_proof(f.0, &expected, stored, origin, DOMAIN, &signed(&t, ORIGIN))?;
        Ok(t)
    })();
    let ours = verdict.map(|t| proof::encode_proof(&key.public_key(), &key.sign(DOMAIN, &signed(&t, DESTINATION))));
    chan.send(ours.map_err(|why| format!("`{me}`: {why}")))?;
    (chan.read)(chan.r, SMALL, origin)?;
    Ok(Routed { origin: origin.to_string(), key: expected, introduced_by, channel: header.channel })
}

/// Who serves a `ROUTE` stream here ([`serve`]): this member's name, the
/// key it proves with as a destination, its stored log.
pub struct Here<'a> {
    pub me: &'a str,
    pub key: &'a SigningKey,
    pub stored: &'a Stored,
}

/// A `ROUTE` stream link peer `from` opened to this member (`here`): its
/// header read and checked within [`OPENING`] ([`read_header`], that it
/// came from `from`: [`came_from`]). For this member, the destination's
/// side ([`dest_side`]): the proven origin and the stream, its inner
/// channel's bytes next, to serve as the origin's (decision 57). Else a
/// relay (decision 56, whatever this member's trust): [`relay_header`],
/// then `reach` connects the next hop's link (`Err` why not: no direct
/// link that's up), the header goes on one TTL lower and the rest is
/// spliced unread until both ways end (`Ok(None)`). A refusal is written
/// back toward the origin and returned.
pub fn serve(
    stream: UnixStream,
    from: &str,
    here: &Here,
    reach: impl FnOnce(&str) -> Result<UnixStream, String>,
) -> Result<Option<(Routed, UnixStream)>> {
    let me = here.me;
    let (mut r, mut w) = (&stream, &stream);
    let refused = |w: &mut &UnixStream, e: anyhow::Error| {
        refuse(w, me, &format!("{e:#}"));
        Err(e)
    };
    stream.set_read_timeout(Some(OPENING)).context("cannot set a routed stream's read timeout")?;
    let header = match read_header(&mut r, from).and_then(|h| came_from(&h, me, from).map(|()| h)) {
        Ok(h) => h,
        Err(e) => return refused(&mut w, e),
    };
    if header.dest == me {
        let routed = dest_side(&mut r, &mut w, &header, me, here.key, here.stored)?;
        stream.set_read_timeout(None).context("cannot clear a routed stream's read timeout")?;
        return Ok(Some((routed, stream)));
    }
    let hop = relay_header(&header, me, here.stored)
        .and_then(|hop| reach(&hop.next).map(|down| (hop, down)).map_err(|why| anyhow!("{}: {why}", header.what())));
    let (hop, down) = match hop {
        Ok(found) => found,
        Err(e) => return refused(&mut w, e),
    };
    if let Err(e) = write_header(&mut &down, &hop.header) {
        return refused(&mut w, e.context(format!("cannot hand it on to `{}`", hop.next)));
    }
    stream.set_read_timeout(None).context("cannot clear a routed stream's read timeout")?;
    splice(stream, down);
    Ok(None)
}

/// Copy `a` and `b` both ways, each direction's end passed on as a
/// half-close; returns once both have ended (the caller's thread is the
/// relay's, counted while it runs).
pub fn splice(a: UnixStream, b: UnixStream) {
    let (Ok(a2), Ok(b2)) = (a.try_clone(), b.try_clone()) else { return };
    let back = std::thread::Builder::new().name("route-splice".into()).spawn(move || {
        let _ = io::copy(&mut &b2, &mut &a2);
        let _ = a2.shutdown(Shutdown::Write);
    });
    let _ = io::copy(&mut &a, &mut &b);
    let _ = b.shutdown(Shutdown::Write);
    if let Ok(t) = back {
        let _ = t.join();
    }
}

/// The origin's side over a link: a `ROUTE` stream through the link to
/// `header`'s first hop (`fleet-<hop>.sock` in socket dir `dir`), its
/// header written and its opening proven with `key` against `stored`
/// ([`origin_side`], each read within [`OPENING`]); the stream, ready for
/// the inner channel's bytes.
pub fn open(dir: &Path, header: &Header, key: &SigningKey, stored: &Stored) -> Result<UnixStream> {
    header.check().with_context(|| header.what())?;
    let first = &header.path[0];
    let socket = endpoint::fleet_socket(dir, first);
    let stream = super::link::connect(&socket, channel::ROUTE)
        .with_context(|| format!("{}: no link to `{first}` is up ({})", header.what(), socket.display()))?;
    stream.set_read_timeout(Some(OPENING)).context("cannot set a routed stream's read timeout")?;
    let (mut r, mut w) = (&stream, &stream);
    write_header(&mut w, header)?;
    origin_side(&mut r, &mut w, header, key, stored)?;
    stream.set_read_timeout(None).context("cannot clear a routed stream's read timeout")?;
    Ok(stream)
}

/// `header`'s body unchecked, for tests to send what [`write_header`]
/// wouldn't.
#[cfg(test)]
pub(crate) fn encode_unchecked(header: &Header) -> Vec<u8> {
    header.encode()
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;
    use crate::serve::fleet::membership::{Introduction, Log, Op};

    fn key(n: u8) -> SigningKey {
        SigningKey::from_seed([n; 32])
    }

    fn pk(n: u8) -> PublicKey {
        key(n).public_key()
    }

    /// `lap` (1) the authority, `k` (2) the hub, `b` (3), `m` (4); one
    /// fleet id, so every call is the same fleet.
    fn stored() -> Stored {
        let mut log = Log::default();
        log.append(Op::genesis_with_id([1; 16], "lap", pk(1), "k"), "lap", &key(1)).unwrap();
        for (name, k) in [("k", 2), ("b", 3), ("m", 4)] {
            log.append(Op::Add { name: name.into(), key: pk(k) }, "lap", &key(1)).unwrap();
        }
        Stored { log, introductions: vec![] }
    }

    fn path(p: &[&str]) -> Vec<String> {
        p.iter().map(|s| s.to_string()).collect()
    }

    fn split(s: UnixStream) -> (UnixStream, UnixStream) {
        (s.try_clone().unwrap(), s)
    }

    /// A relay `me` ([`serve`], key 2) between `up` (toward the origin,
    /// link peer `from`) and `down` (toward the destination).
    fn relay(me: &'static str, from: &'static str, stored: Stored, up: UnixStream, down: UnixStream) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let here = Here { me, key: &key(2), stored: &stored };
            let _ = serve(up, from, &here, |_| Ok(down));
        })
    }

    /// The destination `me` over `s`, its link peer `from`.
    fn destination(me: &'static str, from: &'static str, k: u8, stored: Stored, s: UnixStream) -> thread::JoinHandle<Result<Routed>> {
        thread::spawn(move || {
            let (mut r, mut w) = split(s);
            let header = match read_header(&mut r, from) {
                Ok(h) => h,
                Err(e) => {
                    refuse(&mut w, me, &format!("{e:#}"));
                    return Err(e);
                }
            };
            let out = dest_side(&mut r, &mut w, &header, me, &key(k), &stored);
            let _ = w.shutdown(Shutdown::Both);
            out
        })
    }

    struct Run {
        origin: Result<()>,
        dest: Result<Routed>,
    }

    /// `lap` (key `ok`) opening `header` to `b` (key `dk`), through `k`
    /// when the path has two hops; each end with `stored`.
    fn run(header: Header, ok: u8, dk: u8, origin_stored: Stored, dest_stored: Stored) -> Run {
        let (o, far) = UnixStream::pair().unwrap();
        let relayed = header.path.len() == 2;
        let (dest, hop) = if relayed {
            let (near, d) = UnixStream::pair().unwrap();
            (destination("b", "k", dk, dest_stored, d), Some(relay("k", "lap", stored(), far, near)))
        } else {
            (destination("b", "lap", dk, dest_stored, far), None)
        };
        let (mut r, mut w) = split(o);
        let origin = write_header(&mut w, &header).and_then(|()| origin_side(&mut r, &mut w, &header, &key(ok), &origin_stored));
        let _ = w.shutdown(Shutdown::Write);
        let dest = dest.join().unwrap();
        drop((r, w));
        if let Some(h) = hop {
            h.join().unwrap();
        }
        Run { origin, dest }
    }

    fn err<T: std::fmt::Debug>(r: Result<T>) -> String {
        format!("{:#}", r.expect_err("refused"))
    }

    #[test]
    fn a_direct_and_a_relayed_stream_open_proven() {
        for p in [&["b"][..], &["k", "b"]] {
            let h = Header::new("lap", Channel::Pty, path(p));
            let got = run(h, 1, 3, stored(), stored());
            got.origin.unwrap();
            assert_eq!(got.dest.unwrap(), Routed { origin: "lap".into(), key: pk(1), introduced_by: None, channel: Channel::Pty }, "{p:?}");
        }
    }

    #[test]
    fn the_inner_bytes_follow_the_opening() {
        let (o, d) = UnixStream::pair().unwrap();
        let dest = thread::spawn(move || {
            let (mut r, mut w) = split(d);
            let h = read_header(&mut r, "lap").unwrap();
            dest_side(&mut r, &mut w, &h, "b", &key(3), &stored()).unwrap();
            let mut rest = String::new();
            r.read_to_string(&mut rest).unwrap();
            rest
        });
        let (mut r, mut w) = split(o);
        let h = Header::new("lap", Channel::Api, path(&["b"]));
        write_header(&mut w, &h).unwrap();
        origin_side(&mut r, &mut w, &h, &key(1), &stored()).unwrap();
        w.write_all(b"hello").unwrap();
        w.shutdown(Shutdown::Write).unwrap();
        assert_eq!(dest.join().unwrap(), "hello");
    }

    #[test]
    fn an_introduced_origin_is_proven_capped() {
        let mut s = stored();
        let g = s.log.membership().unwrap().genesis;
        s.introduce(Introduction::sign(&g, "new", pk(5), "k", &key(2))).unwrap();
        let (o, d) = UnixStream::pair().unwrap();
        let dest = destination("b", "new", 3, s.clone(), d);
        let (mut r, mut w) = split(o);
        let h = Header::new("new", Channel::Tunnel, path(&["b"]));
        write_header(&mut w, &h).unwrap();
        origin_side(&mut r, &mut w, &h, &key(5), &s).unwrap();
        let got = dest.join().unwrap().unwrap();
        assert_eq!((got.key, got.introduced_by.as_deref()), (pk(5), Some("k")));
    }

    #[test]
    fn a_forged_origin_is_refused() {
        // `lap` signs with key 9: the log says key 1.
        for p in [&["b"][..], &["k", "b"]] {
            let got = run(Header::new("lap", Channel::Api, path(p)), 9, 3, stored(), stored());
            let d = err(got.dest);
            assert!(d.contains("`lap` proved key") && d.contains("not its key"), "{d}");
            let o = err(got.origin);
            assert!(o.contains("the routed stream with `b` is refused: `b`: `lap` proved key"), "{o}");
        }
    }

    #[test]
    fn a_revoked_origin_is_refused() {
        // `pi` (6) revoked, then added back with 7; the old key proves.
        let mut s = stored();
        for op in [Op::Add { name: "pi".into(), key: pk(6) }, Op::Revoke { name: "pi".into(), key: pk(6) }] {
            s.log.append(op, "lap", &key(1)).unwrap();
        }
        let gone = s.clone();
        s.log.append(Op::Add { name: "pi".into(), key: pk(7) }, "lap", &key(1)).unwrap();
        let open = |s: Stored| {
            let (o, d) = UnixStream::pair().unwrap();
            let dest = destination("b", "pi", 3, s.clone(), d);
            let (mut r, mut w) = split(o);
            let h = Header::new("pi", Channel::Api, path(&["b"]));
            write_header(&mut w, &h).unwrap();
            let origin = origin_side(&mut r, &mut w, &h, &key(6), &s);
            Run { origin, dest: dest.join().unwrap() }
        };
        let got = open(s);
        assert!(err(got.dest).contains("which is revoked"));
        assert!(err(got.origin).contains("which is revoked"));
        // Revoked and gone: an unknown name, refused at the challenge.
        let got = open(gone);
        assert!(err(got.dest).contains("`pi` is neither a member"));
        assert!(err(got.origin).contains("is refused: `b`: `pi` is neither a member"));
    }

    /// Writes into `w` and keeps a copy.
    struct Tee<W>(W, Vec<u8>);

    impl<W: Write> Write for Tee<W> {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let n = self.0.write(buf)?;
            self.1.extend_from_slice(&buf[..n]);
            Ok(n)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.0.flush()
        }
    }

    #[test]
    fn a_replayed_proof_is_refused() {
        let h = Header::new("lap", Channel::Api, path(&["b"]));
        // Record the origin's proof from one opening.
        let (o, d) = UnixStream::pair().unwrap();
        let dest = destination("b", "lap", 3, stored(), d);
        let (mut r, w) = split(o);
        let mut tee = Tee(w, Vec::new());
        write_header(&mut tee.0, &h).unwrap();
        origin_side(&mut r, &mut tee, &h, &key(1), &stored()).unwrap();
        dest.join().unwrap().unwrap();
        let len = u32::from_be_bytes(tee.1[..4].try_into().unwrap()) as usize;
        let recorded = tee.1[..4 + len].to_vec();
        // Replay it in a new one: another challenge.
        let (o, d) = UnixStream::pair().unwrap();
        let dest = destination("b", "lap", 3, stored(), d);
        let (mut r, mut w) = split(o);
        write_header(&mut w, &h).unwrap();
        read_msg(&mut r, SMALL, "b").unwrap();
        w.write_all(&recorded).unwrap();
        let e = err(dest.join().unwrap());
        assert!(e.contains("doesn't verify"), "{e}");
        assert!(err(read_msg(&mut r, SMALL, "b")).contains("is refused: `b`: `lap`'s key proof doesn't verify"));
    }

    #[test]
    fn a_relay_posing_as_the_destination_is_refused() {
        // `k` answers the opening to `b` itself, with its own key.
        let (o, d) = UnixStream::pair().unwrap();
        let posing = destination("b", "lap", 2, stored(), d);
        let (mut r, mut w) = split(o);
        let h = Header::new("lap", Channel::Api, path(&["k", "b"]));
        write_header(&mut w, &h).unwrap();
        let o = err(origin_side(&mut r, &mut w, &h, &key(1), &stored()));
        assert!(o.contains("`b` proved key") && o.contains("not its key"), "{o}");
        assert!(err(posing.join().unwrap()).contains("is refused: `lap`: `b` proved key"));
    }

    /// `header` written unchecked to a relay `k` (or straight to `b`): the
    /// origin's error.
    fn raw(body: Vec<u8>, relayed: bool) -> String {
        let (o, far) = UnixStream::pair().unwrap();
        let (dest, hop) = if relayed {
            let (near, d) = UnixStream::pair().unwrap();
            (destination("b", "k", 3, stored(), d), Some(relay("k", "lap", stored(), far, near)))
        } else {
            (destination("b", "lap", 3, stored(), far), None)
        };
        let (mut r, mut w) = split(o);
        let _ = write_raw(&mut w, MESSAGE, &body);
        let got = err(read_msg(&mut r, SMALL, "b"));
        drop((r, w));
        let _ = dest.join().unwrap();
        if let Some(h) = hop {
            h.join().unwrap();
        }
        got
    }

    #[test]
    fn a_bad_header_is_refused_where_it_arrives() {
        let h = |ttl: u8, channel: Channel, p: &[&str]| Header { ttl, ..Header::new("lap", channel, path(p)) };
        let cases: [(Header, bool, &str); 8] = [
            (h(0, Channel::Api, &["b"]), false, "`b`: the routed stream from `lap` to `b`: its TTL ran out"),
            (h(0, Channel::Api, &["k", "b"]), true, "`k`: the routed stream from `lap` to `b`: its TTL ran out"),
            (h(1, Channel::Api, &["k", "b"]), true, "`k`: the routed stream from `lap` to `b`: its TTL 1 doesn't cover the 2 hops left"),
            (h(4, Channel::Api, &["k", "lap", "b"]), false, "its path loops through `lap`"),
            (h(4, Channel::Api, &["k", "k", "b"]), true, "its path loops through `k`"),
            (h(4, Channel::Agent, &["b"]), false, "`agent` isn't routed"),
            (h(4, Channel::Route, &["k", "b"]), true, "a routed stream can't carry another"),
            (Header { dest: "b".into(), ..h(4, Channel::Api, &["k", "m"]) }, false, "its path doesn't end in its destination `b`"),
        ];
        for (header, relayed, want) in cases {
            // The TTL's cover of the hops left is a relay's check.
            let relay_only = want.contains("doesn't cover");
            assert_eq!(write_header(&mut Vec::new(), &header).is_err(), !relay_only, "{header:?}");
            let e = raw(header.encode(), relayed);
            assert!(e.contains(want) && e.starts_with("the routed stream with `b` is refused: "), "{e}");
        }
        let e = raw(vec![0; HEADER_LIMIT + 1], false);
        assert!(e.contains("`b`: `lap` sent a routed stream header of 4098 bytes (at most 4097)"), "{e}");
        assert!(raw(b"junk".to_vec(), true).contains("`k`: `lap` sent a malformed routed stream header"));
    }

    #[test]
    fn a_relay_forwards_one_ttl_lower_to_known_members_only() {
        let s = stored();
        let h = Header::new("lap", Channel::Transfer, path(&["k", "m", "b"]));
        let hop = relay_header(&h, "k", &s).unwrap();
        assert_eq!((hop.next.as_str(), hop.header.ttl), ("m", 3));
        let hop = relay_header(&hop.header, "m", &s).unwrap();
        assert_eq!((hop.next.as_str(), hop.header.ttl), ("b", 2));
        assert!(format!("{:#}", relay_header(&h, "b", &s).unwrap_err()).contains("`b` is its destination"));
        assert!(format!("{:#}", relay_header(&h, "x", &s).unwrap_err()).contains("`x` isn't on its path"));
        let far = Header::new("lap", Channel::Api, path(&["k", "zed"]));
        assert!(format!("{:#}", relay_header(&far, "k", &s).unwrap_err()).contains("`zed` is neither a member"));
        let long = Header::new("lap", Channel::Api, path(&["k", "m", "b", "x", "y"]));
        assert!(format!("{:#}", long.check().unwrap_err()).contains("5 hops (at most 4)"));
    }

    #[test]
    fn a_stream_from_a_peer_off_its_path_is_refused() {
        let h = Header::new("lap", Channel::Api, path(&["k", "b"]));
        came_from(&h, "k", "lap").unwrap();
        came_from(&h, "b", "k").unwrap();
        let e = format!("{:#}", came_from(&h, "b", "lap").unwrap_err());
        assert!(e.contains("it came from `lap`, not from `k`, the hop before `b`"), "{e}");
        // `m` posing as the origin's first hop; `k` sent `b`'s stream back.
        assert!(came_from(&h, "k", "m").is_err());
        assert!(came_from(&Header::new("b", Channel::Api, path(&["k", "lap"])), "k", "lap").is_err());
        // Over a stream: the relay refuses it at once.
        let (o, far) = UnixStream::pair().unwrap();
        let (near, _d) = UnixStream::pair().unwrap();
        let hop = relay("k", "m", stored(), far, near);
        let (mut r, mut w) = split(o);
        write_header(&mut w, &h).unwrap();
        let e = err(origin_side(&mut r, &mut w, &h, &key(1), &stored()));
        assert!(e.contains("is refused: `k`: the routed stream from `lap` to `b`: it came from `m`"), "{e}");
        hop.join().unwrap();
    }

    #[test]
    fn a_relay_whose_next_hop_is_down_refuses() {
        let (o, far) = UnixStream::pair().unwrap();
        let hop = thread::spawn(move || {
            let s = stored();
            let here = Here { me: "k", key: &key(2), stored: &s };
            serve(far, "lap", &here, |next| Err(format!("its link to `{next}` is unreachable"))).map(|_| ())
        });
        let (mut r, mut w) = split(o);
        let h = Header::new("lap", Channel::Api, path(&["k", "b"]));
        write_header(&mut w, &h).unwrap();
        let e = err(origin_side(&mut r, &mut w, &h, &key(1), &stored()));
        assert_eq!(e, "the routed stream with `b` is refused: `k`: the routed stream from `lap` to `b`: its link to `b` is unreachable");
        assert!(hop.join().unwrap().is_err());
    }

    #[test]
    fn a_stream_for_another_member_is_refused() {
        let (o, d) = UnixStream::pair().unwrap();
        let dest = destination("m", "lap", 4, stored(), d);
        let (mut r, mut w) = split(o);
        let h = Header::new("lap", Channel::Api, path(&["b"]));
        write_header(&mut w, &h).unwrap();
        assert!(err(origin_side(&mut r, &mut w, &h, &key(1), &stored())).contains("`m`: the routed stream from `lap` to `b` isn't for `m`"));
        assert!(dest.join().unwrap().is_err());
    }
}
