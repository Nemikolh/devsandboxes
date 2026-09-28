//! `devsbd run start|supervise|ls|logs|wait`: tracked runs inside one
//! container (docs/automations.md, "Runs"). A dispatcher reaches a child's
//! runs through the host (`devsbd exec` / `devsbd run … <key>`, `ctl.rs`),
//! which execs these commands in the child; they work by hand in the child
//! too.
//!
//! Each run is a directory `DIR/<id>/` (id: `control::valid_run_id`):
//!
//! - `argv`: the command, one `arg <escaped>` line per word (`escape.rs`);
//! - `out.log`: its stdout + stderr, appended as written;
//! - `meta`: `started <unix>`, `supervisor <pid>`, `pid <pid>`, and at the end
//!   `ended <unix>` + `exit <code>` or `signal <n>` (one write, so both or
//!   neither). Lines are appended by two processes, so order isn't fixed and
//!   only `\n`-terminated lines count (a torn last line is ignored).
//!
//! Without libc there is no `fork`: `start` spawns `devsbd run supervise <id>`
//! detached (its own process group, null stdio) and exits; the supervisor
//! spawns the command, waits for it, and records how it ended. A run whose
//! supervisor is gone without an `ended` (container restart, killed) is
//! `lost`. The supervisor is identified by its pid *and* its cmdline naming
//! the id, since pids restart from 1 with the container.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::control::{self, valid_run_id};
use crate::escape::{escape, unescape};
use crate::outbox;

/// Runs live next to the notify outbox: `/var/lib` is in the container's
/// writable layer, so they survive a restart (not a rebuild). World-writable +
/// sticky like the outbox (`INSTALL_SCRIPT`): runs start as the sandbox user.
pub const DIR: &str = "/var/lib/devsandbox/runs";

/// Most log bytes one `logs` call prints: keeps a `run-logs` response (up to
/// 3x after U+FFFD replacement) under `control::MAX_RESPONSE`.
pub const MAX_CHUNK: u64 = 256 * 1024;

/// `wait`'s default timeout, seconds.
const DEFAULT_WAIT: u64 = 60;
const POLL: Duration = Duration::from_millis(100);

/// A run with neither `supervisor` nor `ended` is still starting for this
/// long (`start` records the supervisor right after spawning it); after that,
/// `start` died in between and the run is `lost`.
const STARTING_GRACE: u64 = 10;

const USAGE: &str = "usage: devsbd run start [--cwd DIR] -- <cmd>...\n\
       devsbd run ls\n\
       devsbd run logs <id> [--offset N]\n\
       devsbd run wait <id> [--timeout SECS]";

/// Whether argv after `run` is this container's own form (else it names a
/// child: `ctl::run_remote`). Decided by positional count: `ls` takes none
/// here and a `<key>` remotely; `logs`/`wait` take `<id>` here and `<key>
/// <id>` remotely. `start`/`supervise` only exist here.
pub fn is_local(args: &[String]) -> bool {
    let (sub, rest) = match args.split_first() {
        Some((sub, rest)) => (sub.as_str(), rest),
        None => return true,
    };
    let positional = || positionals(rest, &["--offset", "--timeout", "--sandbox"]);
    match sub {
        "ls" => positional() == 0,
        "logs" | "wait" => positional() <= 1,
        _ => true,
    }
}

/// Count of non-flag words, skipping the values of `value_flags` given as
/// `--flag value`; stops at `--`.
fn positionals(args: &[String], value_flags: &[&str]) -> usize {
    let (mut n, mut it) = (0, args.iter());
    while let Some(a) = it.next() {
        if a == "--" {
            break;
        }
        if value_flags.contains(&a.as_str()) {
            it.next();
        } else if !a.starts_with('-') || a == "-" {
            n += 1;
        }
    }
    n
}

/// `devsbd run …` in this container; returns the exit code.
pub fn run(args: &[String]) -> i32 {
    let root = Path::new(DIR);
    let usage = |e: String| {
        eprintln!("devsbd run: {e}\n{USAGE}");
        control::EXIT_USAGE
    };
    let cmd = match parse(args) {
        Ok(cmd) => cmd,
        Err(e) => return usage(e),
    };
    let result = match cmd {
        Cmd::Start { cwd, argv } => start(root, &argv, cwd.as_deref()).map(|id| println!("{id}")),
        Cmd::Supervise { id } => supervise(root, &id),
        Cmd::Ls => ls(root).and_then(|text| io::stdout().write_all(text.as_bytes())),
        Cmd::Logs { id, offset } => {
            read_chunk(root, &id, offset, MAX_CHUNK).and_then(|b| io::stdout().write_all(&b))
        }
        Cmd::Wait { id, timeout } => {
            wait(root, &id, Duration::from_secs(timeout)).map(|s| println!("{}", s.label()))
        }
    };
    match result {
        Ok(()) => control::EXIT_OK,
        Err(e) => {
            eprintln!("devsbd run: {e}");
            control::EXIT_FAILED
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Cmd {
    Start { cwd: Option<String>, argv: Vec<String> },
    Supervise { id: String },
    Ls,
    Logs { id: String, offset: u64 },
    Wait { id: String, timeout: u64 },
}

fn parse(args: &[String]) -> Result<Cmd, String> {
    let (sub, rest) = args.split_first().ok_or("missing subcommand")?;
    let mut cwd = None;
    let (mut offset, mut timeout) = (None, None);
    let mut words: Vec<String> = Vec::new();
    let mut argv = None;
    let mut it = rest.iter();
    while let Some(arg) = it.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") && f.len() > 2 => (f, Some(v.to_string())),
            _ => (arg.as_str(), None),
        };
        let mut value = || match inline.clone().or_else(|| it.next().cloned()) {
            Some(v) if !v.is_empty() => Ok(v),
            _ => Err(format!("{flag} needs a value")),
        };
        let num = |v: String| v.parse::<u64>().map_err(|_| format!("{flag}: bad number `{v}`"));
        match (sub.as_str(), flag) {
            ("start", "--") => {
                argv = Some(it.by_ref().cloned().collect::<Vec<_>>());
                break;
            }
            ("start", "--cwd") => once(&mut cwd, value()?, flag)?,
            ("logs", "--offset") => once(&mut offset, num(value()?)?, flag)?,
            ("wait", "--timeout") => once(&mut timeout, num(value()?)?, flag)?,
            (_, f) if f.starts_with('-') && f.len() > 1 => return Err(format!("unknown option `{f}`")),
            _ => words.push(arg.clone()),
        }
    }
    let id = |words: &mut Vec<String>| match words.as_slice() {
        [id] if valid_run_id(id) => Ok(words.remove(0)),
        [id] => Err(format!("bad run id `{id}`")),
        _ => Err("takes exactly one <id>".to_string()),
    };
    match sub.as_str() {
        "start" => {
            if !words.is_empty() {
                return Err("the command goes after `--`".into());
            }
            match argv {
                Some(argv) if !argv.is_empty() => Ok(Cmd::Start { cwd, argv }),
                _ => Err("missing `-- <cmd>...`".into()),
            }
        }
        "supervise" => Ok(Cmd::Supervise { id: id(&mut words)? }),
        "ls" if words.is_empty() => Ok(Cmd::Ls),
        "ls" => Err("takes no arguments".into()),
        "logs" => Ok(Cmd::Logs { id: id(&mut words)?, offset: offset.unwrap_or(0) }),
        "wait" => Ok(Cmd::Wait { id: id(&mut words)?, timeout: timeout.unwrap_or(DEFAULT_WAIT) }),
        other => Err(format!("unknown subcommand `{other}`")),
    }
}

fn once<T>(slot: &mut Option<T>, value: T, flag: &str) -> Result<(), String> {
    match slot.replace(value) {
        Some(_) => Err(format!("{flag} given twice")),
        None => Ok(()),
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn run_dir(root: &Path, id: &str) -> io::Result<PathBuf> {
    let dir = root.join(id);
    if !valid_run_id(id) || !dir.is_dir() {
        return Err(io::Error::new(io::ErrorKind::NotFound, format!("no run `{id}`")));
    }
    Ok(dir)
}

fn append(dir: &Path, text: &str) -> io::Result<()> {
    // One `write` per call on an O_APPEND file: lines from `start` and the
    // supervisor never interleave mid-line.
    OpenOptions::new().create(true).append(true).open(dir.join("meta"))?.write_all(text.as_bytes())
}

/// Create run `<now>-<hex>` for `argv`: its dir (a fresh name: `create_dir`
/// fails on a taken one, so concurrent starts can't share), `argv`, and the
/// `started` line. Returns the id.
fn create(root: &Path, argv: &[String], now: u64) -> io::Result<String> {
    outbox::ensure_dir(root)?;
    let seed = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0)
        ^ std::process::id().rotate_left(16);
    for n in 0..64u32 {
        let id = format!("{now:010}-{:04x}", (seed.wrapping_add(n.wrapping_mul(0x9E37))) & 0xFFFF);
        let dir = root.join(&id);
        match fs::create_dir(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
        let mut text = String::new();
        for arg in argv {
            text.push_str("arg ");
            escape(arg, &mut text);
            text.push('\n');
        }
        fs::write(dir.join("argv"), text)?;
        append(&dir, &format!("started {now}\n"))?;
        return Ok(id);
    }
    Err(io::Error::other("no free run id"))
}

/// `run start`: create the run, spawn its supervisor detached (in `cwd`, so
/// the command runs there; a bad `cwd` fails here, before anything runs),
/// record the supervisor's pid.
fn start(root: &Path, argv: &[String], cwd: Option<&str>) -> io::Result<String> {
    let id = create(root, argv, now())?;
    let dir = root.join(&id);
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.args(["run", "supervise", &id])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    match cmd.spawn() {
        Ok(child) => {
            append(&dir, &format!("supervisor {}\n", child.id()))?;
            Ok(id)
        }
        Err(e) => {
            let _ = fs::remove_dir_all(&dir);
            Err(io::Error::new(e.kind(), format!("cannot start the run: {e}")))
        }
    }
}

fn read_argv(dir: &Path) -> io::Result<Vec<String>> {
    let text = fs::read_to_string(dir.join("argv"))?;
    text.lines()
        .map(|l| {
            let v = l.strip_prefix("arg ").or((l == "arg").then_some("")).ok_or("bad argv line")?;
            unescape(v)
        })
        .collect::<Result<_, _>>()
        .map_err(io::Error::other)
}

/// `run supervise`: run the command (stdin null, output to `out.log`, cwd and
/// env inherited from `start`), record its pid, wait, record the end. A
/// command that can't be spawned ends at once with exit 127, its error in the
/// log, as a shell would.
fn supervise(root: &Path, id: &str) -> io::Result<()> {
    let dir = run_dir(root, id)?;
    let argv = read_argv(&dir)?;
    let mut log = OpenOptions::new().create(true).append(true).open(dir.join("out.log"))?;
    let Some((program, rest)) = argv.split_first() else {
        return Err(io::Error::other("empty argv"));
    };
    let spawned = Command::new(program)
        .args(rest)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log.try_clone()?)
        .spawn();
    let end = match spawned {
        Ok(mut child) => {
            append(&dir, &format!("pid {}\n", child.id()))?;
            exit_line(child.wait()?)
        }
        Err(e) => {
            let _ = writeln!(log, "devsbd: cannot run `{program}`: {e}");
            "exit 127".to_string()
        }
    };
    append(&dir, &format!("ended {}\n{end}\n", now()))
}

fn exit_line(status: ExitStatus) -> String {
    match (status.code(), status.signal()) {
        (Some(code), _) => format!("exit {code}"),
        (None, Some(sig)) => format!("signal {sig}"),
        (None, None) => "exit 1".to_string(),
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Meta {
    started: Option<u64>,
    supervisor: Option<u32>,
    ended: Option<u64>,
    exit: Option<State>,
}

/// Complete lines of a `meta` file; unknown keys are skipped (a newer helper
/// may add some), as is a torn last line.
fn parse_meta(text: &str) -> Meta {
    let mut meta = Meta::default();
    let complete = &text[..text.rfind('\n').map_or(0, |i| i + 1)];
    for line in complete.lines() {
        let Some((key, value)) = line.split_once(' ') else { continue };
        match key {
            "started" => meta.started = value.parse().ok(),
            "supervisor" => meta.supervisor = value.parse().ok(),
            "ended" => meta.ended = value.parse().ok(),
            "exit" => meta.exit = value.parse().ok().map(State::Exited),
            "signal" => meta.exit = value.parse().ok().map(State::Killed),
            _ => {}
        }
    }
    meta
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Running,
    Exited(i32),
    Killed(i32),
    Lost,
}

impl State {
    /// As printed by `ls` and `wait` (and parsed by `ctl.rs`).
    fn label(self) -> String {
        match self {
            State::Running => "running".into(),
            State::Exited(code) => format!("exited {code}"),
            State::Killed(sig) => format!("killed {sig}"),
            State::Lost => "lost".into(),
        }
    }
}

/// A run's state from its `meta`: ended → how; else running while the
/// supervisor lives (`alive`), `lost` once it's gone. No supervisor recorded
/// yet: starting (running) within `STARTING_GRACE` of `started`.
fn state_of(meta: &Meta, now: u64, alive: impl Fn(u32) -> bool) -> State {
    if meta.ended.is_some() {
        return meta.exit.unwrap_or(State::Lost);
    }
    match meta.supervisor {
        Some(pid) if alive(pid) => State::Running,
        Some(_) => State::Lost,
        None if now.saturating_sub(meta.started.unwrap_or(0)) < STARTING_GRACE => State::Running,
        None => State::Lost,
    }
}

/// Whether `pid` is run `id`'s supervisor: alive, and its cmdline names the
/// id (a zombie's cmdline is empty, so a finished unreaped one doesn't count).
fn is_supervisor(pid: u32, id: &str) -> bool {
    let cmdline = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    cmdline.split(|&b| b == 0).any(|w| w == id.as_bytes())
}

fn load_state(dir: &Path, id: &str) -> io::Result<(Meta, State)> {
    let read = || fs::read_to_string(dir.join("meta")).map(|t| parse_meta(&t));
    let meta = read()?;
    let state = state_of(&meta, now(), |pid| is_supervisor(pid, id));
    if state != State::Lost {
        return Ok((meta, state));
    }
    // The supervisor may have written `ended` and exited between the read and
    // the liveness check: look once more before calling it lost. A young run's
    // supervisor may also be mid-exec (spawn returns before exec sets up its
    // cmdline), so give that a moment.
    if now().saturating_sub(meta.started.unwrap_or(0)) < STARTING_GRACE {
        std::thread::sleep(Duration::from_millis(50));
    }
    let meta = read()?;
    let state = state_of(&meta, now(), |pid| is_supervisor(pid, id));
    Ok((meta, state))
}

/// `run ls`: one line per run, oldest first:
/// `<id> <state> <started, UTC> <argv…>` (state is one or two words:
/// `running`, `exited N`, `killed N`, `lost`). Argv words are joined by
/// spaces, escaped as in `escape.rs` so a run stays one line.
fn ls(root: &Path) -> io::Result<String> {
    let mut ids: Vec<String> = match fs::read_dir(root) {
        Ok(entries) => entries
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| valid_run_id(n))
            .collect(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e),
    };
    ids.sort();
    let mut out = String::new();
    for id in ids {
        let dir = root.join(&id);
        let Ok((meta, state)) = load_state(&dir, &id) else { continue };
        let argv = read_argv(&dir).unwrap_or_default();
        let started = meta.started.or_else(|| id[..10].parse().ok()).unwrap_or(0);
        out.push_str(&format!("{id} {} {}", state.label(), crate::boot::format_utc(started)));
        for arg in argv {
            out.push(' ');
            escape(&arg, &mut out);
        }
        out.push('\n');
    }
    Ok(out)
}

/// `run logs`: up to `cap` bytes of the log from `offset` (none past the end,
/// or before the command wrote anything).
fn read_chunk(root: &Path, id: &str, offset: u64, cap: u64) -> io::Result<Vec<u8>> {
    let dir = run_dir(root, id)?;
    let mut f = match File::open(dir.join("out.log")) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = Vec::new();
    f.take(cap).read_to_end(&mut buf)?;
    Ok(buf)
}

/// `run wait`: the run's state once it isn't running, or `Running` after
/// `timeout` (callers loop).
fn wait(root: &Path, id: &str, timeout: Duration) -> io::Result<State> {
    let dir = run_dir(root, id)?;
    let deadline = Instant::now() + timeout;
    loop {
        let (_, state) = load_state(&dir, id)?;
        if state != State::Running || Instant::now() >= deadline {
            return Ok(state);
        }
        std::thread::sleep(POLL.min(deadline.saturating_duration_since(Instant::now())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("devsbd-runs-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        root
    }

    #[test]
    fn parses_local_forms() {
        let p = |a: &[&str]| parse(&strings(a));
        assert_eq!(
            p(&["start", "--cwd", "/w", "--", "sh", "-c", "echo --x"]),
            Ok(Cmd::Start { cwd: Some("/w".into()), argv: strings(&["sh", "-c", "echo --x"]) })
        );
        assert_eq!(p(&["start", "--", "true"]), Ok(Cmd::Start { cwd: None, argv: strings(&["true"]) }));
        assert_eq!(p(&["ls"]), Ok(Cmd::Ls));
        let id = "1790000000-a1b2";
        assert_eq!(p(&["logs", id]), Ok(Cmd::Logs { id: id.into(), offset: 0 }));
        assert_eq!(p(&["logs", id, "--offset=9"]), Ok(Cmd::Logs { id: id.into(), offset: 9 }));
        assert_eq!(p(&["wait", "--timeout", "3", id]), Ok(Cmd::Wait { id: id.into(), timeout: 3 }));
        assert_eq!(p(&["wait", id]), Ok(Cmd::Wait { id: id.into(), timeout: DEFAULT_WAIT }));
        assert_eq!(p(&["supervise", id]), Ok(Cmd::Supervise { id: id.into() }));

        assert!(p(&[]).is_err());
        assert!(p(&["start"]).unwrap_err().contains("missing `-- <cmd>"));
        assert!(p(&["start", "--"]).unwrap_err().contains("missing"));
        assert!(p(&["start", "sh"]).unwrap_err().contains("after `--`"));
        assert!(p(&["ls", "x"]).unwrap_err().contains("no arguments"));
        assert!(p(&["logs", "../etc"]).unwrap_err().contains("bad run id"));
        assert!(p(&["logs"]).unwrap_err().contains("exactly one"));
        assert!(p(&["logs", id, "--offset", "x"]).unwrap_err().contains("bad number"));
        assert!(p(&["logs", id, "--timeout", "1"]).unwrap_err().contains("unknown option"));
        assert!(p(&["wait", id, "--timeout", "1", "--timeout", "2"]).unwrap_err().contains("twice"));
        assert!(p(&["nope"]).unwrap_err().contains("unknown subcommand"));
    }

    #[test]
    fn local_or_remote_by_positional_count() {
        let local = |a: &[&str]| is_local(&strings(a));
        let id = "1790000000-a1b2";
        assert!(local(&["ls"]));
        assert!(!local(&["ls", "pr-1"]));
        assert!(!local(&["ls", "--sandbox", "web", "pr-1"]));
        assert!(local(&["logs", id, "--offset", "5"]));
        assert!(!local(&["logs", "pr-1", id]));
        assert!(!local(&["logs", "pr-1", id, "--follow"]));
        assert!(local(&["wait", id, "--timeout", "5"]));
        assert!(!local(&["wait", "pr-1", id]));
        assert!(local(&["start", "--", "a", "b", "c"]));
        assert!(local(&[]));
    }

    #[test]
    fn meta_parsing_and_state() {
        let m = parse_meta("started 100\nsupervisor 7\npid 8\nended 105\nexit 3\n");
        assert_eq!(m, Meta { started: Some(100), supervisor: Some(7), ended: Some(105), exit: Some(State::Exited(3)) });
        assert_eq!(state_of(&m, 200, |_| false), State::Exited(3));
        let m = parse_meta("started 100\nended 101\nsignal 9\nfuture x\n");
        assert_eq!(state_of(&m, 200, |_| true), State::Killed(9));
        // Torn last line: `ended` not complete yet.
        let m = parse_meta("started 100\nsupervisor 7\nended 10");
        assert_eq!(m.ended, None);
        assert_eq!(state_of(&m, 101, |pid| pid == 7), State::Running);
        // Supervisor gone without `ended`: lost.
        assert_eq!(state_of(&m, 101, |_| false), State::Lost);
        // Not recorded yet: starting, then lost after the grace.
        let m = parse_meta("started 100\n");
        assert_eq!(state_of(&m, 100 + STARTING_GRACE - 1, |_| false), State::Running);
        assert_eq!(state_of(&m, 100 + STARTING_GRACE, |_| false), State::Lost);
        assert_eq!(State::Exited(0).label(), "exited 0");
        assert_eq!(State::Killed(15).label(), "killed 15");
    }

    /// start → supervise → ended on the host itself (in-process supervisor:
    /// the test binary isn't devsbd), then logs with offsets, ls, wait.
    #[test]
    fn a_run_end_to_end_in_process() {
        let root = temp_root("e2e");
        let argv = strings(&["sh", "-c", "echo hi; echo err >&2; exit 3"]);
        let id = create(&root, &argv, now()).unwrap();
        assert!(valid_run_id(&id), "{id}");
        assert_eq!(read_argv(&root.join(&id)).unwrap(), argv);
        // No supervisor yet, fresh: running; a zero wait says so.
        assert_eq!(wait(&root, &id, Duration::ZERO).unwrap(), State::Running);
        supervise(&root, &id).unwrap();
        assert_eq!(wait(&root, &id, Duration::from_secs(5)).unwrap(), State::Exited(3));
        assert_eq!(read_chunk(&root, &id, 0, MAX_CHUNK).unwrap(), b"hi\nerr\n");
        assert_eq!(read_chunk(&root, &id, 3, MAX_CHUNK).unwrap(), b"err\n");
        assert_eq!(read_chunk(&root, &id, 1, 2).unwrap(), b"i\n");
        assert_eq!(read_chunk(&root, &id, 99, MAX_CHUNK).unwrap(), b"");
        let meta = fs::read_to_string(root.join(&id).join("meta")).unwrap();
        assert!(meta.contains("\npid ") && meta.ends_with("exit 3\n"), "{meta}");

        let listed = ls(&root).unwrap();
        let line = listed.lines().next().unwrap();
        assert!(line.starts_with(&format!("{id} exited 3 20")), "{line}");
        assert!(line.ends_with("Z sh -c echo hi; echo err >&2; exit 3"), "{line}");

        // A killed command and one that can't be spawned.
        let id2 = create(&root, &strings(&["sh", "-c", "kill -9 $$"]), now()).unwrap();
        supervise(&root, &id2).unwrap();
        assert_eq!(wait(&root, &id2, Duration::ZERO).unwrap(), State::Killed(9));
        let id3 = create(&root, &strings(&["/nonexistent/cmd"]), now()).unwrap();
        supervise(&root, &id3).unwrap();
        assert_eq!(wait(&root, &id3, Duration::ZERO).unwrap(), State::Exited(127));
        let log = read_chunk(&root, &id3, 0, MAX_CHUNK).unwrap();
        assert!(String::from_utf8_lossy(&log).contains("cannot run `/nonexistent/cmd`"));
        assert_eq!(ls(&root).unwrap().lines().count(), 3);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn lost_runs_and_unknown_ids() {
        let root = temp_root("lost");
        let id = create(&root, &strings(&["sleep", "1000"]), now()).unwrap();
        // A recorded supervisor that doesn't exist (pid beyond pid_max), or is
        // some other process (ours: its cmdline doesn't name the id).
        append(&root.join(&id), "supervisor 4294967\n").unwrap();
        assert_eq!(wait(&root, &id, Duration::from_secs(5)).unwrap(), State::Lost);
        let id2 = create(&root, &strings(&["true"]), now()).unwrap();
        append(&root.join(&id2), &format!("supervisor {}\n", std::process::id())).unwrap();
        assert_eq!(wait(&root, &id2, Duration::ZERO).unwrap(), State::Lost);
        assert!(ls(&root).unwrap().lines().all(|l| l.split(' ').nth(1) == Some("lost")));

        let missing = "1790000000-ffff";
        assert_eq!(read_chunk(&root, missing, 0, 1).unwrap_err().kind(), io::ErrorKind::NotFound);
        assert!(wait(&root, missing, Duration::ZERO).is_err());
        assert!(read_chunk(&root, "../lost", 0, 1).is_err());
        // An empty or missing root lists nothing.
        assert_eq!(ls(&temp_root("none")).unwrap(), "");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn wait_times_out_on_a_live_run() {
        let root = temp_root("wait");
        let id = create(&root, &strings(&["true"]), now()).unwrap();
        // The test thread's own cmdline doesn't name the id, so fake a live
        // supervisor by spawning one whose argv does (`; :` keeps sh from
        // exec'ing sleep in its place).
        let mut sup = Command::new("sh").args(["-c", "sleep 5; :", &id]).spawn().unwrap();
        append(&root.join(&id), &format!("supervisor {}\n", sup.id())).unwrap();
        // A vfork-style spawn returns before exec has set up the new cmdline.
        let t = Instant::now();
        while !is_supervisor(sup.id(), &id) && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(10));
        }
        let t = Instant::now();
        assert_eq!(wait(&root, &id, Duration::from_millis(300)).unwrap(), State::Running);
        assert!(t.elapsed() >= Duration::from_millis(300));
        let _ = sup.kill();
        let _ = sup.wait();
        assert_eq!(wait(&root, &id, Duration::ZERO).unwrap(), State::Lost);
        let _ = fs::remove_dir_all(&root);
    }
}
