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
//! the dashboard and `devsandbox serve` call these commands in-process, where
//! fds 1/2 belong to the terminal UI or `serve.log` and other threads write
//! there too.
//!
//! Unix only: elsewhere [`open`] returns an inert log and nothing is written.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;

use crate::state::{Instance, State};

/// The log being written; the previous run's is [`PREVIOUS`].
pub const FILE: &str = "build.log";
/// The one rotated generation kept.
pub const PREVIOUS: &str = "build.1.log";

/// A line longer than this is logged without waiting for its newline, so a
/// child printing `\r`-only progress can't grow the buffer forever.
const MAX_PENDING: usize = 64 * 1024;

static ENABLED: AtomicBool = AtomicBool::new(false);

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

/// `$ devsandbox <args…>` (like the dispatch logs), then when.
fn header(args: &[String], now: u64) -> String {
    format!("$ devsandbox {}\n# {}\n", args.join(" "), crate::inbox::rfc3339(now))
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
    let inert = BuildLog {
        #[cfg(unix)]
        tee: None,
        file: None,
    };
    if !cfg!(unix) || !ENABLED.load(Ordering::SeqCst) {
        return inert;
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let file = match dir(instance_id).and_then(|d| Ok(create(&d, &header(&args, Instance::now()))?)) {
        Ok(file) => Arc::new(Mutex::new(file)),
        Err(e) => {
            eprintln!("warning: no build log for `{instance_id}`: {e:#}");
            return inert;
        }
    };
    #[cfg(unix)]
    {
        match imp::Tee::start(file.clone()) {
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

    /// How often a pipe reader checks whether it was told to stop.
    const POLL_MS: i32 = 100;

    /// fds 1/2 pointed at pipes, one copier thread each.
    pub(super) struct Tee {
        streams: Vec<Stream>,
        stop: Arc<AtomicBool>,
    }

    struct Stream {
        fd: RawFd,
        /// What `fd` pointed at before, put back on drop.
        saved: OwnedFd,
        thread: Option<JoinHandle<()>>,
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

    fn pipe_cloexec() -> std::io::Result<(OwnedFd, OwnedFd)> {
        let mut fds = [0; 2];
        // SAFETY: `fds` is a valid 2-element array for pipe(2).
        check(unsafe { libc::pipe(fds.as_mut_ptr()) })?;
        // SAFETY: both fds were just created and are owned here.
        let (r, w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        for fd in [&r, &w] {
            // SAFETY: fcntl on an owned, open fd.
            check(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) })?;
        }
        Ok((r, w))
    }

    fn flush_std() {
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
    }

    impl Tee {
        pub(super) fn start(log: Arc<Mutex<File>>) -> std::io::Result<Tee> {
            flush_std();
            let stop = Arc::new(AtomicBool::new(false));
            // Built up one fd at a time: on an error, dropping what's there
            // puts the fds already switched back.
            let mut tee = Tee { streams: Vec::new(), stop: stop.clone() };
            for fd in [1, 2] {
                let saved = dup_cloexec(fd)?;
                let term = File::from(saved.try_clone()?);
                let (r, w) = pipe_cloexec()?;
                // SAFETY: dup2 onto a standard stream; `saved` keeps the
                // original open. `w` closes below, leaving `fd` the only
                // write end (EOF once it's restored).
                check(unsafe { libc::dup2(w.as_raw_fd(), fd) })?;
                drop(w);
                let src = StopRead { file: File::from(r), stop: stop.clone() };
                let log = log.clone();
                let thread = std::thread::Builder::new()
                    .name(format!("build-log-{fd}"))
                    .spawn(move || super::copy(src, term, &log));
                let thread = match thread {
                    Ok(t) => Some(t),
                    Err(e) => {
                        // SAFETY: as above, back to the original.
                        unsafe { libc::dup2(saved.as_raw_fd(), fd) };
                        return Err(e);
                    }
                };
                tee.streams.push(Stream { fd, saved, thread });
            }
            Ok(tee)
        }
    }

    impl Drop for Tee {
        /// fds back first (closing the pipes' last write ends, so the copiers
        /// drain to EOF and flush), then join. A child that outlives the
        /// command holding a write end (nothing should; the daemon gets its
        /// own stdio) would block EOF forever: the stop flag ends the wait
        /// once the pipe stays empty for a poll interval.
        fn drop(&mut self) {
            flush_std();
            for s in &self.streams {
                // SAFETY: `saved` is an open fd owned by `s`.
                unsafe { libc::dup2(s.saved.as_raw_fd(), s.fd) };
            }
            self.stop.store(true, Ordering::SeqCst);
            for s in &mut self.streams {
                if let Some(t) = s.thread.take() {
                    let _ = t.join();
                }
            }
        }
    }

    /// A pipe's read end that reports EOF once `stop` is set and nothing is
    /// left to read. EINTR (a Ctrl-C caught by the cancel guard) is retried:
    /// the rollback's output must still come through.
    struct StopRead {
        file: File,
        stop: Arc<AtomicBool>,
    }

    impl Read for StopRead {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            loop {
                let mut p = libc::pollfd { fd: self.file.as_raw_fd(), events: libc::POLLIN, revents: 0 };
                // SAFETY: one valid pollfd.
                let ready = unsafe { libc::poll(&mut p, 1, POLL_MS) };
                if ready < 0 {
                    let e = std::io::Error::last_os_error();
                    if e.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(e);
                }
                if ready == 0 {
                    if self.stop.load(Ordering::SeqCst) {
                        return Ok(0);
                    }
                    continue;
                }
                // Readable, or hung up (read returns what's left, then 0).
                return self.file.read(buf);
            }
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
        assert_eq!(header(&args, 0), "$ devsandbox rebuild --all\n# 1970-01-01T00:00:00Z\n");
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
