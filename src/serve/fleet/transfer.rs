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
//! nobody receives is an `error`, the temp file removed. A kind may check
//! or unpack the placed body before the answer ([`receive_then`],
//! [`Receivers::then`]): its error is the answer too.
//!
//! A **pull** goes the other way on the same channel: the opener writes a
//! [`PullRequest`] line (`{"pull": "<kind>", "name": "<file>"}`, never a
//! valid [`Header`]: it has no `kind`); the serving end answers a
//! [`Header`] line then exactly `size` raw bytes, or `error <why>` (and
//! nothing else); the opener writes the body into a temp file next to its
//! destination, checks its size (at most its limit) and sha256, renames it
//! into place and answers one line, `ok` or `error <why>`; then both close.
//! The serving end keeps its stream open until that answer, so the opener
//! never has to read past the body (no half-close needed), and only a
//! pull answered `ok` counts as sent ([`Receivers::serves`]).
//!
//! [`send`] and [`receive`] are the two ends over any byte stream, as are
//! [`pull`] and [`Receivers::receive`]; [`Receivers`] is an end's registry
//! of what it accepts and serves, by kind, which the link hands each
//! `TRANSFER` stream the peer opens (`link::opener_with`,
//! `link::answerer_with`).

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

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
    /// Where below the receiver's tree the body belongs, for a kind that
    /// places more than its file (a `bundle`'s checkout): a relative
    /// `/`-separated path of plain components. Left out of the line when
    /// unset; each receiver checks what it means.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// A `bundle`'s repo identity (16 lowercase hex,
    /// `code::repo::identity`): which cache it goes into. Left out unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// A `bundle`'s refs, full name → commit id: what the receiver's cache
    /// sets once the body is in (at most [`HEADER_REFS_MAX`]). Left out
    /// unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refs: Option<BTreeMap<String, String>>,
}

/// The most refs a header names.
pub const HEADER_REFS_MAX: usize = 8;

/// How long one read or write on a stream [`Receivers::accept`] hands out
/// may block: a body flows (the link's flow control), a sender waits on a
/// landing it doesn't read meanwhile, a puller checks a body before it
/// answers; this long means the other end is stuck.
pub const STREAM_WAIT: Duration = Duration::from_secs(5 * 60);

/// A pull's opening line: what the opener wants from the serving end.
/// `deny_unknown_fields`, and a [`Header`] needs `kind`: neither parses as
/// the other.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PullRequest {
    /// Which provider ([`Receivers::serves`]): `[a-z0-9-]`, at most 32 bytes.
    pub pull: String,
    /// The file the provider made for this opener (a plain name, as a
    /// header's).
    pub name: String,
}

/// A pull request line (the newline optional), checked; `None` when the
/// line isn't one (a header).
fn parse_pull(line: &[u8]) -> Option<Result<PullRequest>> {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let req: PullRequest = serde_json::from_slice(line).ok()?;
    Some(check_kind(&req.pull).and_then(|()| check_name(&req.name)).map(|()| req))
}

fn check_kind(kind: &str) -> Result<()> {
    if kind.is_empty() || kind.len() > 32 || !kind.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
        bail!("bad transfer kind `{kind}`");
    }
    Ok(())
}

fn check_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 255 || name == "." || name == ".." || name.contains(['/', '\0', '\n']) {
        bail!("bad transfer name {name:?}");
    }
    Ok(())
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
    check_kind(&h.kind)?;
    check_name(&h.name)?;
    if h.sha256.len() != 64 || !h.sha256.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        bail!("bad transfer sha256 `{}`", h.sha256);
    }
    if let Some(to) = &h.to {
        let plain = |c: &str| !c.is_empty() && c != "." && c != ".." && !c.contains(['\0', '\n', '\\']);
        if to.len() > 1024 || !to.split('/').all(plain) {
            bail!("bad transfer path {to:?}");
        }
    }
    if let Some(repo) = &h.repo {
        super::code::repo::check_id(repo).context("bad transfer repo")?;
    }
    if let Some(refs) = &h.refs {
        if refs.len() > HEADER_REFS_MAX {
            bail!("a transfer names at most {HEADER_REFS_MAX} refs, not {}", refs.len());
        }
        for (name, id) in refs {
            super::code::repo::check_ref(name).context("bad transfer ref")?;
            super::code::repo::check_oid(id).context("bad transfer ref")?;
        }
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
/// may follow it), at most `REPLY_MAX` bytes. An `EINTR` is read again: a
/// socket read with a timeout fails with it even under `SA_RESTART`
/// (signal(7)) whenever a handler runs, and the wait for a landing's
/// answer is long.
fn read_reply(stream: &mut impl Read, line: &mut Vec<u8>) -> io::Result<()> {
    let mut byte = [0u8];
    while (line.len() as u64) < REPLY_MAX && !line.ends_with(b"\n") {
        match stream.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => line.push(byte[0]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Pull `name` of kind `kind` on `stream` into `dest` (the opener's end):
/// the [`PullRequest`] line, then the serving end's [`Header`] (its kind
/// and name the asked ones, its `size` at most `limit`) and exactly `size`
/// bytes into a temp file next to `dest`, checked against its sha256 and
/// renamed to `dest`; then this end's answer, `ok` or `error <why>` (so the
/// serving end knows it may drop its copy). The size pulled, or why not
/// (`pull refused: <why>` for the serving end's `error`; nothing left at
/// `dest` or beside it). Every read and write as bounded as `stream` is.
pub fn pull(stream: impl Read + Write, kind: &str, name: &str, limit: u64, dest: &Path) -> Result<u64> {
    let req = PullRequest { pull: kind.to_string(), name: name.to_string() };
    check_kind(kind)?;
    check_name(name)?;
    let mut stream = BufReader::new(stream);
    let mut line = serde_json::to_vec(&req).expect("a pull request serializes");
    line.push(b'\n');
    let out = stream.get_mut();
    out.write_all(&line).and_then(|()| out.flush()).context("cannot ask for the pull")?;
    let line = first_line(&mut stream).context("no answer to the pull")?;
    if line.starts_with(b"error ") || line == b"ok\n" {
        let why = parse_reply(&line).err().unwrap_or_else(|| "`ok` before a header".into());
        bail!("pull refused: {why}");
    }
    let got = (|| {
        let header = parse_header(&line)?;
        if header.kind != kind || header.name != name {
            bail!("asked for the `{kind}` {name}, the answer is the `{}` {}", header.kind, header.name);
        }
        check_size(header.size, limit)?;
        let temp = Temp::create(dest)?;
        let mut buf = vec![0u8; 64 * 1024];
        let hash = read_exact_into(&mut stream, header.size, &temp, &mut 0, &mut buf)?;
        check_body(hash, &header, &temp)?;
        temp.place(dest)?;
        Ok(header.size)
    })();
    let answered = answer(stream.get_mut(), &got.as_ref().map(|_| ()).map_err(|e| format!("{e:#}")));
    let size = got?;
    if let Err(e) = answered {
        let _ = std::fs::remove_file(dest);
        return Err(anyhow!(e).context("cannot answer the pull"));
    }
    Ok(size)
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
#[cfg_attr(not(test), allow(dead_code))] // every kind goes through `Receivers`
pub fn receive(stream: impl Read + Write, limit: u64, decide: impl FnOnce(&Header) -> Result<PathBuf>) -> Result<Received> {
    receive_then(stream, limit, decide, |_| Ok(()))
}

/// [`receive`], with `then` run on what was received (in place, checked)
/// before the answer: its error is the answer (`error <why>`), the file
/// removed. So a receiver that unpacks the body reports the unpacking.
pub fn receive_then(
    stream: impl Read + Write,
    limit: u64,
    decide: impl FnOnce(&Header) -> Result<PathBuf>,
    then: impl FnOnce(&Received) -> Result<()>,
) -> Result<Received> {
    let mut stream = BufReader::new(stream);
    let body = first_line(&mut stream).and_then(|line| receive_body(&mut stream, &line, limit, decide));
    finish(&mut stream, body, then)
}

/// A received body through `then` (removed if it fails), then the answer.
fn finish(
    stream: &mut BufReader<impl Read + Write>,
    body: Result<Received>,
    then: impl FnOnce(&Received) -> Result<()>,
) -> Result<Received> {
    let result = body.and_then(|received| match then(&received) {
        Ok(()) => Ok(received),
        Err(e) => {
            let _ = std::fs::remove_file(&received.path);
            Err(e)
        }
    });
    let answered = answer(stream.get_mut(), &result.as_ref().map(|_| ()).map_err(|e| format!("{e:#}")));
    let received = result?;
    answered.context("cannot answer the transfer")?;
    Ok(received)
}

fn answer(out: &mut impl Write, result: &Result<(), String>) -> io::Result<()> {
    out.write_all(encode_reply(result).as_bytes()).and_then(|()| out.flush())
}

/// A stream's first line (a header or a pull request), at most
/// [`HEADER_MAX`] bytes, newline included.
fn first_line(stream: &mut BufReader<impl Read>) -> Result<Vec<u8>> {
    let mut line = Vec::new();
    stream.take(HEADER_MAX).read_until(b'\n', &mut line).context("cannot read the transfer header")?;
    if !line.ends_with(b"\n") {
        bail!(if line.len() as u64 >= HEADER_MAX { "the transfer header is too long" } else { "no transfer header" });
    }
    Ok(line)
}

fn receive_body(
    stream: &mut BufReader<impl Read + Write>,
    line: &[u8],
    limit: u64,
    decide: impl FnOnce(&Header) -> Result<PathBuf>,
) -> Result<Received> {
    let header = parse_header(line)?;
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
    let mut buf = vec![0u8; 64 * 1024];
    let hash = read_exact_into(stream, header.size, temp, got, &mut buf)?;
    if stream.read(&mut buf[..1]).context("cannot read the transfer body")? > 0 {
        bail!("the body is longer than its {} bytes", header.size);
    }
    check_body(hash, header, temp)
}

/// Exactly `size` bytes of `stream` into `temp`, hashed; `got` counts them.
fn read_exact_into(stream: &mut impl Read, size: u64, temp: &Temp, got: &mut u64, buf: &mut [u8]) -> Result<Sha256> {
    let mut hash = Sha256::new();
    let mut file = &temp.file;
    while *got < size {
        let want = buf.len().min((size - *got) as usize);
        let n = match stream.read(&mut buf[..want]) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(anyhow!(e).context("cannot read the transfer body")),
        };
        if n == 0 {
            bail!("the body ended after {got} of {size} bytes");
        }
        hash.update(&buf[..n]);
        *got += n as u64;
        file.write_all(&buf[..n]).with_context(|| format!("cannot write {}", temp.path.display()))?;
    }
    Ok(hash)
}

/// A body read whole into `temp`: its sha256 against `header`'s, synced.
fn check_body(hash: Sha256, header: &Header, temp: &Temp) -> Result<()> {
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
type Then = Box<dyn Fn(&Received) -> Result<()> + Send + Sync>;

struct Kind {
    limit: u64,
    decide: Decide,
    then: Option<Then>,
    done: Done,
    failed: Option<Failed>,
}

type Resolve = Box<dyn Fn(&str) -> Result<PathBuf> + Send + Sync>;
type Sent = Box<dyn Fn(&Path) + Send + Sync>;

/// What one kind of pull serves.
struct Provider {
    resolve: Resolve,
    sent: Sent,
}

/// An end's receivers and providers by kind: each `TRANSFER` stream the
/// peer opens goes to the receiver its header names, or the provider its
/// pull request names; any other kind is refused.
pub struct Receivers {
    kinds: HashMap<String, Kind>,
    providers: HashMap<String, Provider>,
    log: fn(&str),
}

impl Receivers {
    /// No kinds yet: every transfer is refused. `log` hears every transfer
    /// that failed on this end.
    pub fn new(log: fn(&str)) -> Receivers {
        Receivers { kinds: HashMap::new(), providers: HashMap::new(), log }
    }

    /// Serve pulls of `kind`: `resolve` maps an asked name to the file
    /// served (its error refuses the pull with that why); the file must be
    /// a regular one, opened without following a symlink. `sent` runs with
    /// its path once the opener answered `ok` (on the stream's thread).
    pub fn serves(
        mut self,
        kind: &str,
        resolve: impl Fn(&str) -> Result<PathBuf> + Send + Sync + 'static,
        sent: impl Fn(&Path) + Send + Sync + 'static,
    ) -> Receivers {
        self.providers.insert(kind.to_string(), Provider { resolve: Box::new(resolve), sent: Box::new(sent) });
        self
    }

    /// The log fn [`Receivers::new`] took, for a kind's own lines.
    pub fn log(&self) -> fn(&str) {
        self.log
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
        self.kinds.insert(kind.to_string(), Kind { limit, decide: Box::new(decide), then: None, done: Box::new(done), failed: None });
        self
    }

    /// For `kind` (registered by [`Receivers::with`]): `then` runs with each
    /// one received before its answer goes out ([`receive_then`]): its error
    /// is the answer, and the kind's `failed`.
    pub fn then(mut self, kind: &str, then: impl Fn(&Received) -> Result<()> + Send + Sync + 'static) -> Receivers {
        if let Some(k) = self.kinds.get_mut(kind) {
            k.then = Some(Box::new(then));
        }
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

    /// One `TRANSFER` stream by its first line: a pull request served by
    /// its kind's provider (`None`, [`Receivers::serve`]), else
    /// [`receive_then`] by its header's kind, then that kind's `done` (or,
    /// failed once its `decide` took it, its `failed`).
    pub fn receive(&self, stream: impl Read + Write) -> Result<Option<Received>> {
        let mut stream = BufReader::new(stream);
        let line = match first_line(&mut stream) {
            Ok(line) => line,
            Err(e) => {
                let _ = answer(stream.get_mut(), &Err(format!("{e:#}")));
                return Err(e);
            }
        };
        if let Some(req) = parse_pull(&line) {
            return self.serve(&mut stream, req).map(|()| None);
        }
        let mut kind = None;
        let body = receive_body(&mut stream, &line, u64::MAX, |h| {
            let k = self.kinds.get(&h.kind).with_context(|| format!("nothing here receives `{}` transfers", h.kind))?;
            check_size(h.size, k.limit)?;
            let dest = (k.decide)(h)?;
            kind = Some(k);
            Ok(dest)
        });
        let received = finish(&mut stream, body, |r| match self.kinds.get(&r.header.kind).and_then(|k| k.then.as_ref()) {
            Some(then) => then(r),
            None => Ok(()),
        });
        match (received, kind) {
            (Ok(received), Some(k)) => {
                (k.done)(received.clone());
                Ok(Some(received))
            }
            (Err(e), Some(Kind { failed: Some(failed), .. })) => {
                failed(&format!("{e:#}"));
                Err(e)
            }
            (received, _) => received.map(Some),
        }
    }

    /// One pull (the serving end of [`pull`]): `req`'s provider resolves
    /// the file, opened `O_NOFOLLOW` and checked to be a regular file;
    /// its header and body go out, then the opener's answer is read: `ok`
    /// runs the provider's `sent`. A refusal is answered `error <why>`.
    fn serve(&self, stream: &mut BufReader<impl Read + Write>, req: Result<PullRequest>) -> Result<()> {
        let opened = req.and_then(|req| {
            let p = self.providers.get(&req.pull).with_context(|| format!("nothing here serves `{}` pulls", req.pull))?;
            let path = (p.resolve)(&req.name)?;
            let mut file = File::options()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&path)
                .with_context(|| format!("no {} to pull here", req.name))?;
            if !file.metadata().with_context(|| format!("cannot read {}", path.display()))?.is_file() {
                bail!("{} isn't a file", req.name);
            }
            let (size, sha256) = digest(&mut file).with_context(|| format!("cannot read {}", path.display()))?;
            io::Seek::rewind(&mut file).with_context(|| format!("cannot read {}", path.display()))?;
            let header = Header { kind: req.pull.clone(), name: req.name.clone(), size, sha256, to: None, repo: None, refs: None };
            Ok((p, path, file, header))
        });
        let (p, path, file, header) = match opened {
            Ok(o) => o,
            Err(e) => {
                let _ = answer(stream.get_mut(), &Err(format!("{e:#}")));
                return Err(e);
            }
        };
        let out = stream.get_mut();
        let sent = (|| {
            out.write_all(encode_header(&header).as_bytes())?;
            let copied = io::copy(&mut file.take(header.size), out)?;
            if copied != header.size {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, format!("{} shrank while sent", path.display())));
            }
            out.flush()
        })();
        sent.with_context(|| format!("cannot send {}", header.name))?;
        let mut line = Vec::new();
        read_reply(stream, &mut line).context("no answer to the pull")?;
        parse_reply(&line).map_err(|why| anyhow!("the pull of {} failed there: {why}", header.name))?;
        (p.sent)(&path);
        Ok(())
    }

    /// The link's end of one `TRANSFER` stream the peer opened: a socket
    /// pair whose other end [`Receivers::receive`]s on its own thread (the
    /// link's callback runs on the mux's read thread and must be quick),
    /// each read and write on it bounded by [`STREAM_WAIT`].
    pub fn accept(self: &Arc<Self>) -> Option<Conn> {
        let (ours, theirs) = UnixStream::pair().ok()?;
        ours.set_read_timeout(Some(STREAM_WAIT)).ok()?;
        ours.set_write_timeout(Some(STREAM_WAIT)).ok()?;
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
        Header { kind: kind.into(), name: name.into(), size, sha256, to: None, repo: None, refs: None }
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
        for to in ["", "/a", "a/", "a//b", "a/../b", "./a", ".."] {
            let h = Header { to: Some(to.into()), ..h.clone() };
            let err = format!("{:#}", parse_header(encode_header(&h).as_bytes()).unwrap_err());
            assert!(err.contains("path"), "{to:?}: {err}");
        }
        let to = Header { to: Some("0123/repos/web".into()), ..h.clone() };
        let line = encode_header(&to);
        assert!(line.contains("\"to\":\"0123/repos/web\""), "{line}");
        assert_eq!(parse_header(line.as_bytes()).unwrap(), to);
        // A bundle's repo and refs.
        let oid = "a".repeat(40);
        let refs: BTreeMap<String, String> =
            [("refs/heads/main".to_string(), oid.clone()), ("refs/devsandbox/wip/main".to_string(), "b".repeat(64))].into();
        let bundle = Header { repo: Some("0123456789abcdef".into()), refs: Some(refs.clone()), ..to.clone() };
        let line = encode_header(&bundle);
        assert!(line.contains("\"repo\":\"0123456789abcdef\"") && line.contains(&format!("\"refs/heads/main\":\"{oid}\"")), "{line}");
        assert_eq!(parse_header(line.as_bytes()).unwrap(), bundle);
        for repo in ["", "0123", "0123456789ABCDEF", "../0123456789abc"] {
            let h = Header { repo: Some(repo.into()), ..bundle.clone() };
            let err = format!("{:#}", parse_header(encode_header(&h).as_bytes()).unwrap_err());
            assert!(err.contains("bad transfer repo"), "{repo:?}: {err}");
        }
        for (name, id) in [("main", oid.as_str()), ("refs/heads/-x", oid.as_str()), ("refs/heads/main", "HEAD"), ("refs/heads/main", "abc")] {
            let h = Header { refs: Some([(name.to_string(), id.to_string())].into()), ..bundle.clone() };
            let err = format!("{:#}", parse_header(encode_header(&h).as_bytes()).unwrap_err());
            assert!(err.contains("bad transfer ref"), "{name} {id}: {err}");
        }
        let many: BTreeMap<String, String> = (0..=HEADER_REFS_MAX).map(|i| (format!("refs/heads/b{i}"), oid.clone())).collect();
        let err = format!("{:#}", parse_header(encode_header(&Header { refs: Some(many), ..bundle }).as_bytes()).unwrap_err());
        assert!(err.contains("at most 8 refs"), "{err}");
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
    fn a_kinds_then_answers_for_the_received_body() {
        let dir = scratch("then");
        let failures = Arc::new(Mutex::new(Vec::<String>::new()));
        let heard = Arc::clone(&failures);
        let to = dir.clone();
        let receivers = Receivers::new(|_| {})
            .with("root-tar", 100, move |h| Ok(to.join(&h.name)), |_| {})
            .then("root-tar", |r| if std::fs::read(&r.path)? == b"bad" { bail!("cannot unpack") } else { Ok(()) })
            .on_failure("root-tar", move |why| heard.lock().unwrap().push(why.to_string()));
        let receivers = Arc::new(receivers);
        send(receivers.accept().unwrap(), &header_for("root-tar", "good", b"good"), &b"good"[..]).unwrap();
        let err = format!("{:#}", send(receivers.accept().unwrap(), &header_for("root-tar", "bad", b"bad"), &b"bad"[..]).unwrap_err());
        assert_eq!(err, "transfer refused: cannot unpack");
        assert_eq!(files(&dir), ["good"], "the refused body is removed");
        let start = std::time::Instant::now();
        while failures.lock().unwrap().is_empty() {
            assert!(start.elapsed() < std::time::Duration::from_secs(5), "the hook ran");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(*failures.lock().unwrap(), ["cannot unpack"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A socket pair, both ends' reads and writes bounded by `wait`.
    fn bounded_pair(wait: std::time::Duration) -> (UnixStream, UnixStream) {
        let (a, b) = UnixStream::pair().unwrap();
        for s in [&a, &b] {
            s.set_read_timeout(Some(wait)).unwrap();
            s.set_write_timeout(Some(wait)).unwrap();
        }
        (a, b)
    }

    /// Receivers serving `kind` `pull` out of `dir` (`resolve` joins the
    /// name), recording what was sent.
    fn providing(dir: &Path) -> (Arc<Receivers>, Arc<Mutex<Vec<PathBuf>>>) {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let heard = Arc::clone(&sent);
        let at = dir.to_path_buf();
        let r = Receivers::new(|_| {}).serves("pull", move |name| Ok(at.join(name)), move |p| heard.lock().unwrap().push(p.to_path_buf()));
        (Arc::new(r), sent)
    }

    #[test]
    fn a_pull_round_trips_and_is_counted_sent_once_answered() {
        let dir = scratch("pull");
        let (src, dest) = (dir.join("src"), dir.join("dest"));
        std::fs::create_dir_all(&src).unwrap();
        let body = vec![9u8; 200 * 1024];
        std::fs::write(src.join("b.bundle"), &body).unwrap();
        let (r, sent) = providing(&src);
        let (a, b) = bounded_pair(std::time::Duration::from_secs(30));
        let server = std::thread::spawn(move || r.receive(b));
        assert_eq!(pull(a, "pull", "b.bundle", 1 << 20, &dest.join("got")).unwrap(), body.len() as u64);
        assert_eq!(server.join().unwrap().unwrap(), None, "a pull, not a transfer");
        assert_eq!(std::fs::read(dest.join("got")).unwrap(), body);
        assert_eq!(files(&dest), ["got"], "no temp file left");
        assert_eq!(*sent.lock().unwrap(), [src.join("b.bundle")]);
        // A pull line never parses as a header, nor a header as a pull.
        assert!(parse_header(b"{\"pull\":\"pull\",\"name\":\"b\"}\n").is_err());
        assert!(parse_pull(encode_header(&header_for("pull", "b", b"")).as_bytes()).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_refused_pull_answers_error_and_writes_nothing() {
        let dir = scratch("pull-refused");
        let (src, dest) = (dir.join("src"), dir.join("dest"));
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dir.join("outside"), "secret").unwrap();
        std::os::unix::fs::symlink(dir.join("outside"), src.join("link")).unwrap();
        std::fs::create_dir_all(src.join("sub")).unwrap();
        let (r, sent) = providing(&src);
        // Through `pull`: what its server refuses.
        for (kind, name, why) in [("pull", "missing", "no missing to pull here"), ("pull", "link", "no link to pull here"), ("pull", "sub", "sub isn't a file"), ("other", "x", "nothing here serves `other` pulls")] {
            let (a, b) = bounded_pair(std::time::Duration::from_secs(30));
            let r = Arc::clone(&r);
            let server = std::thread::spawn(move || r.receive(b));
            let err = format!("{:#}", pull(a, kind, name, 100, &dest.join("got")).unwrap_err());
            assert!(err.contains("pull refused") && err.contains(why), "{name}: {err}");
            assert!(server.join().unwrap().is_err(), "{name}");
        }
        // Raw request lines `pull` itself never sends.
        for line in ["{\"pull\":\"pull\",\"name\":\"../outside\"}\n", "{\"pull\":\"pull\",\"name\":\"..\"}\n", "{\"pull\":\"pull\",\"name\":\"a/b\"}\n"] {
            let (mut a, b) = bounded_pair(std::time::Duration::from_secs(30));
            let r = Arc::clone(&r);
            let server = std::thread::spawn(move || r.receive(b));
            a.write_all(line.as_bytes()).unwrap();
            let mut reply = Vec::new();
            read_reply(&mut a, &mut reply).unwrap();
            assert!(reply.starts_with(b"error bad transfer name"), "{line}: {}", String::from_utf8_lossy(&reply));
            assert!(server.join().unwrap().is_err());
        }
        assert!(pull(bounded_pair(std::time::Duration::from_secs(1)).0, "pull", "a/b", 100, &dest.join("got")).is_err(), "refused before asking");
        assert_eq!(files(&dest), Vec::<String>::new());
        assert!(sent.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `pull` against a server writing `answer` raw, then reading the
    /// puller's reply: (the pull's error, that reply).
    fn pull_against(dir: &Path, answer: Vec<u8>, limit: u64) -> (String, String) {
        let (a, mut b) = bounded_pair(std::time::Duration::from_secs(30));
        let server = std::thread::spawn(move || {
            let mut reader = BufReader::new(b.try_clone().unwrap());
            let mut req = Vec::new();
            reader.read_until(b'\n', &mut req).unwrap();
            let _ = b.write_all(&answer);
            let mut reply = Vec::new();
            let _ = read_reply(&mut reader, &mut reply);
            String::from_utf8_lossy(&reply).into_owned()
        });
        let err = format!("{:#}", pull(a, "pull", "b", limit, &dir.join("got")).unwrap_err());
        (err, server.join().unwrap())
    }

    #[test]
    fn a_pull_checks_its_size_and_sha256() {
        let dir = scratch("pull-checks");
        let h = header_for("pull", "b", b"0123456789");
        let mut big = encode_header(&h).into_bytes();
        big.extend_from_slice(b"0123456789");
        let (err, reply) = pull_against(&dir, big, 9);
        assert!(err.contains("10 bytes is over this receiver's limit of 9"), "{err}");
        assert!(reply.starts_with("error ") && reply.contains("limit of 9"), "{reply}");
        let mut wrong = encode_header(&h).into_bytes();
        wrong.extend_from_slice(b"0123456780");
        let (err, reply) = pull_against(&dir, wrong, 100);
        assert!(err.contains("its header says"), "{err}");
        assert!(reply.contains("its header says"), "{reply}");
        let mut other = encode_header(&header_for("pull", "c", b"")).into_bytes();
        other.extend_from_slice(b"");
        let (err, _) = pull_against(&dir, other, 100);
        assert!(err.contains("the answer is the `pull` c"), "{err}");
        assert_eq!(files(&dir), Vec::<String>::new(), "nothing written");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stalled_pull_times_out() {
        let dir = scratch("pull-stall");
        let (a, b) = bounded_pair(std::time::Duration::from_millis(200));
        // A server that reads the request, sends half a body and stalls.
        let server = std::thread::spawn(move || {
            let mut b = b;
            let mut reader = BufReader::new(b.try_clone().unwrap());
            let mut req = Vec::new();
            reader.read_until(b'\n', &mut req).unwrap();
            b.write_all(encode_header(&header_for("pull", "b", b"0123456789")).as_bytes()).unwrap();
            b.write_all(b"01234").unwrap();
            std::thread::sleep(std::time::Duration::from_secs(1));
        });
        let start = std::time::Instant::now();
        let err = format!("{:#}", pull(a, "pull", "b", 100, &dir.join("got")).unwrap_err());
        assert!(start.elapsed() < std::time::Duration::from_secs(5), "bounded");
        assert!(err.contains("cannot read the transfer body"), "{err}");
        server.join().unwrap();
        // No answer at all.
        let (a, _b) = bounded_pair(std::time::Duration::from_millis(200));
        let err = format!("{:#}", pull(a, "pull", "b", 100, &dir.join("got")).unwrap_err());
        assert!(err.contains("no answer to the pull"), "{err}");
        assert_eq!(files(&dir), Vec::<String>::new());
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
