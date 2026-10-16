//! Per-instance build logs (docs/architecture/instances/images-and-features.md):
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
//! `devsandbox serve`'s autostart pass calls these commands in-process, where
//! fds 1/2 belong to `serve.log` and other threads write there too. The
//! dashboard's `run`/`rebuild`/`start` are verb requests the daemon runs as
//! `devsandbox` subprocesses, so they log.
//!
//! On a terminal the tee is a pty (docs/architecture/instances/images-and-features.md, *Drift and logs*):
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
/// `tty` (docs/reference/api.md), passed to the verb as [`TTY_ENV`].
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
/// A pty log (`# tty` header) is replayed ([`Replay`]): `ansi` keeps its
/// colours as SGR escapes, else the lines are plain text. A pipe log reads
/// the same either way.
#[cfg_attr(not(unix), allow(dead_code))] // only the daemon (unix) uses it
pub fn tail_in(data: &Path, instance_id: &str, lines: usize, ansi: bool) -> Result<Option<Tail>> {
    tail_at(&dir_in(data, instance_id).join(FILE), lines, ansi)
}

/// Bytes read off a log's start for its header: the command line can be
/// long, the rest is short.
const HEAD_BYTES: usize = 4096;

/// The pty size a build log was written at and its header's length, from
/// the log's first bytes; `None` for a pipe log or an older one.
pub fn tty_header(head: &[u8]) -> Option<(TtySize, usize)> {
    let mut lines = head.split_inclusive(|&b| b == b'\n').filter(|l| l.ends_with(b"\n"));
    let (cmd, when, tty) = (lines.next()?, lines.next()?, lines.next()?);
    if !cmd.starts_with(b"$ ") || !when.starts_with(b"# ") {
        return None;
    }
    let size = TtySize::parse(std::str::from_utf8(tty.strip_prefix(b"# tty ")?).ok()?)?;
    Some((size, cmd.len() + when.len() + tty.len()))
}

/// The first [`HEAD_BYTES`] of `file` (from its current position).
pub fn read_head(file: &mut File) -> std::io::Result<Vec<u8>> {
    let mut head = Vec::new();
    Read::by_ref(file).take(HEAD_BYTES as u64).read_to_end(&mut head)?;
    Ok(head)
}

fn tail_at(path: &Path, lines: usize, ansi: bool) -> Result<Option<Tail>> {
    let mut file = match File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("cannot open {}", path.display())),
    };
    let read = |file: &mut File| -> std::io::Result<(Option<(TtySize, usize)>, Vec<u8>, u64)> {
        let tty = tty_header(&read_head(file)?);
        let mut start = file.metadata()?.len().saturating_sub(TAIL_BYTES);
        if tty.is_some_and(|(_, header)| start < header as u64) {
            start = 0; // the header comes whole or not at all
        }
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok((tty, bytes, start))
    };
    let (tty, bytes, start) = read(&mut file).with_context(|| format!("cannot read {}", path.display()))?;
    let Some((size, header)) = tty else {
        return Ok(Some(last_lines(&String::from_utf8_lossy(&bytes), lines, start > 0)));
    };
    Ok(Some(replay_tail(&bytes, size, header, start > 0, lines, ansi)))
}

/// [`Tail`] of a pty log read from mid-file (`partial`) or from its start,
/// where its first `header` bytes are the header (kept as they are).
fn replay_tail(bytes: &[u8], size: TtySize, header: usize, partial: bool, n: usize, ansi: bool) -> Tail {
    let mut kept = std::collections::VecDeque::with_capacity(n.min(1024));
    let mut total = 0usize;
    let mut keep = |line: String| {
        total += 1;
        if kept.len() == n {
            kept.pop_front();
        }
        if n > 0 {
            kept.push_back(line);
        }
    };
    let body = if partial {
        // Its first line may be cut short (or start mid-escape): dropped,
        // like a plain log's.
        bytes.iter().position(|&b| b == b'\n').map_or(&[][..], |i| &bytes[i + 1..])
    } else {
        let header = header.min(bytes.len());
        String::from_utf8_lossy(&bytes[..header]).lines().for_each(|l| keep(l.to_string()));
        &bytes[header..]
    };
    let mut replay = Replay::new(size, ansi);
    replay.feed(body, &mut keep);
    replay.finish(&mut keep);
    let cut = partial || total > kept.len();
    Tail { text: Vec::from(kept).join("\n"), cut }
}

/// Bytes [`Replay`] hands vt100 at a time; after each, the rows that
/// scrolled off the screen go out. Small enough that a chunk's rows (one
/// per `\r\n` at most, short of explicit scrolls) stay cheap to hold.
const REPLAY_CHUNK: usize = 8 * 1024;

/// A pty log's body played through a terminal emulator (`vt100`) the size
/// it was written at, so progress displays that redraw in place (docker
/// buildkit's cursor-up block) come out as their final state, not as every
/// frame. Rows go out as they become final: once scrolled off the screen,
/// then the screen's own at [`finish`](Self::finish). Trailing blank rows
/// never go out. Each row is one line (a wrapped line stays split as the
/// terminal showed it), trailing spaces trimmed; with `ansi` it carries
/// SGR escapes only (built from the cells' attributes, no cursor moves),
/// and stripping them gives the plain row.
///
/// vt100 can't drop scrollback, so after each chunk the parser is swapped
/// for a fresh one fed the old screen's state (`state_formatted`): memory
/// stays bounded by a chunk whatever the log's length. What that state
/// lacks (a scroll region, a saved cursor) is lost at the swap; chunks end
/// at a line end when they can, so a swap rarely splits an escape.
pub struct Replay {
    parser: vt100::Parser,
    size: TtySize,
    ansi: bool,
    /// Normalized input not yet played.
    pending: Vec<u8>,
    /// The last byte fed was `\r`.
    cr: bool,
    /// Blank rows held back until a non-blank one follows.
    blanks: usize,
}

impl Replay {
    pub fn new(size: TtySize, ansi: bool) -> Replay {
        Replay { parser: Replay::parser(size), size, ansi, pending: Vec::new(), cr: false, blanks: 0 }
    }

    fn parser(size: TtySize) -> vt100::Parser {
        // Room for every row one chunk can scroll off (a row takes `\r\n`).
        vt100::Parser::new(size.rows, size.cols, REPLAY_CHUNK)
    }

    /// Play `bytes`, handing finished rows to `out`. A bare `\n` becomes
    /// `\r\n`: the pty's own output has ONLCR's, but what's written to
    /// the log after the tee (`error: …` at the end) has none.
    pub fn feed(&mut self, bytes: &[u8], out: &mut impl FnMut(String)) {
        for &b in bytes {
            if b == b'\n' && !self.cr {
                self.pending.push(b'\r');
            }
            self.pending.push(b);
            self.cr = b == b'\r';
        }
        while self.pending.len() >= REPLAY_CHUNK {
            let window = &self.pending[..REPLAY_CHUNK];
            let end = window.iter().rposition(|&b| b == b'\n').map_or(REPLAY_CHUNK, |i| i + 1);
            let chunk: Vec<u8> = self.pending.drain(..end).collect();
            self.play(&chunk, out);
        }
    }

    /// Play what's left and hand out the screen's rows.
    pub fn finish(mut self, out: &mut impl FnMut(String)) {
        let rest = std::mem::take(&mut self.pending);
        self.play(&rest, out);
        let rows = self.size.rows;
        for r in 0..rows {
            let line = row_text(self.parser.screen(), r, self.ansi);
            self.emit(line, out);
        }
    }

    fn play(&mut self, chunk: &[u8], out: &mut impl FnMut(String)) {
        self.parser.process(chunk);
        let screen = self.parser.screen_mut();
        screen.set_scrollback(usize::MAX);
        let scrolled = screen.scrollback();
        if scrolled == 0 {
            return;
        }
        let rows = usize::from(self.size.rows);
        let mut lines = Vec::with_capacity(scrolled);
        // The view shows `rows` rows from scrollback row `top` on.
        for top in (0..scrolled).step_by(rows) {
            screen.set_scrollback(scrolled - top);
            for r in 0..rows.min(scrolled - top) {
                lines.push(row_text(screen, r as u16, self.ansi));
            }
        }
        screen.set_scrollback(0);
        let state = screen.state_formatted();
        self.parser = Replay::parser(self.size);
        self.parser.process(&state);
        for line in lines {
            self.emit(line, out);
        }
    }

    fn emit(&mut self, line: String, out: &mut impl FnMut(String)) {
        if line.is_empty() {
            self.blanks += 1;
            return;
        }
        for _ in 0..std::mem::take(&mut self.blanks) {
            out(String::new());
        }
        out(line);
    }
}

/// Visible row `r` of `screen` as text, trailing blanks trimmed; with
/// `ansi`, SGR escapes where the attributes change (and a reset at the end
/// when any are on).
fn row_text(screen: &vt100::Screen, r: u16, ansi: bool) -> String {
    let (_, cols) = screen.size();
    let mut cells: Vec<(&str, Sgr)> = Vec::with_capacity(usize::from(cols));
    for c in 0..cols {
        let Some(cell) = screen.cell(r, c) else { break };
        if cell.is_wide_continuation() {
            continue;
        }
        let text = if cell.has_contents() { cell.contents() } else { " " };
        cells.push((text, Sgr::of(cell)));
    }
    while cells.last().is_some_and(|(text, _)| text.trim().is_empty()) {
        cells.pop();
    }
    let mut line = String::new();
    let mut on = Sgr::default();
    for (text, sgr) in cells {
        if ansi && sgr != on {
            sgr.write(&mut line);
            on = sgr;
        }
        line.push_str(text);
    }
    if ansi && on != Sgr::default() {
        line.push_str("\x1b[0m");
    }
    line
}

/// A cell's drawing attributes, as SGR writes them.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Sgr {
    fg: vt100::Color,
    bg: vt100::Color,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
}

impl Sgr {
    fn of(cell: &vt100::Cell) -> Sgr {
        Sgr {
            fg: cell.fgcolor(),
            bg: cell.bgcolor(),
            bold: cell.bold(),
            dim: cell.dim(),
            italic: cell.italic(),
            underline: cell.underline(),
            inverse: cell.inverse(),
        }
    }

    /// `ESC[0;…m`: from a reset, so it never depends on what was on.
    fn write(&self, out: &mut String) {
        use std::fmt::Write as _;
        out.push_str("\x1b[0");
        for (on, code) in [(self.bold, 1), (self.dim, 2), (self.italic, 3), (self.underline, 4), (self.inverse, 7)] {
            if on {
                let _ = write!(out, ";{code}");
            }
        }
        for (color, base) in [(self.fg, 30u8), (self.bg, 40)] {
            let _ = match color {
                vt100::Color::Default => Ok(()),
                vt100::Color::Idx(n) if n < 8 => write!(out, ";{}", base + n),
                vt100::Color::Idx(n) if n < 16 => write!(out, ";{}", base + 60 + n - 8),
                vt100::Color::Idx(n) => write!(out, ";{};5;{n}", base + 8),
                vt100::Color::Rgb(r, g, b) => write!(out, ";{};2;{r};{g};{b}", base + 8),
            };
        }
        out.push('m');
    }
}

/// Every line of a pty log's body (after its `header` bytes) replayed
/// plain ([`Replay`]) to `out`, the whole file in [`REPLAY_CHUNK`]s
/// (`logs --build` off a terminal). The header's lines go out as they are.
pub fn replay_file(file: &mut File, size: TtySize, header: usize, out: &mut impl Write) -> std::io::Result<()> {
    file.seek(SeekFrom::Start(0))?;
    let mut head = vec![0; header];
    file.read_exact(&mut head)?;
    out.write_all(&head)?;
    let mut result = Ok(());
    let mut put = |line: String| {
        if result.is_ok() {
            result = writeln!(out, "{line}");
        }
    };
    let mut replay = Replay::new(size, false);
    let mut buf = vec![0; REPLAY_CHUNK];
    loop {
        let n = match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        replay.feed(&buf[..n], &mut put);
    }
    replay.finish(&mut put);
    result
}

/// Escape sequences dropped from a stream as it comes (`logs --build
/// --follow` off a terminal, where a pty log can't be replayed before it
/// ends): CSI, OSC and other string sequences, two- and three-byte `ESC`
/// ones. `\r\n` becomes `\n`; a lone `\r` (a redraw) a line break, so
/// redrawn lines repeat. Other C0 controls but `\t` go too.
#[derive(Default)]
pub struct Strip {
    state: StripState,
    cr: bool,
}

#[derive(Default, Clone, Copy)]
enum StripState {
    #[default]
    Ground,
    Esc,
    Csi,
    /// OSC, DCS, SOS, PM, APC: up to BEL or `ESC \`.
    Str,
    StrEsc,
}

impl Strip {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        use StripState::*;
        let mut out = Vec::with_capacity(bytes.len());
        for &b in bytes {
            self.state = match self.state {
                Ground => {
                    if std::mem::take(&mut self.cr) && b != b'\n' {
                        out.push(b'\n');
                    }
                    match b {
                        0x1b => Esc,
                        b'\r' => {
                            self.cr = true;
                            Ground
                        }
                        b'\n' | b'\t' => {
                            out.push(b);
                            Ground
                        }
                        0..=0x1f | 0x7f => Ground,
                        _ => {
                            out.push(b);
                            Ground
                        }
                    }
                }
                Esc => match b {
                    b'[' => Csi,
                    b']' | b'P' | b'X' | b'^' | b'_' => Str,
                    0x20..=0x2f => Esc, // an intermediate: one more byte
                    _ => Ground,
                },
                Csi => if (0x40..=0x7e).contains(&b) { Ground } else { Csi },
                Str => match b {
                    0x07 => Ground,
                    0x1b => StrEsc,
                    _ => Str,
                },
                StrEsc => if b == b'\\' { Ground } else { Str },
            };
        }
        out
    }
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

    const SIZE: TtySize = TtySize { cols: 40, rows: 5 };

    /// Every line [`Replay`] hands out for `bytes` fed in one go.
    fn replay(bytes: &[u8], ansi: bool) -> Vec<String> {
        let mut lines = Vec::new();
        let mut put = |l: String| lines.push(l);
        let mut r = Replay::new(SIZE, ansi);
        r.feed(bytes, &mut put);
        r.finish(&mut put);
        lines
    }

    #[test]
    fn tty_header_reads_the_size_off_the_third_line() {
        let head = b"$ devsandbox run\n# 2026-10-15T19:38:53Z\n# tty 120x30\nbody\r\n";
        assert_eq!(tty_header(head), Some((TtySize { cols: 120, rows: 30 }, head.len() - 6)));
        assert_eq!(tty_header(b"$ devsandbox run\n# 2026-10-15T19:38:53Z\nbody\n"), None, "a pipe log");
        assert_eq!(tty_header(b"$ devsandbox run\n# 2026\n# tty 120x30"), None, "unfinished");
        assert_eq!(tty_header(b"x\n# 2026\n# tty 120x30\n"), None);
        assert_eq!(tty_header(b""), None);
    }

    #[test]
    fn a_redraw_collapses_to_its_final_state() {
        // Buildkit-style: a block drawn, then the cursor moved up over it and
        // the block drawn again, longer.
        let mut log = b"Preparing worktree\r\n".to_vec();
        log.extend_from_slice(b"\x1b[?25l[+] Building 0.0s (0/2)\r\n => step a 0.0s\r\n\x1b[?25h");
        log.extend_from_slice(b"\x1b[2A\x1b[0G\x1b[?25l[+] Building 1.2s (2/2) FINISHED\r\n => step a 0.1s\r\n => step b 1.0s\r\n");
        assert_eq!(
            replay(&log, false),
            ["Preparing worktree", "[+] Building 1.2s (2/2) FINISHED", " => step a 0.1s", " => step b 1.0s"]
        );
        // `\r` progress on one line keeps its last state too.
        assert_eq!(replay(b"step 1/3\rstep 3/3\r\ndone\r\n", false), ["step 3/3", "done"]);
    }

    #[test]
    fn replay_keeps_colours_as_sgr_only_with_ansi() {
        let log = b"\x1b[34m => blue\x1b[0m plain \x1b[1;38;5;200mpink\x1b[0m\r\n\x1b[48;2;1;2;3mrgb\x1b[m\r\n";
        assert_eq!(replay(log, false), [" => blue plain pink", "rgb"]);
        let ansi = replay(log, true);
        assert_eq!(ansi, ["\x1b[0;34m => blue\x1b[0m plain \x1b[0;1;38;5;200mpink\x1b[0m", "\x1b[0;48;2;1;2;3mrgb\x1b[0m"]);
        let bright = replay(b"\x1b[91;104mx\r\n", true);
        assert_eq!(bright, ["\x1b[0;91;104mx\x1b[0m"]);
    }

    #[test]
    fn replay_drops_trailing_blank_rows_and_scrolls_rows_out_in_order() {
        // More rows than the screen: the first ones scroll off and still come
        // out, in order; blank rows between lines stay, trailing ones go.
        let log: String = (0..12).map(|i| if i == 3 { "\r\n".to_string() } else { format!("l{i}\r\n") }).collect();
        let want: Vec<String> = (0..12).map(|i| if i == 3 { String::new() } else { format!("l{i}") }).collect();
        assert_eq!(replay(format!("{log}\r\n\r\n").as_bytes(), false), want);
        // A bare `\n` (the error line written after the tee) starts a line.
        assert_eq!(replay(b"a\r\nerror: boom\nnext\n", false), ["a", "error: boom", "next"]);
    }

    #[test]
    fn replay_survives_many_chunks() {
        // Far more than one chunk, fed in odd pieces: every line, in order,
        // and colours still on after a parser swap mid-colour.
        let mut log = b"\x1b[32m".to_vec();
        for i in 0..5000 {
            log.extend_from_slice(format!("line {i}\r\n").as_bytes());
        }
        log.extend_from_slice(b"\x1b[0mend\r\n");
        let mut lines = Vec::new();
        let mut put = |l: String| lines.push(l);
        let mut r = Replay::new(SIZE, true);
        for piece in log.chunks(777) {
            r.feed(piece, &mut put);
        }
        r.finish(&mut put);
        assert_eq!(lines.len(), 5001);
        assert_eq!(lines[0], "\x1b[0;32mline 0\x1b[0m");
        assert_eq!(lines[4999], "\x1b[0;32mline 4999\x1b[0m");
        assert_eq!(lines[5000], "end");
    }

    fn tty_log(dir: &Path, body: &[u8]) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(FILE);
        let mut bytes = header(&["run".into()], 0, Some(SIZE)).into_bytes();
        bytes.extend_from_slice(body);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn tail_replays_a_pty_log_and_reads_a_pipe_log_as_lines() {
        let dir = temp_dir("tail");
        let path = tty_log(&dir, b"\x1b[31mred\x1b[0m\r\n50%\r100%\r\n");
        let head = "$ devsandbox run\n# 1970-01-01T00:00:00Z\n# tty 40x5";
        let plain = tail_at(&path, 10, false).unwrap().unwrap();
        assert_eq!(plain, Tail { text: format!("{head}\nred\n100%"), cut: false });
        let ansi = tail_at(&path, 10, true).unwrap().unwrap();
        assert_eq!(ansi.text, format!("{head}\n\x1b[0;31mred\x1b[0m\n100%"));
        assert_eq!(tail_at(&path, 2, false).unwrap().unwrap(), Tail { text: "red\n100%".into(), cut: true });
        // Without the tty line: lines as before, escapes and all, either way.
        let pipe = "$ devsandbox run\n# 1970-01-01T00:00:00Z\n\x1b[31mred\x1b[0m\n50%\r100%\n";
        std::fs::write(&path, pipe).unwrap();
        let want = Tail { text: "$ devsandbox run\n# 1970-01-01T00:00:00Z\n\x1b[31mred\x1b[0m\n100%".into(), cut: false };
        assert_eq!(tail_at(&path, 10, false).unwrap().unwrap(), want);
        assert_eq!(tail_at(&path, 10, true).unwrap().unwrap(), want);
        assert_eq!(tail_at(&dir.join("none"), 10, true).unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_long_pty_log_is_replayed_from_mid_file_even_mid_escape() {
        let dir = temp_dir("tail-long");
        // Each line coloured, so the tail's cut lands inside some escape.
        let mut body = Vec::new();
        for i in 0..40_000 {
            body.extend_from_slice(format!("\x1b[38;2;10;20;30mline {i}\x1b[0m\r\n").as_bytes());
        }
        let path = tty_log(&dir, &body);
        let tail = tail_at(&path, 3, false).unwrap().unwrap();
        assert_eq!(tail, Tail { text: "line 39997\nline 39998\nline 39999".into(), cut: true });
        // Its first (partial) line is dropped, as for a plain log.
        let tail = tail_at(&path, 100_000, false).unwrap().unwrap();
        let first = tail.text.lines().next().unwrap();
        let n: usize = first.strip_prefix("line ").unwrap().parse().unwrap();
        assert_eq!(tail.text.lines().count(), 40_000 - n, "{first}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replay_file_prints_the_whole_log_plain() {
        let dir = temp_dir("replay-file");
        let mut body = Vec::new();
        for i in 0..3000 {
            body.extend_from_slice(format!("\x1b[1mline {i}\x1b[0m\r\n").as_bytes());
        }
        body.extend_from_slice(b"\x1b[1A\x1b[0Glast\x1b[K\r\n");
        let path = tty_log(&dir, &body);
        let mut file = File::open(&path).unwrap();
        let (size, header_len) = tty_header(&read_head(&mut file).unwrap()).unwrap();
        let mut out = Vec::new();
        replay_file(&mut file, size, header_len, &mut out).unwrap();
        let out = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[..3], ["$ devsandbox run", "# 1970-01-01T00:00:00Z", "# tty 40x5"]);
        assert_eq!(lines[3], "line 0");
        assert_eq!(lines[3 + 2998], "line 2998");
        assert_eq!(lines[3 + 2999], "last", "the redraw over line 2999");
        assert_eq!(lines.len(), 3 + 3000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn strip_drops_escapes_as_they_come() {
        let mut s = Strip::default();
        let mut out = Vec::new();
        // Split mid-escape and between `\r` and `\n`.
        for piece in [&b"\x1b[34m => a\x1b"[..], b"[0m\r", b"\n\x1b[1A\x1b[0G\x1b[?25lb\x1b]0;title\x07c\x1b]8;;u\x1b\\d\x1b(Be\r", b"f\r\n"] {
            out.extend(s.feed(piece));
        }
        assert_eq!(String::from_utf8(out).unwrap(), " => a\nbcde\nf\n");
        assert_eq!(Strip::default().feed(b"tab\there\x07\x08!"), b"tab\there!");
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
