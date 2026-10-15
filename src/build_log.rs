//! Per-instance build logs (docs/instance-phase.md):
//! `<data>/devsandbox/logs/<instance_id>/build.log`, next to `state.toml`.
//! While `run`, `rebuild` or `start` works on an instance, everything written
//! to fds 1 and 2 (our own prints and every child inheriting stdio: docker
//! build, lifecycle `docker exec`s, git) goes both where it went before and
//! into the log, so the output of a build nobody watched (a detached run, a
//! failed one whose terminal is gone) can be read back with
//! `devsandbox logs --build`. Opening a log rotates the previous one to
//! `build.1.log`; the directory goes with the instance record (`rm`, a
//! rolled-back `run`).
//!
//! The tee is process-wide fd surgery, so it only happens when the CLI asked
//! for it ([`enable`], from `main` for the `run`/`rebuild`/`start` verbs):
//! `devsandbox serve` and the dashboard's autostart pass call these commands
//! in-process, where fds 1/2 belong to `serve.log` or the terminal and other
//! threads write there too. The dashboard's own `run`/`rebuild`/`start` are
//! `devsandbox` subprocesses, so they log.
//!
//! On a terminal the tee is a pty (docs/instance-phase.md, *Build logs*):
//! when fds 1 and 2 are both terminals, or when [`TTY_ENV`] gives a size
//! (the daemon sets it for a verb request's `tty`), fds 1/2 point at a pty's
//! slave sized like the terminal, so docker build and every other child see a
//! TTY and keep their colour and progress display. The pty is this process's
//! own, never the daemon's: a daemon exit (handoff, drain) must not hang it up
//! under a running verb. Its log says so in the header (`# tty <cols>x<rows>`),
//! the size a reader replays it at. Otherwise (a pipe, a file) the tee is a
//! pipe per fd and children see no terminal.
//!
//! Unix only: elsewhere [`open`] returns an inert log and nothing is written.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use crate::state::{Instance, State};

/// The log being written; the previous run's is [`PREVIOUS`].
pub const FILE: &str = "build.log";
/// The one rotated generation kept.
pub const PREVIOUS: &str = "build.1.log";

/// A line longer than this is logged without waiting for its newline, so a
/// child printing `\r`-only progress can't grow the buffer forever.
const MAX_PENDING: usize = 64 * 1024;

static ENABLED: AtomicBool = AtomicBool::new(false);

/// `<cols>x<rows>`: the pty size a verb process's build log tees through,
/// when its own fds 1/2 aren't terminals (a daemon's verb subprocess).
pub const TTY_ENV: &str = "DEVSANDBOX_TTY";

/// A terminal's size in cells: a build log's pty, and a verb request's
/// `tty` (docs/api.md), passed to the verb as [`TTY_ENV`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TtySize {
    pub cols: u16,
    pub rows: u16,
}

impl TtySize {
    /// The largest side taken: well past any real terminal, small enough
    /// that a reader replaying the log at this size stays cheap.
    pub const MAX: u16 = 1000;

    /// Both sides in `1..=MAX`.
    pub fn valid(self) -> bool {
        (1..=Self::MAX).contains(&self.cols) && (1..=Self::MAX).contains(&self.rows)
    }

    /// A terminal's reported size made usable: a side it doesn't report
    /// (0, some serial consoles) as 80x24 has it, a huge one capped.
    pub fn sane(cols: u16, rows: u16) -> TtySize {
        let side = |n: u16, default| if n == 0 { default } else { n.min(Self::MAX) };
        TtySize { cols: side(cols, 80), rows: side(rows, 24) }
    }

    /// `<cols>x<rows>`, as [`TTY_ENV`] and the header write it; `None` when
    /// malformed or not [`valid`](Self::valid).
    pub fn parse(s: &str) -> Option<TtySize> {
        let (cols, rows) = s.trim().split_once('x')?;
        let size = TtySize { cols: cols.parse().ok()?, rows: rows.parse().ok()? };
        size.valid().then_some(size)
    }
}

impl std::fmt::Display for TtySize {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}x{}", self.cols, self.rows)
    }
}

/// `fd`'s terminal size (`TIOCGWINSZ`, [`TtySize::sane`]); `None` when it
/// isn't a terminal.
#[cfg(unix)]
pub fn terminal_size(fd: i32) -> Option<TtySize> {
    // SAFETY: an all-zero winsize is valid; TIOCGWINSZ fills it in.
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: TIOCGWINSZ writes one winsize through the pointer.
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } < 0 {
        return None;
    }
    Some(TtySize::sane(ws.ws_col, ws.ws_row))
}

/// The pty a build log tees through, if any: the terminal's size when
/// stdout is one and stderr too (a CLI verb in a terminal), else
/// [`TTY_ENV`]'s. A stdout terminal with stderr redirected keeps the pipes:
/// merging the two onto one pty would send the errors to stdout.
#[cfg_attr(not(unix), allow(dead_code))]
fn pty_size(stdout: Option<TtySize>, stderr_is_terminal: bool, env: Option<&str>) -> Option<TtySize> {
    match stdout {
        Some(size) if stderr_is_terminal => Some(size),
        _ => env.and_then(TtySize::parse),
    }
}

/// Let [`open`] tee fds 1/2 for the rest of the process. Only for a CLI
/// process running one verb (see the module doc).
pub fn enable() {
    ENABLED.store(true, Ordering::SeqCst);
}

/// `logs/<instance_id>/` under the data dir holding `state.toml`.
pub fn dir(instance_id: &str) -> Result<PathBuf> {
    let state = State::path()?;
    let data = state.parent().unwrap_or(Path::new("."));
    Ok(dir_in(data, instance_id))
}

fn dir_in(data: &Path, instance_id: &str) -> PathBuf {
    data.join("logs").join(instance_id)
}

/// The current build log's path (which may not exist).
pub fn path(instance_id: &str) -> Result<PathBuf> {
    Ok(dir(instance_id)?.join(FILE))
}

/// Drop an instance's log dir along with its record. Best effort: a log
/// left behind is harmless, and the record's removal must not fail on it.
pub fn remove(instance_id: &str) {
    if let Ok(dir) = dir(instance_id) {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Bytes [`tail_in`] reads off the end of a log: plenty for its line count
/// unless lines are very long, without reading a big build's whole output
/// on the daemon's thread.
const TAIL_BYTES: u64 = 256 * 1024;

/// The end of a build log, for the dashboard's build-log modal (and the
/// daemon's `instances.buildLog` answer).
#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Tail {
    pub text: String,
    /// Earlier lines were left out.
    pub cut: bool,
}

/// The last `lines` lines of `instance_id`'s build log under data dir
/// `data` (the dir holding `state.toml`); `None` when it has none (an
/// instance from before build logs, or one never built since). The daemon
/// reads an injected state path, so its tests never touch the user's logs.
#[cfg_attr(not(unix), allow(dead_code))] // only the daemon (unix) uses it
pub fn tail_in(data: &Path, instance_id: &str, lines: usize) -> Result<Option<Tail>> {
    tail_at(&dir_in(data, instance_id).join(FILE), lines)
}

fn tail_at(path: &Path, lines: usize) -> Result<Option<Tail>> {
    let mut file = match File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("cannot open {}", path.display())),
    };
    let read = |file: &mut File| -> std::io::Result<(Vec<u8>, bool)> {
        let start = file.metadata()?.len().saturating_sub(TAIL_BYTES);
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok((bytes, start > 0))
    };
    let (bytes, partial) = read(&mut file).with_context(|| format!("cannot read {}", path.display()))?;
    Ok(Some(last_lines(&String::from_utf8_lossy(&bytes), lines, partial)))
}

/// The last `n` lines of `text`, each as a terminal would leave it (after
/// its last `\r`: a child's progress redraws). `partial`: `text` starts
/// mid-file, so its first line may be cut short and is dropped.
fn last_lines(text: &str, n: usize, partial: bool) -> Tail {
    let mut lines: Vec<&str> = text
        .lines()
        .map(|l| l.trim_end_matches('\r').rsplit('\r').next().unwrap_or(""))
        .collect();
    if partial && !lines.is_empty() {
        lines.remove(0);
    }
    let cut = partial || lines.len() > n;
    let skip = lines.len().saturating_sub(n);
    Tail { text: lines[skip..].join("\n"), cut }
}

/// Make `dir`, move an existing `build.log` to `build.1.log` (replacing the
/// older one), and create a fresh `build.log` starting with `header`.
fn create(dir: &Path, header: &str) -> std::io::Result<File> {
    std::fs::create_dir_all(dir)?;
    let current = dir.join(FILE);
    if current.exists() {
        std::fs::rename(&current, dir.join(PREVIOUS))?;
    }
    let mut file = File::create(current)?;
    file.write_all(header.as_bytes())?;
    Ok(file)
}

/// `$ devsandbox <args…>` (like the dispatch logs), then when, then the
/// pty's size when the output went through one (`# tty <cols>x<rows>`).
fn header(args: &[String], now: u64, tty: Option<TtySize>) -> String {
    let mut header = format!("$ devsandbox {}\n# {}\n", args.join(" "), crate::inbox::rfc3339(now));
    if let Some(size) = tty {
        header.push_str(&format!("# tty {size}\n"));
    }
    header
}

/// An instance's build log while it's open: fds 1/2 are teed into it until
/// [`BuildLog::outcome`] (or drop) puts them back. Inert when not enabled,
/// off unix, or when the log can't be created (warned).
pub struct BuildLog {
    #[cfg(unix)]
    tee: Option<imp::Tee>,
    file: Option<Arc<Mutex<File>>>,
}

/// Start `instance_id`'s build log. Output before this (a sandbox pick,
/// name resolution) isn't in it. One log at a time: a `--all` loop finishes
/// one instance's before opening the next.
pub fn open(instance_id: &str) -> BuildLog {
    if !cfg!(unix) || !ENABLED.load(Ordering::SeqCst) {
        return BuildLog::inert();
    }
    match dir(instance_id) {
        Ok(dir) => open_in(&dir, instance_id),
        Err(e) => {
            eprintln!("warning: no build log for `{instance_id}`: {e:#}");
            BuildLog::inert()
        }
    }
}

/// [`open`] in log dir `dir`.
fn open_in(dir: &Path, instance_id: &str) -> BuildLog {
    let inert = BuildLog::inert();
    #[cfg(unix)]
    let pty = imp::pty_size().and_then(|size| match imp::Pty::open(size) {
        Ok(pty) => Some(pty),
        Err(e) => {
            eprintln!("warning: build log of `{instance_id}`: no pty ({e}), so no terminal for its commands");
            None
        }
    });
    #[cfg(unix)]
    let tty = pty.as_ref().map(|p| p.size);
    #[cfg(not(unix))]
    let tty = None;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let file = match create(dir, &header(&args, Instance::now(), tty)) {
        Ok(file) => Arc::new(Mutex::new(file)),
        Err(e) => {
            eprintln!("warning: no build log for `{instance_id}`: {e:#}");
            return inert;
        }
    };
    #[cfg(unix)]
    {
        match imp::Tee::start(file.clone(), pty) {
            Ok(tee) => BuildLog { tee: Some(tee), file: Some(file) },
            Err(e) => {
                eprintln!("warning: no build log for `{instance_id}`: cannot tee output: {e}");
                inert
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = file;
        inert
    }
}

impl BuildLog {
    fn inert() -> BuildLog {
        BuildLog {
            #[cfg(unix)]
            tee: None,
            file: None,
        }
    }

    /// Close the log with the command's result: an error goes in as the last
    /// line (to the log only: the caller still reports it), so a log read
    /// later says how it ended, whoever printed the error.
    pub fn outcome<T>(mut self, result: Result<T>) -> Result<T> {
        #[cfg(unix)]
        drop(self.tee.take());
        if let (Err(e), Some(file)) = (&result, &self.file) {
            let mut file = file.lock().unwrap_or_else(|e| e.into_inner());
            let _ = writeln!(file, "error: {e:#}");
        }
        result
    }
}

/// Copy `src` to `term` as it comes (partial lines included, as a terminal
/// would show them) and to `log` in whole lines, so two streams sharing one
/// log interleave by line rather than mid-line. A failing `term` (closed
/// pipe) stops only that side: the command, and its log, go on.
#[cfg_attr(not(unix), allow(dead_code))]
fn copy(mut src: impl Read, mut term: impl Write, log: &Mutex<impl Write>) {
    let mut buf = [0u8; 8192];
    let mut pending = Vec::new();
    let mut term_ok = true;
    let flush = |bytes: &[u8]| {
        let mut log = log.lock().unwrap_or_else(|e| e.into_inner());
        let _ = log.write_all(bytes).and_then(|()| log.flush());
    };
    loop {
        let n = match src.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        let chunk = &buf[..n];
        if term_ok {
            term_ok = term.write_all(chunk).and_then(|()| term.flush()).is_ok();
        }
        pending.extend_from_slice(chunk);
        let end = match pending.iter().rposition(|&b| b == b'\n') {
            Some(i) => i + 1,
            None if pending.len() > MAX_PENDING => pending.len(),
            None => continue,
        };
        flush(&pending[..end]);
        pending.drain(..end);
    }
    if !pending.is_empty() {
        flush(&pending);
    }
}

#[cfg(unix)]
mod imp {
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;

    use super::TtySize;

    /// How often a reader checks whether it was told to stop.
    const POLL_MS: i32 = 100;

    /// fds 1/2 pointed at pipes (one copier thread each) or at one pty's
    /// slave (one copier on its master).
    pub(super) struct Tee {
        /// Each switched fd and what it pointed at before, put back on drop.
        saved: Vec<(RawFd, OwnedFd)>,
        threads: Vec<JoinHandle<()>>,
        stop: Arc<AtomicBool>,
    }

    fn check(ret: i32) -> std::io::Result<i32> {
        if ret < 0 { Err(std::io::Error::last_os_error()) } else { Ok(ret) }
    }

    /// `dup` with close-on-exec: children must only see fds 0–2.
    fn dup_cloexec(fd: RawFd) -> std::io::Result<OwnedFd> {
        // SAFETY: plain fcntl on an fd; a fresh fd on success is ours.
        let new = check(unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) })?;
        Ok(unsafe { OwnedFd::from_raw_fd(new) })
    }

    fn set_cloexec(fd: &OwnedFd) -> std::io::Result<()> {
        // SAFETY: fcntl on an owned, open fd.
        check(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) }).map(drop)
    }

    fn pipe_cloexec() -> std::io::Result<(OwnedFd, OwnedFd)> {
        let mut fds = [0; 2];
        // SAFETY: `fds` is a valid 2-element array for pipe(2).
        check(unsafe { libc::pipe(fds.as_mut_ptr()) })?;
        // SAFETY: both fds were just created and are owned here.
        let (r, w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        set_cloexec(&r)?;
        set_cloexec(&w)?;
        Ok((r, w))
    }

    fn flush_std() {
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
    }

    /// The pty to tee through, if any (see [`super::pty_size`]).
    pub(super) fn pty_size() -> Option<TtySize> {
        // SAFETY: isatty on a standard fd.
        let stderr_is_terminal = unsafe { libc::isatty(2) } == 1;
        let env = std::env::var(super::TTY_ENV).ok();
        super::pty_size(super::terminal_size(1), stderr_is_terminal, env.as_deref())
    }

    /// A fresh pty, both ends close-on-exec (the slave reaches children
    /// only as their fds 1/2). The termios is the default, `ONLCR` included:
    /// the master's bytes are what a terminal shows (`\r\n`), so they read
    /// right on the CLI's terminal and replay right in a terminal emulator.
    pub(super) struct Pty {
        pub(super) master: OwnedFd,
        pub(super) slave: OwnedFd,
        pub(super) size: TtySize,
    }

    impl Pty {
        pub(super) fn open(size: TtySize) -> std::io::Result<Pty> {
            let (mut master, mut slave) = (0, 0);
            let mut ws = libc::winsize { ws_row: size.rows, ws_col: size.cols, ws_xpixel: 0, ws_ypixel: 0 };
            // SAFETY: out-pointers to two ints; no name buffer, default
            // termios, `ws` read for the size.
            check(unsafe {
                libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), &mut ws)
            })?;
            // SAFETY: both fds were just opened and are owned here.
            let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
            set_cloexec(&master)?;
            set_cloexec(&slave)?;
            Ok(Pty { master, slave, size })
        }
    }

    impl Tee {
        /// Tee fds 1/2 into `log`: through `pty` when given, else pipes.
        pub(super) fn start(log: Arc<Mutex<File>>, pty: Option<Pty>) -> std::io::Result<Tee> {
            flush_std();
            // Built up one fd at a time: on an error, dropping what's there
            // puts the fds already switched back.
            let mut tee = Tee { saved: Vec::new(), threads: Vec::new(), stop: Arc::new(AtomicBool::new(false)) };
            match pty {
                None => {
                    for fd in [1, 2] {
                        let term = File::from(dup_cloexec(fd)?);
                        let (r, w) = pipe_cloexec()?;
                        // `w` closes below, leaving `fd` the only write end
                        // (EOF once it's restored).
                        tee.switch(fd, &w)?;
                        drop(w);
                        tee.copier(&format!("build-log-{fd}"), r, term, &log)?;
                    }
                }
                Some(pty) => {
                    // Both streams to the original stdout, as a terminal
                    // merges them.
                    let term = File::from(dup_cloexec(1)?);
                    for fd in [1, 2] {
                        tee.switch(fd, &pty.slave)?;
                    }
                    // fds 1/2 are the slave's only refs now: restoring them
                    // hangs it up, which ends the copier's reads.
                    drop(pty.slave);
                    tee.copier("build-log-pty", pty.master, term, &log)?;
                }
            }
            Ok(tee)
        }

        /// Point `fd` at `to`, saving what it pointed at.
        fn switch(&mut self, fd: RawFd, to: &OwnedFd) -> std::io::Result<()> {
            let saved = dup_cloexec(fd)?;
            // SAFETY: dup2 onto a standard stream; `saved` keeps the
            // original open.
            check(unsafe { libc::dup2(to.as_raw_fd(), fd) })?;
            self.saved.push((fd, saved));
            Ok(())
        }

        fn copier(&mut self, name: &str, src: OwnedFd, term: File, log: &Arc<Mutex<File>>) -> std::io::Result<()> {
            let src = StopRead { file: File::from(src), stop: self.stop.clone() };
            let log = log.clone();
            let thread = std::thread::Builder::new().name(name.into()).spawn(move || super::copy(src, term, &log))?;
            self.threads.push(thread);
            Ok(())
        }
    }

    impl Drop for Tee {
        /// fds back first (closing the pipes' last write ends, or the pty
        /// slave's last refs, so the copiers drain to EOF and flush), then
        /// join. A child that outlives the command holding a write end or
        /// the slave (nothing should; the daemon gets its own stdio) would
        /// block EOF forever: the stop flag ends the wait once the source
        /// stays empty for a poll interval.
        fn drop(&mut self) {
            flush_std();
            for (fd, saved) in &self.saved {
                // SAFETY: `saved` is an open fd owned here.
                unsafe { libc::dup2(saved.as_raw_fd(), *fd) };
            }
            self.stop.store(true, Ordering::SeqCst);
            for t in self.threads.drain(..) {
                let _ = t.join();
            }
        }
    }

    /// A pipe's read end, or a pty's master, that reports EOF once `stop` is
    /// set and nothing is left to read. EINTR (a Ctrl-C caught by the cancel
    /// guard) is retried: the rollback's output must still come through. A
    /// master whose slave is closed everywhere reads `EIO` on Linux (not 0):
    /// that's its EOF.
    pub(super) struct StopRead {
        pub(super) file: File,
        pub(super) stop: Arc<AtomicBool>,
    }

    impl Read for StopRead {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            loop {
                match readable(self.file.as_raw_fd(), POLL_MS) {
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                    Ok(false) if self.stop.load(Ordering::SeqCst) => return Ok(0),
                    Ok(false) => continue,
                    // Readable, or hung up (read returns what's left, then
                    // 0 or EIO).
                    Ok(true) => {}
                }
                return match self.file.read(buf) {
                    Err(e) if e.raw_os_error() == Some(libc::EIO) => Ok(0),
                    r => r,
                };
            }
        }
    }

    /// Wait up to `ms` for `fd` to be readable (or hung up).
    #[cfg(not(target_os = "macos"))]
    fn readable(fd: RawFd, ms: i32) -> std::io::Result<bool> {
        let mut p = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd.
        Ok(check(unsafe { libc::poll(&mut p, 1, ms) })? > 0)
    }

    /// macOS's poll(2) doesn't support devices, a pty master included:
    /// select(2) does.
    #[cfg(target_os = "macos")]
    fn readable(fd: RawFd, ms: i32) -> std::io::Result<bool> {
        if fd as usize >= libc::FD_SETSIZE as usize {
            return Ok(true); // can't wait on it: a plain blocking read
        }
        // SAFETY: a zeroed fd_set is empty; FD_SET with an fd below
        // FD_SETSIZE; select over that one set.
        unsafe {
            let mut set: libc::fd_set = std::mem::zeroed();
            libc::FD_ZERO(&mut set);
            libc::FD_SET(fd, &mut set);
            let mut tv = libc::timeval { tv_sec: 0, tv_usec: (ms * 1000) as _ };
            let n = libc::select(fd + 1, &mut set, std::ptr::null_mut(), std::ptr::null_mut(), &mut tv);
            Ok(check(n)? > 0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("devsandbox-build-log-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn dir_is_per_instance_under_logs() {
        assert_eq!(dir_in(Path::new("/d/devsandbox"), "web-2"), PathBuf::from("/d/devsandbox/logs/web-2"));
    }

    #[test]
    fn last_lines_keeps_the_end_as_a_terminal_shows_it() {
        let t = last_lines("a\nb\nc\n", 2, false);
        assert_eq!(t, Tail { text: "b\nc".into(), cut: true });
        let t = last_lines("a\nb\n", 5, false);
        assert_eq!(t, Tail { text: "a\nb".into(), cut: false });
        // A read from mid-file drops its first, possibly partial, line.
        let t = last_lines("tial\nb\n", 5, true);
        assert_eq!(t, Tail { text: "b".into(), cut: true });
        // Progress redraws keep their last state.
        let t = last_lines("step 1/3\rstep 3/3\r\ndone\n", 5, false);
        assert_eq!(t.text, "step 3/3\ndone");
        assert_eq!(last_lines("", 5, false), Tail { text: String::new(), cut: false });
    }

    #[test]
    fn create_rotates_one_generation() {
        let dir = temp_dir("rotate").join("logs/web");
        drop(create(&dir, "first\n").unwrap());
        assert_eq!(std::fs::read_to_string(dir.join(FILE)).unwrap(), "first\n");
        assert!(!dir.join(PREVIOUS).exists());

        let mut f = create(&dir, "second\n").unwrap();
        f.write_all(b"out\n").unwrap();
        assert_eq!(std::fs::read_to_string(dir.join(FILE)).unwrap(), "second\nout\n");
        assert_eq!(std::fs::read_to_string(dir.join(PREVIOUS)).unwrap(), "first\n");

        drop(create(&dir, "third\n").unwrap());
        assert_eq!(std::fs::read_to_string(dir.join(PREVIOUS)).unwrap(), "second\nout\n");
        let names: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names.len(), 2, "one generation kept: {names:?}");
        let _ = std::fs::remove_dir_all(dir.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn header_names_the_command_and_time() {
        let args = ["rebuild".to_string(), "--all".to_string()];
        assert_eq!(header(&args, 0, None), "$ devsandbox rebuild --all\n# 1970-01-01T00:00:00Z\n");
        let tty = Some(TtySize { cols: 120, rows: 40 });
        assert_eq!(header(&args, 0, tty), "$ devsandbox rebuild --all\n# 1970-01-01T00:00:00Z\n# tty 120x40\n");
    }

    #[test]
    fn tty_sizes_parse_and_print_as_colsxrows() {
        let size = TtySize { cols: 100, rows: 30 };
        assert_eq!(TtySize::parse("100x30"), Some(size));
        assert_eq!(size.to_string(), "100x30");
        for bad in ["", "100", "x30", "100x", "0x30", "100x0", "1001x30", "-1x30", "100X30", "axb"] {
            assert_eq!(TtySize::parse(bad), None, "{bad}");
        }
        assert_eq!(TtySize::sane(0, 0), TtySize { cols: 80, rows: 24 });
        assert_eq!(TtySize::sane(5000, 50), TtySize { cols: TtySize::MAX, rows: 50 });
    }

    #[test]
    fn a_pty_needs_both_terminals_or_the_env() {
        let term = Some(TtySize { cols: 200, rows: 50 });
        let env = Some("100x30");
        assert_eq!(pty_size(term, true, env), term, "the terminal's own size first");
        assert_eq!(pty_size(term, false, None), None, "stderr redirected: pipes");
        assert_eq!(pty_size(term, false, env), TtySize::parse("100x30"));
        assert_eq!(pty_size(None, true, env), TtySize::parse("100x30"));
        assert_eq!(pty_size(None, false, Some("junk")), None);
        assert_eq!(pty_size(None, false, None), None);
    }

    /// A child on the pty's slave sees a terminal of the pty's size; the
    /// master reads its output (ONLCR's `\r\n`) up to the hang-up (Linux's
    /// `EIO`) as EOF, without the stop flag.
    #[cfg(unix)]
    #[test]
    fn a_pty_child_sees_a_terminal_and_the_master_reads_to_eof() {
        use std::process::{Command, Stdio};
        let pty = imp::Pty::open(TtySize { cols: 100, rows: 30 }).unwrap();
        let script = "test -t 1 && echo tty; test -t 2 && echo err-tty >&2; stty size <&1";
        let mut child = Command::new("/bin/sh")
            .args(["-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::from(pty.slave.try_clone().unwrap()))
            .stderr(Stdio::from(pty.slave.try_clone().unwrap()))
            .spawn()
            .unwrap();
        drop(pty.slave);
        let stop = Arc::new(AtomicBool::new(false));
        let src = imp::StopRead { file: File::from(pty.master), stop };
        let mut term = Vec::new();
        let log = Mutex::new(Vec::new());
        copy(src, &mut term, &log);
        assert!(child.wait().unwrap().success());
        assert_eq!(String::from_utf8_lossy(&term), "tty\r\nerr-tty\r\n30 100\r\n");
        assert_eq!(log.into_inner().unwrap(), term);
    }

    const CHILD_ENV: &str = "DEVSANDBOX_TEST_BUILD_LOG_DIR";

    /// The whole tee on this process's real fds 1/2, so only in a process
    /// of its own: [`the_tee_runs_commands_on_a_pty_from_the_env`] runs this
    /// test binary for it. A no-op otherwise.
    #[cfg(unix)]
    #[test]
    fn pty_tee_child() {
        let Some(dir) = std::env::var_os(CHILD_ENV) else { return };
        let log = open_in(Path::new(&dir), "t");
        let script = "test -t 1 && echo out-tty; test -t 2 && echo err-tty >&2; stty size <&1";
        let status = std::process::Command::new("/bin/sh").args(["-c", script]).status();
        log.outcome(Ok(())).unwrap();
        assert!(status.unwrap().success());
        // fds back: straight to the file, no ONLCR.
        println!("after");
    }

    #[cfg(unix)]
    #[test]
    fn the_tee_runs_commands_on_a_pty_from_the_env() {
        let dir = temp_dir("pty-tee");
        std::fs::create_dir_all(&dir).unwrap();
        let out = File::create(dir.join("stdout")).unwrap();
        let err = File::create(dir.join("stderr")).unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["build_log::tests::pty_tee_child", "--exact", "--nocapture", "--test-threads=1"])
            .env(CHILD_ENV, dir.join("logs"))
            .env(TTY_ENV, "100x30")
            .stdin(std::process::Stdio::null())
            .stdout(out)
            .stderr(err)
            .status()
            .unwrap();
        let read = |name: &str| std::fs::read_to_string(dir.join(name)).unwrap();
        let (stdout, stderr) = (read("stdout"), read("stderr"));
        assert!(status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
        // Both streams through the pty onto the original stdout.
        let teed = "out-tty\r\nerr-tty\r\n30 100\r\n";
        assert!(stdout.contains(&format!("{teed}after\n")), "{stdout:?}");
        assert!(!stderr.contains("err-tty"), "{stderr:?}");
        let log = read("logs/build.log");
        let (head, body) = log.split_once("# tty 100x30\n").unwrap_or_else(|| panic!("{log:?}"));
        assert!(head.starts_with("$ devsandbox ") && head.lines().count() == 2, "{log:?}");
        assert_eq!(body, teed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Hands out its input in fixed-size pieces, like a pipe would.
    struct Chunked<'a>(&'a [u8], usize);

    impl Read for Chunked<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.1.min(self.0.len()).min(buf.len());
            buf[..n].copy_from_slice(&self.0[..n]);
            self.0 = &self.0[n..];
            Ok(n)
        }
    }

    /// Records each write separately, to see what reached the log when.
    #[derive(Default)]
    struct Writes(Vec<Vec<u8>>);

    impl Write for Writes {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.push(buf.to_vec());
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn copy_passes_through_and_logs_whole_lines() {
        let input = b"step 1\nstep 2 ... done\nno newline";
        let mut term = Vec::new();
        let log = Mutex::new(Writes::default());
        copy(Chunked(input, 3), &mut term, &log);
        assert_eq!(term, input);
        let writes = log.into_inner().unwrap().0;
        // Every write but the trailing remainder ends a line.
        let (last, lines) = writes.split_last().unwrap();
        assert!(lines.iter().all(|w| w.ends_with(b"\n")), "{writes:?}");
        assert_eq!(last, b"no newline");
        assert_eq!(writes.concat(), input);
    }

    #[test]
    fn copy_logs_an_overlong_line_without_its_newline() {
        let input = vec![b'x'; MAX_PENDING + 10];
        let log = Mutex::new(Vec::new());
        copy(Chunked(&input, 4096), std::io::sink(), &log);
        assert_eq!(log.into_inner().unwrap(), input);
    }

    struct Broken;

    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn copy_keeps_logging_when_the_terminal_side_fails() {
        let log = Mutex::new(Vec::new());
        copy(Chunked(b"a\nb\n", 1), Broken, &log);
        assert_eq!(log.into_inner().unwrap(), b"a\nb\n");
    }
}
