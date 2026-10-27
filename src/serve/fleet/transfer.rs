//! One bulk transfer on a fleet link's `TRANSFER` stream
//! (docs/architecture/daemon/fleet.md, *Transfer*): the sender writes a
//! [`Header`] line; the receiver answers `ready` (or `error <why>`, and
//! no body is sent); the sender writes exactly `size` raw bytes, then
//! half-closes; the receiver writes the body into a temp file next to its
//! destination, checks its size and sha256, renames it into place and
//! answers one line, `ok` or `error <why>`; then both close. The `ready`
//! step is there because a receiver that refuses and closes while the body
//! is still coming loses its answer on the mux: the sender would only see a
//! broken pipe. A body longer or shorter than
//! `size`, a hash mismatch, a `size` over the receiver's limit or a kind
//! nobody receives is an `error`, the temp file removed.
//!
//! [`send`] and [`receive`] are the two ends over any byte stream;
//! [`Receivers`] is an end's registry of what it accepts, by kind, which the
//! link hands each `TRANSFER` stream the peer opens (`link::opener_with`,
//! `link::answerer_with`).

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::devsbd::mux::Conn;

/// How long the header line may be, newline included.
pub const HEADER_MAX: u64 = 4096;

/// How long the receiver's answer line may be.
const REPLY_MAX: u64 = 4096;

/// The receiver's answer to a header it takes: send the body.
pub const READY: &str = "ready\n";

/// How much past its header's `size` a bad body is read and dropped before
/// the receiver answers (a longer one may lose the answer to the sender's
/// write error).
pub const DRAIN_EXTRA: u64 = 64 * 1024;

/// What a transfer carries, the first line on its stream (JSON).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Header {
    /// What the body is, which picks its receiver: `binary`, `root-tar`,
    /// `bundle`. `[a-z0-9-]`, at most 32 bytes.
    pub kind: String,
    /// The body's file name for the receiver (no `/`, not `.` or `..`).
    pub name: String,
    /// The body's length in bytes.
    pub size: u64,
    /// The body's sha256, lowercase hex.
    pub sha256: String,
}

/// A transfer the receiver took: its header and where the body now is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Received {
    pub header: Header,
    pub path: PathBuf,
}

/// `header` as its line, newline included.
pub fn encode_header(header: &Header) -> String {
    let mut line = serde_json::to_string(header).expect("a header serializes");
    line.push('\n');
    line
}

/// A header line (the newline optional), checked by [`check_header`].
pub fn parse_header(line: &[u8]) -> Result<Header> {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let header: Header = serde_json::from_slice(line).context("bad transfer header")?;
    check_header(&header)?;
    Ok(header)
}

/// What every header must be, whatever its receiver.
pub fn check_header(h: &Header) -> Result<()> {
    if h.kind.is_empty() || h.kind.len() > 32 || !h.kind.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
        bail!("bad transfer kind `{}`", h.kind);
    }
    if h.name.is_empty() || h.name.len() > 255 || h.name == "." || h.name == ".." || h.name.contains(['/', '\0', '\n']) {
        bail!("bad transfer name {:?}", h.name);
    }
    if h.sha256.len() != 64 || !h.sha256.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        bail!("bad transfer sha256 `{}`", h.sha256);
    }
    Ok(())
}

/// `size` against a receiver's `limit`.
pub fn check_size(size: u64, limit: u64) -> Result<()> {
    if size > limit {
        bail!("{size} bytes is over this receiver's limit of {limit}");
    }
    Ok(())
}

/// The receiver's answer line, newline included: `ok` or `error <why>`
/// (`why` on one line).
pub fn encode_reply(result: &Result<(), String>) -> String {
    match result {
        Ok(()) => "ok\n".into(),
        Err(why) => format!("error {}\n", why.replace(['\n', '\r'], " ")),
    }
}

/// An answer line: `Ok` for `ok`, the receiver's why for `error <why>`.
pub fn parse_reply(line: &[u8]) -> Result<(), String> {
    let line = String::from_utf8_lossy(line);
    let line = line.trim_end_matches(['\n', '\r']);
    match line {
        "ok" => Ok(()),
        _ => match line.strip_prefix("error ") {
            Some(why) => Err(why.to_string()),
            None if line.is_empty() => Err("the receiver closed without an answer".into()),
            None => Err(format!("unexpected answer `{line}`")),
        },
    }
}

/// The size and sha256 (lowercase hex) of everything `body` reads: what a
/// sender puts in its [`Header`].
pub fn digest(mut body: impl Read) -> io::Result<(u64, String)> {
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let n = body.read(&mut buf)?;
        if n == 0 {
            return Ok((size, hex(&hash.finalize())));
        }
        hash.update(&buf[..n]);
        size += n as u64;
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A stream whose write half can be shut alone (the sender's end-of-body).
pub trait HalfClose {
    fn close_write(&self) -> io::Result<()>;
}

impl HalfClose for UnixStream {
    fn close_write(&self) -> io::Result<()> {
        self.shutdown(Shutdown::Write)
    }
}

impl HalfClose for Conn {
    fn close_write(&self) -> io::Result<()> {
        self.shutdown(Shutdown::Write)
    }
}

/// Send `header`, wait for the receiver's `ready`, send everything `body`
/// reads, half-close, and wait for its answer: `Ok` on `ok`, its why as the
/// error otherwise (`transfer refused: <why>`; a refused header sends no
/// body). `body` is sent as is: one that doesn't match `header` is the
/// receiver's to refuse.
pub fn send(mut stream: impl Read + Write + HalfClose, header: &Header, body: impl Read) -> Result<()> {
    stream.write_all(encode_header(header).as_bytes()).and_then(|()| stream.flush()).context("cannot send the transfer")?;
    let mut line = Vec::new();
    read_reply(&mut stream, &mut line).context("no answer to the transfer header")?;
    if line != READY.as_bytes() {
        let why = match parse_reply(&line) {
            Ok(()) => "`ok` before the body".to_string(),
            Err(why) => why,
        };
        bail!("transfer refused: {why}");
    }
    let sent = (|| {
        io::copy(&mut { body }, &mut stream)?;
        stream.flush()?;
        stream.close_write()
    })();
    line.clear();
    let read = read_reply(&mut stream, &mut line);
    // An answer beats our write error: the receiver closed on us.
    if !line.is_empty() {
        parse_reply(&line).map_err(|why| anyhow!("transfer refused: {why}"))?;
        return sent.context("cannot send the transfer");
    }
    sent.context("cannot send the transfer")?;
    read.context("no answer to the transfer")?;
    bail!("no answer to the transfer: the receiver closed")
}

/// One answer line into `line`, a byte at a time (it's short, and the body
/// may follow it), at most `REPLY_MAX` bytes.
fn read_reply(stream: &mut impl Read, line: &mut Vec<u8>) -> io::Result<()> {
    let mut byte = [0u8];
    while (line.len() as u64) < REPLY_MAX && !line.ends_with(b"\n") {
        if stream.read(&mut byte)? == 0 {
            break;
        }
        line.push(byte[0]);
    }
    Ok(())
}

/// Receive one transfer on `stream`: read and check its header (its `size`
/// at most `limit`), ask `decide` where the body goes (its error refuses
/// the transfer with that why), answer `ready`, write the body to a temp
/// file next to there, check it ended at `size` with the header's sha256,
/// rename it into place, answer `ok`. Any failure answers `error <why>`
/// (the temp file removed) and is returned too: a refused header at once,
/// a bad body once the rest of it is read and dropped (at most what the
/// header said plus [`DRAIN_EXTRA`]), so the answer isn't lost to a
/// sender still writing.
pub fn receive(stream: impl Read + Write, limit: u64, decide: impl FnOnce(&Header) -> Result<PathBuf>) -> Result<Received> {
    let mut stream = BufReader::new(stream);
    let result = receive_body(&mut stream, limit, decide);
    let reply = encode_reply(&result.as_ref().map(|_| ()).map_err(|e| format!("{e:#}")));
    let out = stream.get_mut();
    let answered = out.write_all(reply.as_bytes()).and_then(|()| out.flush());
    let received = result?;
    answered.context("cannot answer the transfer")?;
    Ok(received)
}

fn receive_body(
    stream: &mut BufReader<impl Read + Write>,
    limit: u64,
    decide: impl FnOnce(&Header) -> Result<PathBuf>,
) -> Result<Received> {
    let mut line = Vec::new();
    stream.take(HEADER_MAX).read_until(b'\n', &mut line).context("cannot read the transfer header")?;
    if !line.ends_with(b"\n") {
        bail!(if line.len() as u64 >= HEADER_MAX { "the transfer header is too long" } else { "no transfer header" });
    }
    let header = parse_header(&line)?;
    check_size(header.size, limit)?;
    let dest = decide(&header)?;
    let temp = Temp::create(&dest)?;
    let out = stream.get_mut();
    out.write_all(READY.as_bytes()).and_then(|()| out.flush()).context("cannot answer the transfer header")?;
    let mut got = 0u64;
    let body = read_body(stream, &header, &temp, &mut got);
    if body.is_err() {
        let rest = header.size.saturating_sub(got) + DRAIN_EXTRA;
        let _ = io::copy(&mut stream.take(rest), &mut io::sink());
    }
    body?;
    temp.place(&dest)?;
    Ok(Received { header, path: dest })
}

/// The body into `temp`, checked against `header`; `got` counts what was
/// read of it.
fn read_body(stream: &mut impl Read, header: &Header, temp: &Temp, got: &mut u64) -> Result<()> {
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut file = &temp.file;
    while *got < header.size {
        let want = buf.len().min((header.size - *got) as usize);
        let n = stream.read(&mut buf[..want]).context("cannot read the transfer body")?;
        if n == 0 {
            bail!("the body ended after {got} of {} bytes", header.size);
        }
        hash.update(&buf[..n]);
        *got += n as u64;
        file.write_all(&buf[..n]).with_context(|| format!("cannot write {}", temp.path.display()))?;
    }
    if stream.read(&mut buf[..1]).context("cannot read the transfer body")? > 0 {
        bail!("the body is longer than its {} bytes", header.size);
    }
    let sha = hex(&hash.finalize());
    if sha != header.sha256 {
        bail!("the body's sha256 is {sha}, its header says {}", header.sha256);
    }
    temp.file.sync_all().with_context(|| format!("cannot write {}", temp.path.display()))?;
    Ok(())
}

/// The temp file a body is written to, next to its destination; removed
/// on drop unless placed.
struct Temp {
    file: File,
    path: PathBuf,
    placed: bool,
}

impl Temp {
    fn create(dest: &Path) -> Result<Temp> {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let dir = dest.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
        let name = dest.file_name().with_context(|| format!("transfer destination {} has no file name", dest.display()))?;
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        let path = dir.join(format!(
            ".{}.{}-{}.part",
            name.to_string_lossy(),
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let file = File::options()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("cannot create {}", path.display()))?;
        Ok(Temp { file, path, placed: false })
    }

    fn place(mut self, dest: &Path) -> Result<()> {
        std::fs::rename(&self.path, dest).with_context(|| format!("cannot move the transfer to {}", dest.display()))?;
        self.placed = true;
        Ok(())
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        if !self.placed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Where one kind's transfers go and what happens once one is in.
type Decide = Box<dyn Fn(&Header) -> Result<PathBuf> + Send + Sync>;
type Done = Box<dyn Fn(Received) + Send + Sync>;
type Failed = Box<dyn Fn(&str) + Send + Sync>;

struct Kind {
    limit: u64,
    decide: Decide,
    done: Done,
    failed: Option<Failed>,
}

/// An end's receivers by kind: each `TRANSFER` stream the peer opens goes
/// to the one its header names; any other kind is refused.
pub struct Receivers {
    kinds: HashMap<String, Kind>,
    log: fn(&str),
}

impl Receivers {
    /// No kinds yet: every transfer is refused. `log` hears every transfer
    /// that failed on this end.
    pub fn new(log: fn(&str)) -> Receivers {
        Receivers { kinds: HashMap::new(), log }
    }

    /// Receive `kind`, at most `limit` bytes, into where `decide` says;
    /// `done` runs with each one received (after its `ok` went out, on the
    /// transfer's thread).
    pub fn with(
        mut self,
        kind: &str,
        limit: u64,
        decide: impl Fn(&Header) -> Result<PathBuf> + Send + Sync + 'static,
        done: impl Fn(Received) + Send + Sync + 'static,
    ) -> Receivers {
        self.kinds.insert(kind.to_string(), Kind { limit, decide: Box::new(decide), done: Box::new(done), failed: None });
        self
    }

    /// For `kind` (registered by [`Receivers::with`]): `failed` runs with
    /// why when one of its transfers failed after its `decide` took it (a
    /// bad or cut body), on the transfer's thread.
    pub fn on_failure(mut self, kind: &str, failed: impl Fn(&str) + Send + Sync + 'static) -> Receivers {
        if let Some(k) = self.kinds.get_mut(kind) {
            k.failed = Some(Box::new(failed));
        }
        self
    }

    /// [`receive`] on `stream` by its header's kind, then that kind's `done`
    /// (or, failed once its `decide` took it, its `failed`).
    pub fn receive(&self, stream: impl Read + Write) -> Result<Received> {
        let mut kind = None;
        let received = receive(stream, u64::MAX, |h| {
            let k = self.kinds.get(&h.kind).with_context(|| format!("nothing here receives `{}` transfers", h.kind))?;
            check_size(h.size, k.limit)?;
            let dest = (k.decide)(h)?;
            kind = Some(k);
            Ok(dest)
        });
        match (received, kind) {
            (Ok(received), Some(k)) => {
                (k.done)(received.clone());
                Ok(received)
            }
            (Err(e), Some(Kind { failed: Some(failed), .. })) => {
                failed(&format!("{e:#}"));
                Err(e)
            }
            (received, _) => received,
        }
    }

    /// The link's end of one `TRANSFER` stream the peer opened: a socket
    /// pair whose other end [`receive`]s on its own thread (the link's
    /// callback runs on the mux's read thread and must be quick).
    pub fn accept(self: &Arc<Self>) -> Option<Conn> {
        let (ours, theirs) = UnixStream::pair().ok()?;
        let me = Arc::clone(self);
        std::thread::Builder::new()
            .name("fleet-transfer".into())
            .spawn(move || {
                if let Err(e) = me.receive(ours) {
                    (me.log)(&format!("fleet: transfer failed: {e:#}"));
                }
            })
            .ok()?;
        Some(theirs.into())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::mpsc;

    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("devsandbox-transfer-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn header_for(kind: &str, name: &str, body: &[u8]) -> Header {
        let (size, sha256) = digest(body).unwrap();
        Header { kind: kind.into(), name: name.into(), size, sha256 }
    }

    fn files(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }

    /// `send` of `header` + `body` against `receive(limit)` into `dir`, over
    /// a socket pair: (the sender's result, the receiver's).
    fn transfer(dir: &Path, header: &Header, body: &[u8], limit: u64) -> (Result<()>, Result<Received>) {
        let (a, b) = UnixStream::pair().unwrap();
        let dir = dir.to_path_buf();
        let receiver = std::thread::spawn(move || receive(b, limit, |h| Ok(dir.join(&h.name))));
        let sent = send(a, header, body);
        (sent, receiver.join().unwrap())
    }

    #[test]
    fn the_header_round_trips_and_is_checked() {
        let h = header_for("root-tar", "cfg.tar", b"abc");
        let line = encode_header(&h);
        assert_eq!(
            line,
            "{\"kind\":\"root-tar\",\"name\":\"cfg.tar\",\"size\":3,\
             \"sha256\":\"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad\"}\n"
        );
        assert_eq!(parse_header(line.as_bytes()).unwrap(), h);
        assert_eq!(parse_header(line.trim_end().as_bytes()).unwrap(), h);
        let bad = |f: fn(&mut Header)| {
            let mut h = h.clone();
            f(&mut h);
            format!("{:#}", parse_header(encode_header(&h).as_bytes()).unwrap_err())
        };
        assert!(bad(|h| h.kind = "Bin".into()).contains("kind"));
        assert!(bad(|h| h.kind = String::new()).contains("kind"));
        assert!(bad(|h| h.name = "../x".into()).contains("name"));
        assert!(bad(|h| h.name = "..".into()).contains("name"));
        assert!(bad(|h| h.name = String::new()).contains("name"));
        assert!(bad(|h| h.sha256 = "AB".repeat(32)).contains("sha256"));
        assert!(bad(|h| h.sha256 = "ab".into()).contains("sha256"));
        assert!(parse_header(b"{\"kind\":\"x\"}").is_err());
        assert!(parse_header(b"not json").is_err());
        assert!(check_size(10, 10).is_ok());
        assert!(format!("{:#}", check_size(11, 10).unwrap_err()).contains("over this receiver's limit of 10"));
    }

    #[test]
    fn the_reply_round_trips() {
        assert_eq!(encode_reply(&Ok(())), "ok\n");
        assert_eq!(encode_reply(&Err("two\nlines".into())), "error two lines\n");
        assert_eq!(parse_reply(b"ok\n"), Ok(()));
        assert_eq!(parse_reply(b"ok\r\n"), Ok(()));
        assert_eq!(parse_reply(b"error nope\n"), Err("nope".into()));
        assert_eq!(parse_reply(b""), Err("the receiver closed without an answer".into()));
        assert_eq!(parse_reply(b"huh\n"), Err("unexpected answer `huh`".into()));
    }

    #[test]
    fn a_transfer_lands_at_its_destination() {
        let dir = scratch("ok");
        let body = vec![7u8; 300 * 1024];
        let h = header_for("binary", "devsandbox", &body);
        let (sent, got) = transfer(&dir, &h, &body, 1 << 20);
        sent.unwrap();
        let got = got.unwrap();
        assert_eq!(got, Received { header: h, path: dir.join("devsandbox") });
        assert_eq!(std::fs::read(&got.path).unwrap(), body);
        assert_eq!(files(&dir), ["devsandbox"], "no temp file left");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_body_is_a_transfer_too() {
        let dir = scratch("empty");
        let h = header_for("binary", "e", b"");
        let (sent, got) = transfer(&dir, &h, b"", 0);
        sent.unwrap();
        assert_eq!(std::fs::read(got.unwrap().path).unwrap(), b"");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Each refusal: the sender's error carries the receiver's why, the
    /// receiver's error is the same, nothing is left in `dir`.
    fn refused(tag: &str, header: &Header, body: &[u8], limit: u64, why: &str) {
        let dir = scratch(tag);
        let (sent, got) = transfer(&dir, header, body, limit);
        let sent = format!("{:#}", sent.unwrap_err());
        assert!(sent.contains("transfer refused") && sent.contains(why), "{tag}: {sent}");
        let got = format!("{:#}", got.unwrap_err());
        assert!(got.contains(why), "{tag}: {got}");
        assert_eq!(files(&dir), Vec::<String>::new(), "{tag}: nothing left");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bad_body_is_refused_and_removed() {
        let body = b"0123456789".to_vec();
        let h = header_for("binary", "b", &body);
        refused("short", &h, &body[..4], 100, "the body ended after 4 of 10 bytes");
        refused("long", &h, b"0123456789abc", 100, "the body is longer than its 10 bytes");
        refused("hash", &h, b"0123456780", 100, "its header says");
        refused("limit", &h, &body, 9, "10 bytes is over this receiver's limit of 9");
        // Refused at the header: the body (more than a socket buffer) is never sent.
        let big = vec![1u8; 4 << 20];
        refused("limit-big", &header_for("binary", "b", &big), &big, 1024, "over this receiver's limit of 1024");
    }

    #[test]
    fn the_destination_is_the_receivers_choice() {
        let dir = scratch("decide");
        let h = header_for("binary", "b", b"x");
        let (a, b) = UnixStream::pair().unwrap();
        let receiver = std::thread::spawn(move || receive(b, 10, |_| bail!("not now")));
        struct Untouched;
        impl Read for Untouched {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                panic!("a refused header's body is read")
            }
        }
        let err = format!("{:#}", send(a, &h, Untouched).unwrap_err());
        assert_eq!(err, "transfer refused: not now");
        assert!(receiver.join().unwrap().is_err());
        // A destination in a dir that doesn't exist yet: created.
        let deep = dir.join("a/b/c");
        let (a, b) = UnixStream::pair().unwrap();
        let to = deep.clone();
        let receiver = std::thread::spawn(move || receive(b, 10, move |_| Ok(to)));
        send(a, &h, &b"x"[..]).unwrap();
        assert_eq!(receiver.join().unwrap().unwrap().path, deep);
        assert_eq!(std::fs::read(&deep).unwrap(), b"x");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn receivers_pick_by_kind_and_refuse_the_rest() {
        let dir = scratch("kinds");
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let to = dir.clone();
        let receivers = Arc::new(Receivers::new(|_| {}).with(
            "bundle",
            100,
            move |h| Ok(to.join(&h.name)),
            move |r| tx.lock().unwrap().send(r).unwrap(),
        ));

        let ok = header_for("bundle", "repo.bundle", b"git");
        send(receivers.accept().unwrap(), &ok, &b"git"[..]).unwrap();
        let got = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert_eq!(got.path, dir.join("repo.bundle"));

        let (a, b) = UnixStream::pair().unwrap();
        let theirs = std::thread::spawn({
            let receivers = Arc::clone(&receivers);
            move || receivers.receive(b)
        });
        let err = format!("{:#}", send(a, &header_for("binary", "x", b"x"), &b"x"[..]).unwrap_err());
        assert_eq!(err, "transfer refused: nothing here receives `binary` transfers");
        assert!(theirs.join().unwrap().is_err());

        let (a, b) = UnixStream::pair().unwrap();
        let theirs = std::thread::spawn(move || receivers.receive(b));
        let big = vec![0u8; 101];
        let err = format!("{:#}", send(a, &header_for("bundle", "y", &big), &big[..]).unwrap_err());
        assert!(err.contains("over this receiver's limit of 100"), "{err}");
        assert!(theirs.join().unwrap().is_err());
        assert!(rx.try_recv().is_err(), "`done` only for the one received");
        assert_eq!(files(&dir), ["repo.bundle"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_kinds_failure_hook_hears_a_taken_transfer_fail() {
        let dir = scratch("failed");
        let failures = Arc::new(Mutex::new(Vec::<String>::new()));
        let heard = Arc::clone(&failures);
        let to = dir.clone();
        let receivers = Arc::new(
            Receivers::new(|_| {})
                .with("binary", 100, move |h| if h.name == "no" { bail!("not now") } else { Ok(to.join(&h.name)) }, |_| {})
                .on_failure("binary", move |why| heard.lock().unwrap().push(why.to_string())),
        );
        // Refused by `decide`: not taken, not a failure.
        assert!(send(receivers.accept().unwrap(), &header_for("binary", "no", b"x"), &b"x"[..]).is_err());
        // Taken, then a bad body.
        let h = header_for("binary", "b", b"xyz");
        assert!(send(receivers.accept().unwrap(), &h, &b"xy"[..]).is_err());
        let start = std::time::Instant::now();
        while failures.lock().unwrap().is_empty() {
            assert!(start.elapsed() < std::time::Duration::from_secs(5), "the hook ran");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(*failures.lock().unwrap(), ["the body ended after 2 of 3 bytes"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
