//! A routed stream's end-to-end session (docs/architecture/daemon/fleet.md,
//! *Routed streams*; fleet plan step 10c, decisions 61-63): what lets the
//! relays between a stream's origin and its destination carry ciphertext
//! only, so they can't read, change, inject into, replay or reflect it.
//! Pure over `Read`/`Write` (and a socket pair for [`pump`]); the opening
//! that authenticates the keys is `route`'s.
//!
//! - **The key agreement** ([`Ephemeral`]): each end's fresh x25519 key,
//!   carried in the opening and signed in its transcript (decision 61); a
//!   DH output that isn't contributory (the peer sent a low-order key) is
//!   refused.
//! - **The keys** ([`Keys::derive`]): HKDF-SHA256, the transcript's SHA-256
//!   as salt and the DH output as input, expanded once per direction with
//!   the direction in the info ([`O_TO_D`], [`D_TO_O`]); each end seals
//!   with its own direction's and opens with the other's, so a record
//!   reflected back to its sender doesn't open (decision 62).
//! - **The records** ([`Sealer`], [`Opener`]; decision 63): a `u16`
//!   big-endian length, then that many bytes of ChaCha20-Poly1305
//!   ciphertext and tag, of at most [`MAX_RECORD`] bytes of plaintext. The
//!   nonce is the direction's record counter (big-endian in the last 8 of
//!   its 12 bytes, from 0, never wrapping), so a dropped, reordered or
//!   replayed record doesn't open. The record's type ([`DATA`] or [`END`])
//!   is its associated data and follows from its length: an END's
//!   plaintext is empty and a DATA's never is, so a reader opens an
//!   empty record only as an END and any other only as a DATA, and a
//!   DATA can't pass for an END (or the reverse) without the tag failing.
//!   Each direction ends with an END; a bare EOF, a bad tag, a length out
//!   of bounds or a byte after the END is an error.
//! - **The pump** ([`pump`]): a plaintext stream (the inner channel's
//!   handler) and a sealed one (toward the relays), each read sealed and
//!   sent at once (`PTY` and `TUNNEL` are interactive), an END a
//!   half-close; any error tears both directions down, never a half-close.

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow, bail};
use chacha20poly1305::ChaCha20Poly1305;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey, SharedSecret, StaticSecret};

/// The most plaintext one record carries.
pub const MAX_RECORD: usize = 16 * 1024;
/// A record's tag, the ciphertext's bytes beyond its plaintext.
const TAG: usize = 16;

/// A data record's type (its associated data).
pub const DATA: u8 = 0;
/// An end record's type: its direction's authenticated end.
pub const END: u8 = 1;

/// The origin-to-destination direction's HKDF info.
pub const O_TO_D: &[u8] = b"devsandbox/route/v2 origin->destination";
/// The destination-to-origin direction's HKDF info.
pub const D_TO_O: &[u8] = b"devsandbox/route/v2 destination->origin";

/// This end's ephemeral x25519 key for one session: used once, by
/// [`Ephemeral::agree`]. (`StaticSecret` only for its from-bytes
/// constructor: the seed comes from the OS RNG, as `route`'s nonces do,
/// rather than through `rand_core`.)
pub struct Ephemeral(StaticSecret);

impl Ephemeral {
    pub fn new() -> Result<Ephemeral> {
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).map_err(|e| anyhow!("cannot read the OS random source: {e}"))?;
        Ok(Ephemeral(StaticSecret::from(seed)))
    }

    /// The public half, for the opening.
    pub fn public(&self) -> [u8; 32] {
        PublicKey::from(&self.0).to_bytes()
    }

    /// The DH output with the peer's public key `theirs`, consuming this
    /// key. A non-contributory output (a low-order `theirs`: the output
    /// is then known to anyone) is refused.
    pub fn agree(self, theirs: &[u8; 32]) -> Result<SharedSecret> {
        let dh = self.0.diffie_hellman(&PublicKey::from(*theirs));
        if !dh.was_contributory() {
            bail!("the peer's session key is a low-order point (a non-contributory key agreement)");
        }
        Ok(dh)
    }
}

/// Which end of a routed stream this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Origin,
    Destination,
}

/// One direction's cipher (opaque; zeroized on drop).
#[derive(Clone)]
pub struct DirectionKey(ChaCha20Poly1305);

/// A session's two directions: `seal` this end's own, `open` the peer's.
pub struct Keys {
    pub seal: DirectionKey,
    pub open: DirectionKey,
}

impl Keys {
    /// Both directions' keys from the DH output `dh` and the opening's
    /// `transcript` (both ephemeral keys among it), for `role`'s end.
    pub fn derive(dh: &SharedSecret, transcript: &[u8], role: Role) -> Keys {
        let salt = Sha256::digest(transcript);
        let hk = hkdf::Hkdf::<Sha256>::new(Some(salt.as_slice()), dh.as_bytes());
        let expand = |info: &[u8]| {
            let mut okm = [0u8; 32];
            hk.expand(info, &mut okm).expect("32 bytes is a valid HKDF-SHA256 length");
            DirectionKey(ChaCha20Poly1305::new(&okm.into()))
        };
        let (o_to_d, d_to_o) = (expand(O_TO_D), expand(D_TO_O));
        match role {
            Role::Origin => Keys { seal: o_to_d, open: d_to_o },
            Role::Destination => Keys { seal: d_to_o, open: o_to_d },
        }
    }
}

/// A direction's record counter: the next nonce, `None` once the last
/// one is spent (a counter never wraps: a nonce is used once).
struct Counter(Option<u64>);

impl Counter {
    fn next(&mut self) -> Result<chacha20poly1305::Nonce> {
        let Some(n) = self.0 else { bail!("the session's record counter is spent") };
        self.0 = n.checked_add(1);
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(&n.to_be_bytes());
        Ok(nonce.into())
    }
}

/// The sealing end of one direction over `w` (module doc, *The records*).
pub struct Sealer<W: Write> {
    w: W,
    key: DirectionKey,
    counter: Counter,
    ended: bool,
}

impl<W: Write> Sealer<W> {
    pub fn new(w: W, key: DirectionKey) -> Sealer<W> {
        Sealer { w, key, counter: Counter(Some(0)), ended: false }
    }

    /// `data` as records of at most [`MAX_RECORD`], written and flushed.
    /// Empty `data` writes nothing (an empty record is an END).
    pub fn seal_data(&mut self, data: &[u8]) -> Result<()> {
        for chunk in data.chunks(MAX_RECORD) {
            self.record(DATA, chunk)?;
        }
        self.w.flush()?;
        Ok(())
    }

    /// This direction's END, flushed; nothing is sealed after it.
    pub fn seal_end(&mut self) -> Result<()> {
        self.record(END, &[])?;
        self.ended = true;
        self.w.flush()?;
        Ok(())
    }

    fn record(&mut self, ty: u8, msg: &[u8]) -> Result<()> {
        if self.ended {
            bail!("the session's direction already ended");
        }
        let nonce = self.counter.next()?;
        let ct = self.key.0.encrypt(&nonce, Payload { msg, aad: &[ty] }).map_err(|_| anyhow!("cannot seal a session record"))?;
        let mut out = Vec::with_capacity(2 + ct.len());
        out.extend_from_slice(&(ct.len() as u16).to_be_bytes());
        out.extend_from_slice(&ct);
        self.w.write_all(&out)?;
        Ok(())
    }
}

/// The opening end of one direction over `r` (module doc, *The records*).
pub struct Opener<R: Read> {
    r: R,
    key: DirectionKey,
    counter: Counter,
    ended: bool,
}

impl<R: Read> Opener<R> {
    pub fn new(r: R, key: DirectionKey) -> Opener<R> {
        Opener { r, key, counter: Counter(Some(0)), ended: false }
    }

    /// The next record's plaintext, `None` at the direction's END and,
    /// after it, at the stream's EOF. Reads one record at a time.
    pub fn next(&mut self) -> Result<Option<Vec<u8>>> {
        let mut len = [0u8; 2];
        let got = read_full(&mut self.r, &mut len)?;
        if self.ended {
            if got == 0 {
                return Ok(None);
            }
            bail!("the session's stream has bytes after its end record");
        }
        match got {
            0 => bail!("the session's stream ended without its end record"),
            2 => {}
            _ => bail!("the session's stream ended inside a record"),
        }
        let len = u16::from_be_bytes(len) as usize;
        if !(TAG..=MAX_RECORD + TAG).contains(&len) {
            bail!("a session record's length ({len}) is out of bounds");
        }
        let mut ct = vec![0u8; len];
        if read_full(&mut self.r, &mut ct)? != len {
            bail!("the session's stream ended inside a record");
        }
        let ty = if len == TAG { END } else { DATA };
        let nonce = self.counter.next()?;
        let msg = self
            .key
            .0
            .decrypt(&nonce, Payload { msg: &ct, aad: &[ty] })
            .map_err(|_| anyhow!("a session record doesn't authenticate (altered, out of order, replayed or reflected)"))?;
        if ty == END {
            self.ended = true;
            return Ok(None);
        }
        Ok(Some(msg))
    }
}

/// Fills `buf` from `r` up to EOF: the bytes read (fewer than `buf.len()`
/// only at EOF).
fn read_full(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut at = 0;
    while at < buf.len() {
        match r.read(&mut buf[at..]) {
            Ok(0) => break,
            Ok(n) => at += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(at)
}

/// Pumps a session between `plain` (the inner channel's handler) and
/// `sealed` (toward the peer, through the relays) with `keys`, one
/// direction on a `route-session` thread, the other on this one, until
/// both end (modeled on `route::splice`). Each read of `plain` is sealed
/// and sent at once; its EOF is an END and `sealed`'s half-close. An END
/// read from `sealed` is `plain`'s half-close. Any error (a record that
/// doesn't open, a bare EOF, a failed write) shuts both sockets down both
/// ways, so the other direction ends too and the handler sees the stream
/// die, not end; it's the first error returned.
pub fn pump(plain: UnixStream, sealed: UnixStream, keys: Keys) -> Result<()> {
    let first: Arc<Mutex<Option<anyhow::Error>>> = Arc::new(Mutex::new(None));
    let (plain2, sealed2) = (plain.try_clone()?, sealed.try_clone()?);
    let Keys { seal, open } = keys;
    let fail = {
        let (plain, sealed, first) = (plain.try_clone()?, sealed.try_clone()?, first.clone());
        Arc::new(move |e: anyhow::Error| {
            first.lock().unwrap_or_else(|p| p.into_inner()).get_or_insert(e);
            let _ = sealed.shutdown(Shutdown::Both);
            let _ = plain.shutdown(Shutdown::Both);
        })
    };
    let fail2 = fail.clone();
    let back = std::thread::Builder::new().name("route-session".into()).spawn(move || {
        if let Err(e) = open_into(&sealed2, &plain2, open) {
            fail2(e);
        }
    });
    let back = match back {
        Ok(t) => t,
        Err(e) => {
            fail(e.into());
            return Err(first.lock().unwrap_or_else(|p| p.into_inner()).take().expect("set by fail"));
        }
    };
    if let Err(e) = seal_from(&plain, &sealed, seal) {
        fail(e);
    }
    let _ = back.join();
    match first.lock().unwrap_or_else(|p| p.into_inner()).take() {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// `plain` → `sealed`: each read one or more records at once, EOF an END.
fn seal_from(plain: &UnixStream, sealed: &UnixStream, key: DirectionKey) -> Result<()> {
    let mut sealer = Sealer::new(sealed, key);
    let mut buf = vec![0u8; MAX_RECORD];
    loop {
        let n = match (&mut &*plain).read(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        if n == 0 {
            sealer.seal_end()?;
            sealed.shutdown(Shutdown::Write)?;
            return Ok(());
        }
        sealer.seal_data(&buf[..n])?;
    }
}

/// `sealed` → `plain`: each record's plaintext written, an END `plain`'s
/// half-close, then the stream's EOF expected (a byte after it is an error).
fn open_into(sealed: &UnixStream, plain: &UnixStream, key: DirectionKey) -> Result<()> {
    let mut opener = Opener::new(sealed, key);
    while let Some(data) = opener.next()? {
        (&mut &*plain).write_all(&data)?;
    }
    plain.shutdown(Shutdown::Write)?;
    if opener.next()?.is_some() {
        unreachable!("an opener yields nothing after its end");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    const WAIT: Duration = Duration::from_secs(10);

    fn pair_keys() -> (Keys, Keys) {
        let (o, d) = (Ephemeral::new().unwrap(), Ephemeral::new().unwrap());
        let (op, dp) = (o.public(), d.public());
        let mut transcript = b"transcript".to_vec();
        transcript.extend_from_slice(&op);
        transcript.extend_from_slice(&dp);
        let (odh, ddh) = (o.agree(&dp).unwrap(), d.agree(&op).unwrap());
        assert_eq!(odh.as_bytes(), ddh.as_bytes());
        (Keys::derive(&odh, &transcript, Role::Origin), Keys::derive(&ddh, &transcript, Role::Destination))
    }

    fn sealed(key: &DirectionKey, data: &[&[u8]], end: bool) -> Vec<u8> {
        let mut s = Sealer::new(Vec::new(), key.clone());
        for d in data {
            s.seal_data(d).unwrap();
        }
        if end {
            s.seal_end().unwrap();
        }
        s.w
    }

    /// A sealed byte stream split into its records (header included).
    fn records(bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut at = 0;
        while at < bytes.len() {
            let len = u16::from_be_bytes([bytes[at], bytes[at + 1]]) as usize;
            out.push(bytes[at..at + 2 + len].to_vec());
            at += 2 + len;
        }
        out
    }

    /// Everything `key` opens from `bytes`: the data, then `Ok` at an END
    /// with EOF after it, else the error.
    fn open_all(key: &DirectionKey, bytes: &[u8]) -> (Vec<u8>, Result<()>) {
        let mut o = Opener::new(bytes, key.clone());
        let mut data = Vec::new();
        loop {
            match o.next() {
                Ok(Some(d)) => data.extend(d),
                Ok(None) => return (data, o.next().map(|n| assert!(n.is_none()))),
                Err(e) => return (data, Err(e)),
            }
        }
    }

    fn opens_with(rx: &DirectionKey, tx: &DirectionKey) -> bool {
        open_all(rx, &sealed(tx, &[b"x"], true)).1.is_ok()
    }

    #[test]
    fn both_roles_derive_matching_keys_and_distinct_directions() {
        let (o, d) = pair_keys();
        assert!(opens_with(&d.open, &o.seal));
        assert!(opens_with(&o.open, &d.seal));
        assert!(!opens_with(&o.open, &o.seal), "a direction's record reflected to its sender opens");
        assert!(!opens_with(&d.open, &d.seal));
    }

    #[test]
    fn a_different_transcript_derives_different_keys() {
        let (o, d) = (Ephemeral::new().unwrap(), Ephemeral::new().unwrap());
        let (op, dp) = (o.public(), d.public());
        let (odh, ddh) = (o.agree(&dp).unwrap(), d.agree(&op).unwrap());
        let ko = Keys::derive(&odh, b"one", Role::Origin);
        let kd = Keys::derive(&ddh, b"two", Role::Destination);
        assert!(!opens_with(&kd.open, &ko.seal));
    }

    #[test]
    fn a_low_order_key_is_refused() {
        let e = Ephemeral::new().unwrap();
        let err = e.agree(&[0u8; 32]).err().expect("an all-zero key is refused");
        assert!(err.to_string().contains("non-contributory"), "{err:#}");
    }

    #[test]
    fn records_round_trip_split_at_the_bound() {
        let (o, d) = pair_keys();
        let big = vec![7u8; MAX_RECORD * 2 + 5];
        let bytes = sealed(&o.seal, &[b"hi", &big], true);
        assert_eq!(records(&bytes).len(), 1 + 3 + 1);
        let (data, end) = open_all(&d.open, &bytes);
        end.unwrap();
        assert_eq!(&data[..2], b"hi");
        assert_eq!(&data[2..], &big[..]);
    }

    /// `edit` applied to a two-data-record stream with its END: the data
    /// it still opens and the error.
    fn tampered(edit: impl FnOnce(&mut Vec<Vec<u8>>)) -> (Vec<u8>, String) {
        let (o, d) = pair_keys();
        let mut recs = records(&sealed(&o.seal, &[b"one", b"two"], true));
        edit(&mut recs);
        let (data, end) = open_all(&d.open, &recs.concat());
        (data, format!("{:#}", end.err().expect("the tampering is detected")))
    }

    #[test]
    fn a_flipped_byte_is_detected() {
        let (data, err) = tampered(|r| r[0][5] ^= 1);
        assert!(data.is_empty());
        assert!(err.contains("authenticate"), "{err}");
    }

    #[test]
    fn a_dropped_record_is_detected() {
        let (data, err) = tampered(|r| drop(r.remove(0)));
        assert!(data.is_empty());
        assert!(err.contains("authenticate"), "{err}");
    }

    #[test]
    fn a_dropped_end_is_a_bare_eof() {
        let (data, err) = tampered(|r| drop(r.pop()));
        assert_eq!(data, b"onetwo");
        assert!(err.contains("without its end record"), "{err}");
    }

    #[test]
    fn swapped_records_are_detected() {
        let (data, err) = tampered(|r| r.swap(0, 1));
        assert!(data.is_empty());
        assert!(err.contains("authenticate"), "{err}");
    }

    #[test]
    fn a_replayed_record_is_detected() {
        let (data, err) = tampered(|r| {
            let first = r[0].clone();
            r.insert(1, first);
        });
        assert_eq!(data, b"one");
        assert!(err.contains("authenticate"), "{err}");
    }

    #[test]
    fn a_record_after_the_end_is_detected() {
        let (data, err) = tampered(|r| {
            let first = r[0].clone();
            r.push(first);
        });
        assert_eq!(data, b"onetwo");
        assert!(err.contains("after its end"), "{err}");
    }

    #[test]
    fn a_truncation_inside_a_record_is_detected() {
        let (data, err) = tampered(|r| {
            r[1].truncate(4);
            r.truncate(2);
        });
        assert_eq!(data, b"one");
        assert!(err.contains("inside a record"), "{err}");
    }

    #[test]
    fn a_data_record_cant_pass_for_an_end() {
        // A DATA's tag over an empty body: the length says END, the AD
        // doesn't match.
        let (_, err) = tampered(|r| {
            r[1].truncate(2 + TAG);
            r[1][..2].copy_from_slice(&(TAG as u16).to_be_bytes());
        });
        assert!(err.contains("authenticate"), "{err}");
    }

    #[test]
    fn a_reflected_record_is_detected() {
        let (o, _) = pair_keys();
        let (data, end) = open_all(&o.open, &sealed(&o.seal, &[b"one"], true));
        assert!(data.is_empty());
        assert!(format!("{:#}", end.unwrap_err()).contains("authenticate"));
    }

    #[test]
    fn an_oversized_length_is_refused_before_its_body() {
        let (_, d) = pair_keys();
        let header = ((MAX_RECORD + TAG + 1) as u16).to_be_bytes();
        let (_, end) = open_all(&d.open, &header);
        assert!(format!("{:#}", end.unwrap_err()).contains("out of bounds"));
        let (_, end) = open_all(&d.open, &[0, 3, 1, 2, 3]);
        assert!(format!("{:#}", end.unwrap_err()).contains("out of bounds"));
    }

    #[test]
    fn the_counter_never_wraps() {
        let (o, _) = pair_keys();
        let mut s = Sealer::new(Vec::new(), o.seal);
        s.counter = Counter(Some(u64::MAX));
        s.seal_data(b"last").unwrap();
        assert!(format!("{:#}", s.seal_data(b"wrapped").unwrap_err()).contains("spent"));
    }

    /// The bytes from `from` to `to`, each record (whole, header included)
    /// through `edit` (`None` drops the rest and ends there); `to`'s
    /// half-close at `from`'s EOF, both down at an error.
    fn middle(from: UnixStream, to: UnixStream, mut edit: impl FnMut(usize, Vec<u8>) -> Option<Vec<u8>> + Send + 'static) {
        std::thread::spawn(move || {
            let mut i = 0;
            let ok = (|| -> io::Result<()> {
                loop {
                    let mut len = [0u8; 2];
                    if read_full(&mut &from, &mut len)? < 2 {
                        return Ok(());
                    }
                    let mut rec = len.to_vec();
                    rec.resize(2 + u16::from_be_bytes(len) as usize, 0);
                    (&from).read_exact(&mut rec[2..])?;
                    let Some(out) = edit(i, rec) else { return Ok(()) };
                    (&to).write_all(&out)?;
                    i += 1;
                }
            })();
            if ok.is_ok() {
                let _ = to.shutdown(Shutdown::Write);
            } else {
                let _ = to.shutdown(Shutdown::Both);
                let _ = from.shutdown(Shutdown::Both);
            }
        });
    }

    struct Ends {
        a: UnixStream,
        b: UnixStream,
        pumps: mpsc::Receiver<(Role, Result<()>)>,
    }

    impl Ends {
        fn results(&self) -> Vec<(Role, Result<()>)> {
            let mut out: Vec<_> = (0..2).map(|_| self.pumps.recv_timeout(WAIT).expect("both pumps return")).collect();
            out.sort_by_key(|(r, _)| *r == Role::Destination);
            out
        }
    }

    /// Plain `a` ↔ the origin's pump ↔ the middle (one edit per
    /// direction) ↔ the destination's pump ↔ plain `b`.
    fn wired(
        to_d: impl FnMut(usize, Vec<u8>) -> Option<Vec<u8>> + Send + 'static,
        to_o: impl FnMut(usize, Vec<u8>) -> Option<Vec<u8>> + Send + 'static,
    ) -> Ends {
        let (ok, dk) = pair_keys();
        let (a, a_pump) = UnixStream::pair().unwrap();
        let (b, b_pump) = UnixStream::pair().unwrap();
        let (o_sealed, mid_o) = UnixStream::pair().unwrap();
        let (mid_d, d_sealed) = UnixStream::pair().unwrap();
        middle(mid_o.try_clone().unwrap(), mid_d.try_clone().unwrap(), to_d);
        middle(mid_d, mid_o, to_o);
        let (tx, pumps) = mpsc::channel();
        for (role, plain, sealed, keys) in [(Role::Origin, a_pump, o_sealed, ok), (Role::Destination, b_pump, d_sealed, dk)] {
            let tx = tx.clone();
            std::thread::spawn(move || tx.send((role, pump(plain, sealed, keys))));
        }
        for s in [&a, &b] {
            s.set_read_timeout(Some(WAIT)).unwrap();
        }
        Ends { a, b, pumps }
    }

    fn pass(_: usize, rec: Vec<u8>) -> Option<Vec<u8>> {
        Some(rec)
    }

    fn read_n(s: &UnixStream, n: usize) -> Vec<u8> {
        let mut buf = vec![0u8; n];
        (&mut &*s).read_exact(&mut buf).unwrap();
        buf
    }

    fn read_to_end(s: &UnixStream) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        (&mut &*s).read_to_end(&mut out).map(|_| out)
    }

    #[test]
    fn pumps_back_to_back_carry_both_ways_and_half_close_each_way() {
        let e = wired(pass, pass);
        (&e.a).write_all(b"ping").unwrap();
        assert_eq!(read_n(&e.b, 4), b"ping");
        (&e.b).write_all(b"pong").unwrap();
        assert_eq!(read_n(&e.a, 4), b"pong");

        let big: Vec<u8> = (0..MAX_RECORD * 5 + 123).map(|i| i as u8).collect();
        let a = e.a.try_clone().unwrap();
        let sent = big.clone();
        let writer = std::thread::spawn(move || {
            (&a).write_all(&sent).unwrap();
            a.shutdown(Shutdown::Write).unwrap();
        });
        assert_eq!(read_to_end(&e.b).unwrap(), big, "A's bytes, then A's half-close");
        writer.join().unwrap();

        (&e.b).write_all(b"after A's end").unwrap();
        assert_eq!(read_n(&e.a, 13), b"after A's end");
        e.b.shutdown(Shutdown::Write).unwrap();
        assert_eq!(read_to_end(&e.a).unwrap(), b"");
        for (role, r) in e.results() {
            r.unwrap_or_else(|err| panic!("{role:?}'s pump: {err:#}"));
        }
    }

    /// A torn-down plain end: its read ends (EOF or reset) before `want`
    /// all arrived, and a write fails soon.
    fn torn_down(s: &UnixStream, want: &[u8]) {
        let got = read_to_end(s).unwrap_or_default();
        assert!(want.is_empty() || got != want, "a full stream with a clean end");
        // A clean half-close would still take writes; a torn-down end doesn't.
        let deadline = std::time::Instant::now() + WAIT;
        while (&mut &*s).write_all(b"x").is_ok() {
            assert!(std::time::Instant::now() < deadline, "writes still go through");
            std::thread::yield_now();
        }
    }

    #[test]
    fn a_flipped_byte_tears_the_session_down() {
        let e = wired(|i, mut rec| {
            if i == 1 {
                rec[4] ^= 0x80;
            }
            Some(rec)
        }, pass);
        (&e.a).write_all(b"one").unwrap();
        assert_eq!(read_n(&e.b, 3), b"one");
        (&e.a).write_all(b"two").unwrap();
        torn_down(&e.b, b"two");
        torn_down(&e.a, b"");
        let rs = e.results();
        let (_, d) = &rs[1];
        let err = format!("{:#}", d.as_ref().expect_err("the destination's pump fails"));
        assert!(err.contains("authenticate"), "{err}");
        assert!(rs[0].1.is_err(), "the origin's pump fails too (its peer tore down)");
    }

    #[test]
    fn a_truncation_tears_the_session_down() {
        // The middle swallows the origin's END and closes: a bare EOF.
        let e = wired(|i, rec| (i == 0).then_some(rec), pass);
        (&e.a).write_all(b"one").unwrap();
        assert_eq!(read_n(&e.b, 3), b"one");
        e.a.shutdown(Shutdown::Write).unwrap();
        // The destination saw no END: B can't write on as after a half-close.
        torn_down(&e.b, b"");
        let rs = e.results();
        let err = format!("{:#}", rs[1].1.as_ref().expect_err("the destination's pump fails"));
        assert!(err.contains("without its end record"), "{err}");
    }

    #[test]
    fn a_reflected_record_tears_the_session_down() {
        // The middle sends the origin's own records back to it.
        let (ok, _) = pair_keys();
        let (a, a_pump) = UnixStream::pair().unwrap();
        let (o_sealed, mid) = UnixStream::pair().unwrap();
        a.set_read_timeout(Some(WAIT)).unwrap();
        middle(mid.try_clone().unwrap(), mid, pass);
        let t = std::thread::spawn(move || pump(a_pump, o_sealed, ok));
        (&a).write_all(b"echo?").unwrap();
        torn_down(&a, b"echo?");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || tx.send(t.join().unwrap()));
        let err = format!("{:#}", rx.recv_timeout(WAIT).unwrap().unwrap_err());
        assert!(err.contains("authenticate"), "{err}");
    }
}
