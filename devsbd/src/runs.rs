//! `devsbd run start|supervise|ls|logs|wait|rm|prune`: tracked runs inside one
//! container (docs/automations.md, "Runs"). A dispatcher reaches a child's
//! runs through the host (`devsbd exec` / `devsbd run … <key>`, `ctl.rs`),
//! which execs these commands in the child; they work by hand in the child
//! too.
//!
//! Each run is a directory `DIR/<id>/` (id: `control::valid_run_id`):
//!
//! - `argv`: the command, one `arg <escaped>` line per word (`escape.rs`);
//! - `out.log`: its stdout + stderr, appended as written;
//! - `meta`: `started <unix>`, `supervisor <pid>`, `pid <pid>` + `pgid <pgid>`
//!   (the command leads its own process group, what `rm --force` kills), and at the end
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
//!
//! Runs are kept until `rm <id>` / `prune` deletes them (the dispatcher's
//! job, through `devsbd run rm|prune <key>`).

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

/// `ls` lists only this many runs, the newest: the runs dir is world-writable,
/// so anyone in the container can fill it, and the host reads `ls` often.
const MAX_LS_RUNS: usize = 50;

/// Characters of rendered (escaped) argv per `ls` line; longer is cut, `…`
/// appended.
const MAX_LS_ARGV_CHARS: usize = 200;

/// Largest `argv` / `meta` read; a bigger file makes the run unreadable
/// rather than parsed from a cut-off prefix.
const MAX_ARGV_BYTES: u64 = 64 * 1024;
const MAX_META_BYTES: u64 = 4 * 1024;

/// `wait`'s default timeout, seconds.
const DEFAULT_WAIT: u64 = 60;
const POLL: Duration = Duration::from_millis(100);

/// A run with neither `supervisor` nor `ended` is still starting for this
/// long (`start` records the supervisor right after spawning it); after that,
/// `start` died in between and the run is `lost`.
const STARTING_GRACE: u64 = 10;

/// How long `rm --force` waits for a young run's supervisor to record the
/// command's group, then for the killed run to be recorded as ended.
const KILL_WAIT: Duration = Duration::from_secs(5);

const USAGE: &str = "usage: devsbd run start [--cwd DIR] -- <cmd>...\n\
       devsbd run ls\n\
       devsbd run logs <id> [--offset N]\n\
       devsbd run wait <id> [--timeout SECS]\n\
       devsbd run rm <id> [--force]\n\
       devsbd run prune [--keep N]";

/// Whether argv after `run` is this container's own form (else it names a
/// child: `ctl::run_remote`). Decided by positional count: `ls` takes none
/// here and a `<key>` remotely; `logs`/`wait`/`rm` take `<id>` here and `<key>
/// <id>` remotely; `prune` takes none here and a `<key>` remotely.
/// `start`/`supervise` only exist here.
pub fn is_local(args: &[String]) -> bool {
    let (sub, rest) = match args.split_first() {
        Some((sub, rest)) => (sub.as_str(), rest),
        None => return true,
    };
    let positional = || positionals(rest, &["--offset", "--timeout", "--sandbox", "--keep"]);
    match sub {
        "ls" | "prune" => positional() == 0,
        "logs" | "wait" | "rm" => positional() <= 1,
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
        Cmd::Rm { id, force } => rm(root, &id, force),
        Cmd::Prune { keep } => prune(root, keep).map(|n| println!("removed {n}")),
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
    Rm { id: String, force: bool },
    Prune { keep: usize },
}

fn parse(args: &[String]) -> Result<Cmd, String> {
    let (sub, rest) = args.split_first().ok_or("missing subcommand")?;
    let mut cwd = None;
    let (mut offset, mut timeout, mut keep) = (None, None, None);
    let mut force = false;
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
            ("rm", "--force") if inline.is_none() => force = true,
            ("rm", "--force") => return Err("--force takes no value".into()),
            ("prune", "--keep") => once(&mut keep, num(value()?)?, flag)?,
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
        "rm" => Ok(Cmd::Rm { id: id(&mut words)?, force }),
        "prune" if words.is_empty() => {
            Ok(Cmd::Prune { keep: usize::try_from(keep.unwrap_or(0)).unwrap_or(usize::MAX) })
        }
        "prune" => Err("takes no arguments".into()),
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
    if !valid_run_id(id) || !is_real_dir(&dir) {
        return Err(io::Error::new(io::ErrorKind::NotFound, format!("no run `{id}`")));
    }
    Ok(dir)
}

/// A directory itself, not a symlink to one (`ls` and `run_dir` skip those).
fn is_real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
}

/// `path` as text, when it's a regular file (not a symlink, dir, FIFO, …) of
/// at most `cap` bytes; anything else is an error, so a planted file can't
/// make a reader block or buffer without bound.
fn read_capped(path: &Path, cap: u64) -> io::Result<String> {
    if !fs::symlink_metadata(path)?.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{} is not a regular file", path.display())));
    }
    let mut buf = Vec::new();
    File::open(path)?.take(cap + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > cap {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{} is over {cap} bytes", path.display())));
    }
    String::from_utf8(buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
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
    let exe = std::env::current_exe()?;
    start_with(root, argv, cwd, |id| {
        let mut cmd = Command::new(&exe);
        cmd.args(["run", "supervise", id]);
        cmd
    })
}

/// [`start`] with the supervisor command for run `id` from `supervisor`
/// (tests run it from the test binary, over their own root).
fn start_with(
    root: &Path,
    argv: &[String],
    cwd: Option<&str>,
    supervisor: impl FnOnce(&str) -> Command,
) -> io::Result<String> {
    let id = create(root, argv, now())?;
    let dir = root.join(&id);
    let mut cmd = supervisor(&id);
    cmd.stdin(Stdio::null())
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
    let text = read_capped(&dir.join("argv"), MAX_ARGV_BYTES)?;
    text.lines()
        .map(|l| {
            let v = l.strip_prefix("arg ").or((l == "arg").then_some("")).ok_or("bad argv line")?;
            unescape(v)
        })
        .collect::<Result<_, _>>()
        .map_err(io::Error::other)
}

/// `run supervise`: run the command (stdin null, output to `out.log`, cwd and
/// env inherited from `start`) as the leader of a new process group, record
/// its pid and group, wait, record the end. The group is what `rm --force`
/// kills: the command and whatever it forks, while the supervisor survives
/// to record the end. A
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
        .process_group(0)
        .spawn();
    let end = match spawned {
        Ok(mut child) => {
            let pid = child.id();
            append(&dir, &format!("pid {pid}\npgid {pid}\n"))?;
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
    pid: Option<u32>,
    /// The command's process group (absent: not spawned yet, or a supervisor
    /// from before `rm`, whose command shares the supervisor's group).
    pgid: Option<u32>,
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
            "pid" => meta.pid = value.parse().ok(),
            "pgid" => meta.pgid = value.parse().ok(),
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
    let read = || read_capped(&dir.join("meta"), MAX_META_BYTES).map(|t| parse_meta(&t));
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

/// `run ls`: one line per run, oldest first, only the newest
/// [`MAX_LS_RUNS`]: `<id> <state> <started, UTC> <argv…>` (state is one or
/// two words: `running`, `exited N`, `killed N`, `lost`). Argv words are
/// joined by spaces, escaped as in `escape.rs` so a run stays one line, and
/// cut at [`MAX_LS_ARGV_CHARS`]. Runs that aren't real dirs or whose
/// `meta`/`argv` can't be read are skipped. The newest are picked by name
/// (ids start with the start time) before anything is read, so a flood of
/// dirs costs one `lstat` each.
fn ls(root: &Path) -> io::Result<String> {
    let ids = run_ids(root)?;
    let mut out = String::new();
    for id in &ids[ids.len().saturating_sub(MAX_LS_RUNS)..] {
        let dir = root.join(id);
        let Ok((meta, state)) = load_state(&dir, id) else { continue };
        let argv = match read_argv(&dir) {
            Ok(argv) => argv,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(_) => continue,
        };
        let started = meta.started.or_else(|| id[..10].parse().ok()).unwrap_or(0);
        out.push_str(&format!("{id} {} {}", state.label(), crate::boot::format_utc(started)));
        out.push_str(&render_argv(&argv));
        out.push('\n');
    }
    Ok(out)
}

/// Every run id under `root` that is a real dir, sorted (start order; runs
/// started in the same second tie-break on the id's random part). A missing
/// root has none.
fn run_ids(root: &Path) -> io::Result<Vec<String>> {
    let mut ids: Vec<String> = match fs::read_dir(root) {
        Ok(entries) => entries
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| valid_run_id(n) && is_real_dir(&root.join(n)))
            .collect(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e),
    };
    ids.sort();
    Ok(ids)
}

/// ` <word> <word>…`, escaped, cut to [`MAX_LS_ARGV_CHARS`] chars plus `…`.
fn render_argv(argv: &[String]) -> String {
    let mut text = String::new();
    for arg in argv {
        text.push(' ');
        escape(arg, &mut text);
    }
    match text.char_indices().nth(MAX_LS_ARGV_CHARS + 1) {
        Some((cut, _)) => {
            text.truncate(cut);
            text.push('…');
            text
        }
        None => text,
    }
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

fn other(msg: String) -> io::Error {
    io::Error::other(msg)
}

/// `run rm`: delete a run's dir. A running run is refused unless `force`,
/// which kills it first ([`kill_run`]); so is one whose `meta` can't be read
/// (its state is unknown), which `force` deletes as is. The dir is checked to
/// be a real one (`run_dir`), and `remove_dir_all` never follows symlinks,
/// not even a dir swapped for one in between.
fn rm(root: &Path, id: &str, force: bool) -> io::Result<()> {
    let dir = run_dir(root, id)?;
    match load_state(&dir, id) {
        Ok((_, State::Running)) if !force => {
            return Err(other(format!("run `{id}` is running; `--force` kills it first")));
        }
        Ok((meta, State::Running)) => kill_run(root, &dir, id, meta)?,
        Ok(_) => {}
        Err(e) if !force => {
            return Err(io::Error::new(
                e.kind(),
                format!("cannot read run `{id}`: {e}; `--force` removes it anyway"),
            ));
        }
        Err(_) => {}
    }
    fs::remove_dir_all(&dir)
}

/// SIGKILL running run `id`'s process group ([`kill_target`]) and wait for
/// its end to be recorded, so the supervisor isn't writing into the dir as
/// it's deleted. A young run whose command isn't spawned yet gets a moment to
/// record it first.
fn kill_run(root: &Path, dir: &Path, id: &str, mut meta: Meta) -> io::Result<()> {
    let deadline = Instant::now() + KILL_WAIT;
    while meta.pid.is_none() && Instant::now() < deadline {
        std::thread::sleep(POLL);
        let (fresh, state) = load_state(dir, id)?;
        if state != State::Running {
            return Ok(());
        }
        meta = fresh;
    }
    if let Some(pgid) = kill_target(&meta, proc_stat) {
        kill_group(pgid)?;
    }
    if wait(root, id, KILL_WAIT)? == State::Running {
        return Err(other(format!("run `{id}` is still running after the kill")));
    }
    Ok(())
}

/// `/proc/<pid>/stat`'s `(ppid, pgrp)`. The comm field is parenthesized and
/// may hold spaces or `)`, so fields are counted from the last `)`.
fn parse_stat(text: &str) -> Option<(u32, u32)> {
    let mut fields = text[text.rfind(')')? + 1..].split_whitespace();
    let _state = fields.next()?;
    Some((fields.next()?.parse().ok()?, fields.next()?.parse().ok()?))
}

fn proc_stat(pid: u32) -> Option<(u32, u32)> {
    parse_stat(&fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

/// The process group `rm --force` kills for a running run, checked against
/// the live process table (`stat`: pid → `(ppid, pgrp)`): `meta` is written
/// by the run's user, so its numbers alone must not pick what gets killed.
/// The command's group (`pgid`) while its leader is still the supervisor's
/// child leading it; for a supervisor from before `pgid` (a `pid` without
/// one: the command shares the supervisor's group) or one that never got to
/// spawn, the supervisor's own group (it leads one: `start`). `None`: nothing
/// verifiably the run's (e.g. the command just ended).
fn kill_target(meta: &Meta, stat: impl Fn(u32) -> Option<(u32, u32)>) -> Option<u32> {
    let sup = meta.supervisor?;
    let target = match meta.pgid {
        Some(pgid) => (stat(pgid) == Some((sup, pgid))).then_some(pgid),
        None => (stat(sup).map(|(_, pgrp)| pgrp) == Some(sup)).then_some(sup),
    }?;
    // `kill(-1)` would signal everything we may, `kill(0)` our own group.
    (target > 1).then_some(target)
}

/// SIGKILL process group `pgid` (> 1). libc's `kill` directly: std links it
/// anyway (musl, static), unlike `/bin/kill`, which a minimal image may lack
/// and whose group syntax differs between procps, util-linux and busybox. A
/// group that's already gone is fine.
fn kill_group(pgid: u32) -> io::Result<()> {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    const SIGKILL: i32 = 9;
    const ESRCH: i32 = 3;
    let pgid = i32::try_from(pgid).map_err(|_| other(format!("bad process group {pgid}")))?;
    // SAFETY: a plain syscall wrapper; no memory is shared.
    if unsafe { kill(-pgid, SIGKILL) } != 0 {
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(ESRCH) {
            return Err(e);
        }
    }
    Ok(())
}

/// `run prune`: delete ended (`exited`/`killed`) and `lost` runs but the
/// newest `keep` of them ([`run_ids`] order); running runs and ones whose
/// `meta` can't be read are left alone. Returns how many were deleted. One
/// that can't be (another user's, in the sticky dir) doesn't stop the rest,
/// but makes the result an error naming it.
fn prune(root: &Path, keep: usize) -> io::Result<usize> {
    let ended: Vec<String> = run_ids(root)?
        .into_iter()
        .filter(|id| matches!(load_state(&root.join(id), id), Ok((_, s)) if s != State::Running))
        .collect();
    let (mut removed, mut failed) = (0, None);
    for id in &ended[..ended.len().saturating_sub(keep)] {
        match fs::remove_dir_all(root.join(id)) {
            Ok(()) => removed += 1,
            Err(e) => {
                failed.get_or_insert((id, e));
            }
        }
    }
    match failed {
        None => Ok(removed),
        Some((id, e)) => Err(io::Error::new(e.kind(), format!("removed {removed}; cannot remove run `{id}`: {e}"))),
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
        assert_eq!(p(&["rm", id]), Ok(Cmd::Rm { id: id.into(), force: false }));
        assert_eq!(p(&["rm", "--force", id]), Ok(Cmd::Rm { id: id.into(), force: true }));
        assert_eq!(p(&["prune"]), Ok(Cmd::Prune { keep: 0 }));
        assert_eq!(p(&["prune", "--keep", "5"]), Ok(Cmd::Prune { keep: 5 }));
        assert_eq!(p(&["prune", "--keep=0"]), Ok(Cmd::Prune { keep: 0 }));
        assert!(p(&["rm"]).unwrap_err().contains("exactly one"));
        assert!(p(&["rm", "../x"]).unwrap_err().contains("bad run id"));
        assert_eq!(p(&["rm", id, "--force=1"]), Err("--force takes no value".into()));
        assert!(p(&["rm", id, "--keep", "1"]).unwrap_err().contains("unknown option"));
        assert!(p(&["prune", "x"]).unwrap_err().contains("no arguments"));
        assert!(p(&["prune", "--keep", "-1"]).unwrap_err().contains("bad number"));
        assert!(p(&["prune", "--keep", "1", "--keep", "2"]).unwrap_err().contains("twice"));
        assert!(p(&["prune", "--force"]).unwrap_err().contains("unknown option"));

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
        assert!(local(&["rm", id]));
        assert!(local(&["rm", "--force", id]));
        assert!(!local(&["rm", "pr-1", id]));
        assert!(!local(&["rm", "pr-1", id, "--force", "--sandbox", "web"]));
        assert!(!local(&["rm", "--sandbox", "web", "pr-1", id]));
        assert!(local(&["prune"]));
        assert!(local(&["prune", "--keep", "5"]));
        assert!(local(&["prune", "--keep=5"]));
        assert!(!local(&["prune", "pr-1"]));
        assert!(!local(&["prune", "--keep", "5", "pr-1"]));
        assert!(!local(&["prune", "--sandbox", "web", "--keep", "5", "pr-1"]));
    }

    #[test]
    fn meta_parsing_and_state() {
        let m = parse_meta("started 100\nsupervisor 7\npid 8\npgid 8\nended 105\nexit 3\n");
        assert_eq!(
            m,
            Meta {
                started: Some(100),
                supervisor: Some(7),
                pid: Some(8),
                pgid: Some(8),
                ended: Some(105),
                exit: Some(State::Exited(3)),
            }
        );
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

    /// A finished run made by hand: `argv` and `meta` as given.
    fn fake_run(root: &Path, id: &str, argv: &str, meta: &str) {
        let dir = root.join(id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("argv"), argv).unwrap();
        fs::write(dir.join("meta"), meta).unwrap();
    }

    const DONE: &str = "started 1790000000\nended 1790000001\nexit 0\n";

    #[test]
    fn ls_lists_only_the_newest_runs() {
        let root = temp_root("ls-cap");
        for n in 0..60 {
            fake_run(&root, &format!("{:010}-0000", 1790000000 + n), "arg true\n", DONE);
        }
        let listed = ls(&root).unwrap();
        let ids: Vec<&str> = listed.lines().map(|l| l.split(' ').next().unwrap()).collect();
        assert_eq!(ids.len(), MAX_LS_RUNS);
        assert_eq!(ids.first(), Some(&"1790000010-0000"));
        assert_eq!(ids.last(), Some(&"1790000059-0000"));
        assert!(ids.windows(2).all(|w| w[0] < w[1]), "oldest first");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn ls_truncates_long_argv() {
        let root = temp_root("ls-argv");
        fake_run(&root, "1790000000-0000", &format!("arg {}\n", "x".repeat(500)), DONE);
        fake_run(&root, "1790000001-0000", &format!("arg {}\n", "y".repeat(MAX_LS_ARGV_CHARS)), DONE);
        let listed = ls(&root).unwrap();
        let lines: Vec<&str> = listed.lines().collect();
        assert!(lines[0].ends_with(&format!(" {}\u{2026}", "x".repeat(MAX_LS_ARGV_CHARS))), "{}", lines[0]);
        assert!(!lines[0].contains(&"x".repeat(MAX_LS_ARGV_CHARS + 1)));
        // Exactly at the limit: kept whole, no marker.
        assert!(lines[1].ends_with(&format!(" {}", "y".repeat(MAX_LS_ARGV_CHARS))), "{}", lines[1]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn unreadable_runs_are_skipped_or_refused() {
        let root = temp_root("ls-bad");
        fake_run(&root, "1790000000-0000", "arg ok\n", DONE);
        // Oversized argv / meta (padding lines a parser would ignore).
        let big_argv = "arg a\n".repeat(MAX_ARGV_BYTES as usize / 6 + 1);
        fake_run(&root, "1790000001-0000", &big_argv, DONE);
        let big_meta = format!("{DONE}{}", "pad x\n".repeat(MAX_META_BYTES as usize / 6 + 1));
        fake_run(&root, "1790000002-0000", "arg a\n", &big_meta);
        // `meta` that's a directory, and a run dir that's a symlink.
        let dir = root.join("1790000003-0000");
        fs::create_dir_all(dir.join("meta")).unwrap();
        fs::write(dir.join("argv"), "arg a\n").unwrap();
        std::os::unix::fs::symlink(root.join("1790000000-0000"), root.join("1790000004-0000")).unwrap();
        // A plain file with a run-id name.
        fs::write(root.join("1790000005-0000"), DONE).unwrap();

        let listed = ls(&root).unwrap();
        assert_eq!(listed.lines().count(), 1, "{listed}");
        assert!(listed.starts_with("1790000000-0000 exited 0 "), "{listed}");
        for id in ["1790000002-0000", "1790000003-0000", "1790000004-0000", "1790000005-0000"] {
            assert!(wait(&root, id, Duration::ZERO).is_err(), "{id}");
        }
        assert!(read_argv(&root.join("1790000001-0000")).is_err());
        assert!(supervise(&root, "1790000001-0000").is_err());
        assert!(read_chunk(&root, "1790000004-0000", 0, 1).is_err(), "symlinked run dir");
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

    /// A live stand-in supervisor for run `id`: its cmdline names the id
    /// (`; :` keeps sh from exec'ing sleep in its place). Recorded in `meta`.
    fn fake_supervisor(root: &Path, id: &str) -> std::process::Child {
        let sup = Command::new("sh").args(["-c", "sleep 30; :", id]).spawn().unwrap();
        append(&root.join(id), &format!("supervisor {}\n", sup.id())).unwrap();
        let t = Instant::now();
        while !is_supervisor(sup.id(), id) && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(10));
        }
        sup
    }

    #[test]
    fn rm_deletes_ended_runs_and_refuses_running_or_unreadable_ones() {
        let root = temp_root("rm");
        fake_run(&root, "1790000000-0000", "arg true\n", DONE);
        rm(&root, "1790000000-0000", false).unwrap();
        assert!(!root.join("1790000000-0000").exists());
        assert_eq!(rm(&root, "1790000000-0000", false).unwrap_err().kind(), io::ErrorKind::NotFound);

        // Running (a live supervisor, no command recorded): refused, kept.
        let id = create(&root, &strings(&["true"]), now()).unwrap();
        let mut sup = fake_supervisor(&root, &id);
        let err = rm(&root, &id, false).unwrap_err().to_string();
        assert!(err.contains("is running; `--force` kills it first"), "{err}");
        assert!(root.join(&id).is_dir());
        let _ = sup.kill();
        let _ = sup.wait();

        // State unknown (`meta` a directory): refused, `--force` deletes it.
        let bad = root.join("1790000001-0000");
        fs::create_dir_all(bad.join("meta")).unwrap();
        let err = rm(&root, "1790000001-0000", false).unwrap_err().to_string();
        assert!(err.contains("`--force` removes it anyway"), "{err}");
        rm(&root, "1790000001-0000", true).unwrap();
        assert!(!bad.exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn rm_and_prune_never_follow_a_symlinked_run_dir() {
        let root = temp_root("rm-link");
        let target = temp_root("rm-link-target");
        fake_run(&target, "1790000000-0000", "arg true\n", DONE);
        fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(target.join("1790000000-0000"), root.join("1790000000-0000")).unwrap();
        for force in [false, true] {
            assert_eq!(rm(&root, "1790000000-0000", force).unwrap_err().kind(), io::ErrorKind::NotFound);
        }
        assert_eq!(prune(&root, 0).unwrap(), 0);
        assert!(fs::symlink_metadata(root.join("1790000000-0000")).is_ok(), "link kept");
        assert!(target.join("1790000000-0000/meta").is_file(), "target untouched");
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&target);
    }

    #[test]
    fn prune_keeps_the_newest_ended_runs_and_never_running_ones() {
        let root = temp_root("prune");
        assert_eq!(prune(&root, 0).unwrap(), 0, "no root yet");
        fake_run(&root, "1790000000-0000", "arg a\n", DONE);
        fake_run(&root, "1790000001-0000", "arg a\n", "started 1790000001\nended 1790000002\nsignal 9\n");
        fake_run(&root, "1790000002-0000", "arg a\n", "started 1790000002\nsupervisor 4294967\n");
        fake_run(&root, "1790000003-0000", "arg a\n", "started 1790000003\n");
        let mut sup = fake_supervisor(&root, "1790000003-0000");
        fake_run(&root, "1790000004-0000", "arg a\n", DONE);
        fs::create_dir_all(root.join("1790000005-0000/meta")).unwrap();
        let left = || run_ids(&root).unwrap();

        // Ended: 0 (exited), 1 (killed), 2 (lost), 4 (exited); keep the newest 2.
        assert_eq!(prune(&root, 2).unwrap(), 2);
        assert_eq!(left(), ["1790000002-0000", "1790000003-0000", "1790000004-0000", "1790000005-0000"]);
        assert_eq!(prune(&root, 2).unwrap(), 0, "already at the limit");
        assert_eq!(prune(&root, 99).unwrap(), 0);
        assert_eq!(prune(&root, 0).unwrap(), 2);
        // The running run and the unreadable one stay.
        assert_eq!(left(), ["1790000003-0000", "1790000005-0000"]);
        let _ = sup.kill();
        let _ = sup.wait();
        // Its supervisor gone, the run is lost: prunable now.
        assert_eq!(prune(&root, 0).unwrap(), 1);
        assert_eq!(left(), ["1790000005-0000"]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn stat_parsing_and_kill_targets() {
        assert_eq!(parse_stat("42 (sleep) S 7 42 42 0 -1 4194560"), Some((7, 42)));
        assert_eq!(parse_stat("42 (a) b (c) R 7 40 40"), Some((7, 40)), "comm with `) `");
        assert_eq!(parse_stat("42 (x"), None);
        assert_eq!(parse_stat(""), None);

        let meta = |text: &str| parse_meta(text);
        // The command's group, only while its leader is the supervisor's child
        // leading that group.
        let stat = |pid: u32| match pid {
            50 => Some((7, 50)),
            60 => Some((1, 60)),
            70 => Some((7, 50)),
            7 => Some((1, 7)),
            8 => Some((1, 3)),
            _ => None,
        };
        assert_eq!(kill_target(&meta("supervisor 7\npid 50\npgid 50\n"), stat), Some(50));
        assert_eq!(kill_target(&meta("supervisor 7\npid 60\npgid 60\n"), stat), None, "not its child");
        assert_eq!(kill_target(&meta("supervisor 7\npid 70\npgid 70\n"), stat), None, "not a leader");
        assert_eq!(kill_target(&meta("supervisor 7\npid 99\npgid 99\n"), stat), None, "gone");
        // Older supervisor (no pgid): its own group, if it leads one.
        assert_eq!(kill_target(&meta("supervisor 7\npid 50\n"), stat), Some(7));
        assert_eq!(kill_target(&meta("supervisor 8\npid 50\n"), stat), None);
        assert_eq!(kill_target(&meta("started 1\n"), stat), None, "no supervisor yet");
        // Planted 0/1 never reach `kill`.
        let all = |pid: u32| Some((pid, pid));
        assert_eq!(kill_target(&meta("supervisor 1\npid 1\n"), all), None);
        assert_eq!(kill_target(&meta("supervisor 0\npid 0\npgid 0\n"), all), None);
        assert_eq!(kill_target(&meta("supervisor 1\npid 1\npgid 1\n"), all), None);
    }

    /// Not a test by itself: the supervisor process of
    /// `rm_force_kills_a_real_runs_group` (run from this test binary, which
    /// isn't devsbd), over the root and id in its env.
    #[test]
    #[ignore]
    fn supervisor_process() {
        let (Ok(root), Ok(id)) = (std::env::var("DEVSBD_TEST_RUNS_ROOT"), std::env::var("DEVSBD_TEST_RUN_ID")) else {
            return;
        };
        supervise(Path::new(&root), &id).unwrap();
    }

    /// Whether `pid` is a live process (a zombie isn't: an orphan's may sit
    /// unreaped under a non-reaping PID 1).
    fn alive(pid: u32) -> bool {
        fs::read_to_string(format!("/proc/{pid}/stat"))
            .is_ok_and(|s| s.rfind(')').is_some_and(|i| !s[i + 1..].trim_start().starts_with('Z')))
    }

    /// Members of process group `pgid`, from `/proc`.
    fn group_members(pgid: u32) -> Vec<u32> {
        fs::read_dir("/proc")
            .unwrap()
            .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<u32>().ok())
            .filter(|&pid| proc_stat(pid).is_some_and(|(_, g)| g == pgid) && alive(pid))
            .collect()
    }

    /// The real start → supervise path (a separate supervisor process, the
    /// command in its own group with a forked grandchild): `rm` refuses,
    /// `rm --force` kills the whole group, lets the supervisor record the
    /// kill, and deletes the run.
    #[test]
    fn rm_force_kills_a_real_runs_group() {
        let root = temp_root("rm-kill");
        let exe = std::env::current_exe().unwrap();
        let argv = strings(&["sh", "-c", "sleep 300 & sleep 300; :"]);
        let id = start_with(&root, &argv, None, |id| {
            let mut cmd = Command::new(&exe);
            cmd.args(["--exact", "--ignored", "runs::tests::supervisor_process", id])
                .env("DEVSBD_TEST_RUNS_ROOT", &root)
                .env("DEVSBD_TEST_RUN_ID", id);
            cmd
        })
        .unwrap();
        let dir = root.join(&id);
        let t = Instant::now();
        let pgid = loop {
            let meta = parse_meta(&fs::read_to_string(dir.join("meta")).unwrap());
            match meta.pgid {
                Some(pgid) if group_members(pgid).len() >= 3 => break pgid,
                _ if t.elapsed() > Duration::from_secs(10) => panic!("no command group: {meta:?}"),
                _ => std::thread::sleep(Duration::from_millis(20)),
            }
        };
        let members = group_members(pgid);
        let meta = parse_meta(&fs::read_to_string(dir.join("meta")).unwrap());
        assert_eq!(meta.pid, Some(pgid), "the command leads its group");
        assert!(!members.contains(&meta.supervisor.unwrap()), "the supervisor is outside it");

        let err = rm(&root, &id, false).unwrap_err().to_string();
        assert!(err.contains("is running"), "{err}");
        assert!(members.iter().all(|&p| alive(p)));

        let result = rm(&root, &id, true);
        let t = Instant::now();
        while members.iter().any(|&p| alive(p)) && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        let survivors: Vec<u32> = members.iter().copied().filter(|&p| alive(p)).collect();
        let _ = kill_group(pgid);
        result.unwrap();
        assert!(survivors.is_empty(), "still alive: {survivors:?} of {members:?}");
        assert!(!dir.exists());
        let _ = fs::remove_dir_all(&root);
    }
}
