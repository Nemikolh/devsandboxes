//! PTY-backed terminal sessions for the dashboard's integrated terminal.
//!
//! A [`TermSession`] owns one child shell attached to a pseudo-terminal: the
//! PTY byte stream is fed into a [`vt100::Parser`] on a background reader
//! thread, so the ratatui event loop only has to render the parsed screen and
//! forward key bytes. The parser's callbacks emulate the kitty keyboard
//! protocol ([`super::kitty`]); the reader writes its query replies back. Everything here is deliberately self-contained so the
//! app state machine ([`super::app`]) can stay I/O-free and unit-testable.
//!
//! [`TermTabs`] holds the open sessions as a pure tab strip (active index,
//! open/dedup/next/prev/close); [`super::app`] drives it and forwards keys.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use super::kitty::KittyState;

/// The session's parsed screen, with the kitty protocol state riding along as
/// the parser's callbacks.
pub type TermParser = vt100::Parser<KittyState>;

/// PTY write half, shared between the UI thread (keys) and the reader thread
/// (replies to the child's terminal queries).
type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

/// vt100 scrollback retained per session, in lines. Generous enough that a
/// `cargo build` or a `git log` stays scrollable without unbounded growth.
const SCROLLBACK: usize = 5000;

/// One integrated-terminal tab: a child shell on a PTY, its parsed screen, and
/// the flags the event loop polls. The reader thread owns the read half; the
/// UI thread writes keys and resizes.
pub struct TermSession {
    /// Tab label: instance name, or a service container name minus the
    /// `devsandbox-` prefix. Read by the renderer's tab strip.
    pub title: String,
    /// Container this shell runs in (the resolved `devsandbox-*` name), kept so
    /// the app can dedup `t` against an already-open terminal for the target.
    pub container: String,
    /// Shared parsed screen. The reader thread feeds bytes in; the UI thread
    /// reads it out to render. `Arc<Mutex<…>>` because both threads touch it.
    parser: Arc<Mutex<TermParser>>,
    /// PTY master. `None` in the test constructor (no real PTY). Kept so we can
    /// `resize()` the kernel winsize as the pane changes size.
    master: Option<Box<dyn MasterPty + Send>>,
    /// Write half of the PTY: key bytes go here. In tests this is an in-memory
    /// buffer so key encoding/forwarding can be asserted without a child.
    writer: SharedWriter,
    /// The test writer's buffer, read back by [`Self::take_written`].
    #[cfg(test)]
    written: Arc<Mutex<Vec<u8>>>,
    /// Child handle, kept so [`Drop`] can kill+wait it. `None` in tests.
    child: Option<Box<dyn Child + Send + Sync>>,
    /// Set by the reader thread once the PTY hits EOF (shell exited / container
    /// stopped). The tab then renders as `(exited)` and swallows keys.
    exited: Arc<AtomicBool>,
    /// Current `(rows, cols)`; `resize` is a no-op when unchanged.
    size: (u16, u16),
}

impl TermSession {
    /// Spawn `argv` (the *full* runtime command, e.g. `docker exec -it …`) on a
    /// fresh PTY sized `rows`×`cols`, wire a reader thread that pumps output
    /// into the vt100 parser, and return the live session. `kitty`: whether
    /// the outer terminal speaks the kitty keyboard protocol, i.e. whether to
    /// offer it to the child.
    pub fn spawn(
        title: String,
        container: String,
        argv: Vec<String>,
        rows: u16,
        cols: u16,
        kitty: bool,
    ) -> Result<TermSession> {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("failed to open pty")?;

        let mut cmd = CommandBuilder::from_argv(argv.into_iter().map(Into::into).collect());
        // This TERM only reaches the runtime client (`docker exec …`) on the
        // host: `exec` doesn't forward the caller's environment, so the shell in
        // the container gets the runtime's default (`xterm` on docker) or
        // whatever the image / `remoteEnv` sets.
        cmd.env("TERM", "xterm-256color");
        let child = pair
            .slave
            .spawn_command(cmd)
            .context("failed to spawn shell in pty")?;
        // Drop the slave so only the child holds it; without this the PTY never
        // reports EOF when the child exits and the reader thread hangs forever.
        drop(pair.slave);

        let reader = pair
            .master
            .try_clone_reader()
            .context("failed to clone pty reader")?;
        let writer: SharedWriter = Arc::new(Mutex::new(
            pair.master
                .take_writer()
                .context("failed to take pty writer")?,
        ));

        let parser = Arc::new(Mutex::new(new_parser(rows, cols, kitty)));
        let exited = Arc::new(AtomicBool::new(false));

        spawn_reader(reader, parser.clone(), writer.clone(), exited.clone());

        Ok(TermSession {
            title,
            container,
            parser,
            master: Some(pair.master),
            writer,
            #[cfg(test)]
            written: Arc::default(),
            child: Some(child),
            exited,
            size: (rows, cols),
        })
    }

    /// Shared screen, for the renderer.
    pub fn parser(&self) -> &Arc<Mutex<TermParser>> {
        &self.parser
    }

    /// Propagate a new pane size to the kernel winsize and the parser. No-op
    /// when unchanged so we don't churn on every redraw.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        if self.size == (rows, cols) {
            return;
        }
        if let Some(master) = &self.master {
            let _ = master.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
        if let Ok(mut parser) = self.parser.lock() {
            parser.screen_mut().set_size(rows, cols);
        }
        self.size = (rows, cols);
    }

    /// Forward raw key bytes to the shell. Errors are ignored: once the child
    /// has exited the write fails benignly, and there is nothing to recover.
    pub fn write_key_bytes(&mut self, bytes: &[u8]) {
        write_pty(&self.writer, bytes);
    }

    /// Whether the child has exited (PTY EOF seen by the reader thread).
    pub fn exited(&self) -> bool {
        self.exited.load(Ordering::Relaxed)
    }

    /// Current pane size as last set by [`resize`](Self::resize).
    pub fn size(&self) -> (u16, u16) {
        self.size
    }

    /// Test constructor: an in-memory writer, no PTY, no child. Lets tab and
    /// key-forwarding logic be exercised without a container runtime. Shared with
    /// [`super::app`]'s tests via `pub(crate)`. Kitty emulation is on, as
    /// behind a supporting outer terminal.
    #[cfg(test)]
    pub(crate) fn test_session(title: &str, container: &str, rows: u16, cols: u16) -> TermSession {
        let written: Arc<Mutex<Vec<u8>>> = Arc::default();
        TermSession {
            title: title.into(),
            container: container.into(),
            parser: Arc::new(Mutex::new(new_parser(rows, cols, true))),
            master: None,
            writer: Arc::new(Mutex::new(Box::new(TestWriter(written.clone())))),
            written,
            child: None,
            exited: Arc::new(AtomicBool::new(false)),
            size: (rows, cols),
        }
    }

    /// Test helper: flip the exited flag, standing in for the reader thread's
    /// EOF handling so key-swallowing and dedup can be exercised deterministically.
    #[cfg(test)]
    pub(crate) fn set_exited(&self) {
        self.exited.store(true, Ordering::Relaxed);
    }

    /// Test helper: drain what has been written to the PTY so far.
    #[cfg(test)]
    pub(crate) fn take_written(&self) -> Vec<u8> {
        std::mem::take(&mut *self.written.lock().unwrap())
    }

    /// Test helper: feed child output through the reader thread's path (parse,
    /// then answer queries) without a PTY.
    #[cfg(test)]
    pub(crate) fn feed_output(&self, bytes: &[u8]) {
        process_output(&self.parser, &self.writer, bytes);
    }
}

/// In-memory PTY stand-in for [`TermSession::test_session`].
#[cfg(test)]
struct TestWriter(Arc<Mutex<Vec<u8>>>);

#[cfg(test)]
impl Write for TestWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn new_parser(rows: u16, cols: u16, kitty: bool) -> TermParser {
    vt100::Parser::new_with_callbacks(rows, cols, SCROLLBACK, KittyState::new(kitty))
}

/// Write to the PTY and flush. Errors are ignored: once the child has exited
/// the write fails benignly, and there is nothing to recover.
fn write_pty(writer: &SharedWriter, bytes: &[u8]) {
    if let Ok(mut w) = writer.lock() {
        let _ = w.write_all(bytes);
        let _ = w.flush();
    }
}

impl Drop for TermSession {
    /// Best-effort kill+wait so quitting the dashboard never leaks the
    /// `docker exec` client (which would otherwise linger holding the PTY).
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// The dashboard's open terminals as a pure tab strip: a `Vec<TermSession>`
/// plus the active index. Mechanical only — the app owns the focus/open policy
/// (dedup vs. force-new, spawning); this type just tracks and cycles tabs.
#[derive(Default)]
pub struct TermTabs {
    sessions: Vec<TermSession>,
    /// Index into `sessions` of the shown tab. Meaningless when empty; kept
    /// clamped to `[0, len)` by every mutator.
    active: usize,
}

impl TermTabs {
    /// The open sessions, for rendering the tab strip and active screen.
    pub fn sessions(&self) -> &[TermSession] {
        &self.sessions
    }

    /// Active tab index. Only meaningful when `!is_empty()`.
    pub fn active(&self) -> usize {
        self.active
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Resize every open session to `rows`×`cols`. The event loop calls this
    /// before each draw so background tabs are already right-sized when switched
    /// to; `TermSession::resize` no-ops when a session's size is unchanged.
    pub fn resize_all(&mut self, rows: u16, cols: u16) {
        for session in &mut self.sessions {
            session.resize(rows, cols);
        }
    }

    /// The active session, or `None` when no terminals are open.
    pub fn active_session(&self) -> Option<&TermSession> {
        self.sessions.get(self.active)
    }

    /// The active session mutably (for resize / key forwarding).
    pub fn active_session_mut(&mut self) -> Option<&mut TermSession> {
        self.sessions.get_mut(self.active)
    }

    /// Append `session` and make it active.
    pub fn open(&mut self, session: TermSession) {
        self.sessions.push(session);
        self.active = self.sessions.len() - 1;
    }

    /// Index of the first *live* (non-exited) session for `container`. Exited
    /// tabs are skipped so `t`'s dedup opens a fresh terminal alongside a dead
    /// one for the same target instead of re-focusing the corpse.
    pub fn find(&self, container: &str) -> Option<usize> {
        self.sessions
            .iter()
            .position(|s| s.container == container && !s.exited())
    }

    /// Focus tab `idx`, clamped into range. No-op when empty.
    pub fn set_active(&mut self, idx: usize) {
        if self.sessions.is_empty() {
            return;
        }
        self.active = idx.min(self.sessions.len() - 1);
    }

    /// Focus the next tab, wrapping. No-op when empty.
    pub fn next(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        self.active = (self.active + 1) % self.sessions.len();
    }

    /// Focus the previous tab, wrapping. No-op when empty.
    pub fn prev(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        self.active = (self.active + self.sessions.len() - 1) % self.sessions.len();
    }

    /// Drop the active session (its `Drop` kills the child) and clamp `active`
    /// onto the tab that shifts into its place. No-op when empty.
    pub fn close_active(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        self.sessions.remove(self.active);
        if self.active >= self.sessions.len() {
            self.active = self.sessions.len().saturating_sub(1);
        }
    }
}

/// Pump PTY output into the parser until EOF, flagging `exited` when the stream
/// closes. The event loop redraws on a fixed cadence while any terminal exists
/// (see `mod.rs`), so the reader doesn't need to signal "dirty" — ratatui's
/// buffer diff collapses redraws that changed nothing.
fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    parser: Arc<Mutex<TermParser>>,
    writer: SharedWriter,
    exited: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => process_output(&parser, &writer, &buf[..n]),
                Err(_) => break,
            }
        }
        exited.store(true, Ordering::Relaxed);
    });
}

/// Feed one chunk of child output to the parser, then write back the replies
/// its kitty callbacks queued. The write happens after the parser lock is
/// released so a slow PTY never stalls rendering.
fn process_output(parser: &Mutex<TermParser>, writer: &SharedWriter, bytes: &[u8]) {
    let replies = match parser.lock() {
        Ok(mut parser) => {
            parser.process(bytes);
            if !parser.screen().alternate_screen() {
                parser.callbacks_mut().reset_alt();
            }
            parser.callbacks_mut().take_replies()
        }
        Err(_) => return,
    };
    if !replies.is_empty() {
        write_pty(writer, &replies);
    }
}

/// Encode a wheel notch as the mouse report a child that enabled mouse
/// tracking expects: button 64 (up) / 65 (down) plus the xterm modifier bits,
/// at the 0-based cell `(col, row)` of the terminal body, in the child's
/// chosen encoding. The wheel only ever reports presses. `None` when the
/// legacy encodings can't represent the position (they cap at 223 / 2015),
/// where xterm drops the event too.
///
/// Pure: no I/O, so the encodings are unit-tested.
pub fn encode_wheel(
    up: bool,
    modifiers: KeyModifiers,
    col: u16,
    row: u16,
    encoding: vt100::MouseProtocolEncoding,
) -> Option<Vec<u8>> {
    let mut button: u32 = if up { 64 } else { 65 };
    for (m, bit) in [
        (KeyModifiers::SHIFT, 4),
        (KeyModifiers::ALT, 8),
        (KeyModifiers::CONTROL, 16),
    ] {
        if modifiers.contains(m) {
            button |= bit;
        }
    }
    // Protocol coordinates are 1-based.
    let (x, y) = (u32::from(col) + 1, u32::from(row) + 1);
    match encoding {
        vt100::MouseProtocolEncoding::Sgr => Some(format!("\x1b[<{button};{x};{y}M").into_bytes()),
        // X10: each value is one byte offset by 32.
        vt100::MouseProtocolEncoding::Default => {
            let byte = |v: u32| u8::try_from(v + 32).ok();
            Some(vec![0x1b, b'[', b'M', byte(button)?, byte(x)?, byte(y)?])
        }
        // Same offsets, each value a UTF-8 encoded code point (max 2047).
        vt100::MouseProtocolEncoding::Utf8 => {
            let mut out = b"\x1b[M".to_vec();
            for v in [button, x, y] {
                let c = char::from_u32(v + 32).filter(|c| u32::from(*c) < 0x800)?;
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
            Some(out)
        }
    }
}

/// Encode a crossterm key event into the byte sequence a terminal expects,
/// or `None` for keys with no terminal representation. `application_cursor`
/// selects SS3 (`ESC O …`) over CSI (`ESC [ …`) for the cursor/home/end keys,
/// as toggled by the shell's DECCKM mode.
///
/// Pure: no I/O, so the whole table is unit-tested.
pub fn encode_key(key: KeyEvent, application_cursor: bool) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    // Helper: an ESC-prefixed sequence when Alt is held, plain otherwise.
    let alt_prefixed = |body: Vec<u8>| -> Vec<u8> {
        if alt {
            let mut out = vec![0x1b];
            out.extend(body);
            out
        } else {
            body
        }
    };

    match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                // Control codes: derive C0 from the letter/symbol.
                let byte = match c.to_ascii_lowercase() {
                    'a'..='z' => (c.to_ascii_lowercase() as u8) - b'a' + 1,
                    ' ' | '@' => 0x00,
                    '[' => 0x1b,
                    '\\' => 0x1c,
                    ']' => 0x1d,
                    '^' => 0x1e,
                    '_' | '/' => 0x1f,
                    _ => return None,
                };
                return Some(alt_prefixed(vec![byte]));
            }
            let mut body = [0u8; 4];
            let encoded = c.encode_utf8(&mut body).as_bytes().to_vec();
            Some(alt_prefixed(encoded))
        }
        KeyCode::Enter => Some(alt_prefixed(vec![b'\r'])),
        KeyCode::Tab => Some(alt_prefixed(vec![b'\t'])),
        KeyCode::BackTab => Some(b"\x1b[Z".to_vec()),
        KeyCode::Backspace => Some(alt_prefixed(vec![0x7f])),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Up => Some(cursor_seq(b'A', application_cursor)),
        KeyCode::Down => Some(cursor_seq(b'B', application_cursor)),
        KeyCode::Right => Some(cursor_seq(b'C', application_cursor)),
        KeyCode::Left => Some(cursor_seq(b'D', application_cursor)),
        KeyCode::Home => Some(cursor_seq(b'H', application_cursor)),
        KeyCode::End => Some(cursor_seq(b'F', application_cursor)),
        KeyCode::PageUp => Some(b"\x1b[5~".to_vec()),
        KeyCode::PageDown => Some(b"\x1b[6~".to_vec()),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        KeyCode::Insert => Some(b"\x1b[2~".to_vec()),
        KeyCode::F(n) => function_seq(n),
        _ => None,
    }
}

/// Cursor / home / end key: SS3 (`ESC O <f>`) in application-cursor mode,
/// CSI (`ESC [ <f>`) otherwise.
fn cursor_seq(final_byte: u8, application_cursor: bool) -> Vec<u8> {
    if application_cursor {
        vec![0x1b, b'O', final_byte]
    } else {
        vec![0x1b, b'[', final_byte]
    }
}

/// Function-key sequences (VT/xterm): F1–F4 use SS3, F5+ use CSI `… ~`.
fn function_seq(n: u8) -> Option<Vec<u8>> {
    let seq: &[u8] = match n {
        1 => b"\x1bOP",
        2 => b"\x1bOQ",
        3 => b"\x1bOR",
        4 => b"\x1bOS",
        5 => b"\x1b[15~",
        6 => b"\x1b[17~",
        7 => b"\x1b[18~",
        8 => b"\x1b[19~",
        9 => b"\x1b[20~",
        10 => b"\x1b[21~",
        11 => b"\x1b[23~",
        12 => b"\x1b[24~",
        _ => return None,
    };
    Some(seq.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn key_mods(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn plain_chars_encode_utf8() {
        assert_eq!(encode_key(key(KeyCode::Char('a')), false), Some(vec![b'a']));
        assert_eq!(encode_key(key(KeyCode::Char('Z')), false), Some(vec![b'Z']));
        // multi-byte UTF-8
        assert_eq!(
            encode_key(key(KeyCode::Char('é')), false),
            Some("é".as_bytes().to_vec())
        );
    }

    #[test]
    fn ctrl_letters() {
        for (c, byte) in [('a', 0x01u8), ('c', 0x03), ('z', 0x1a)] {
            assert_eq!(
                encode_key(key_mods(KeyCode::Char(c), KeyModifiers::CONTROL), false),
                Some(vec![byte]),
                "ctrl-{c}"
            );
        }
        // uppercase letter with ctrl maps the same as lowercase
        assert_eq!(
            encode_key(key_mods(KeyCode::Char('C'), KeyModifiers::CONTROL), false),
            Some(vec![0x03])
        );
    }

    #[test]
    fn ctrl_symbols() {
        for (c, byte) in [
            (' ', 0x00u8),
            ('@', 0x00),
            ('[', 0x1b),
            ('\\', 0x1c),
            (']', 0x1d),
            ('^', 0x1e),
            ('_', 0x1f),
        ] {
            assert_eq!(
                encode_key(key_mods(KeyCode::Char(c), KeyModifiers::CONTROL), false),
                Some(vec![byte]),
                "ctrl-{c:?}"
            );
        }
    }

    #[test]
    fn alt_prefixes_esc() {
        assert_eq!(
            encode_key(key_mods(KeyCode::Char('b'), KeyModifiers::ALT), false),
            Some(vec![0x1b, b'b'])
        );
        // alt+ctrl: ESC then the control byte
        assert_eq!(
            encode_key(
                key_mods(
                    KeyCode::Char('c'),
                    KeyModifiers::ALT | KeyModifiers::CONTROL
                ),
                false
            ),
            Some(vec![0x1b, 0x03])
        );
    }

    #[test]
    fn named_keys() {
        assert_eq!(encode_key(key(KeyCode::Enter), false), Some(vec![b'\r']));
        assert_eq!(encode_key(key(KeyCode::Tab), false), Some(vec![b'\t']));
        assert_eq!(
            encode_key(key(KeyCode::BackTab), false),
            Some(b"\x1b[Z".to_vec())
        );
        assert_eq!(encode_key(key(KeyCode::Backspace), false), Some(vec![0x7f]));
        assert_eq!(encode_key(key(KeyCode::Esc), false), Some(vec![0x1b]));
    }

    #[test]
    fn arrows_csi_vs_ss3() {
        assert_eq!(
            encode_key(key(KeyCode::Up), false),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            encode_key(key(KeyCode::Down), false),
            Some(b"\x1b[B".to_vec())
        );
        assert_eq!(
            encode_key(key(KeyCode::Right), false),
            Some(b"\x1b[C".to_vec())
        );
        assert_eq!(
            encode_key(key(KeyCode::Left), false),
            Some(b"\x1b[D".to_vec())
        );
        // application cursor mode → SS3
        assert_eq!(encode_key(key(KeyCode::Up), true), Some(b"\x1bOA".to_vec()));
        assert_eq!(
            encode_key(key(KeyCode::Left), true),
            Some(b"\x1bOD".to_vec())
        );
    }

    #[test]
    fn home_end_pgup_pgdn_del_ins() {
        assert_eq!(
            encode_key(key(KeyCode::Home), false),
            Some(b"\x1b[H".to_vec())
        );
        assert_eq!(
            encode_key(key(KeyCode::End), false),
            Some(b"\x1b[F".to_vec())
        );
        assert_eq!(encode_key(key(KeyCode::Home), true), Some(b"\x1bOH".to_vec()));
        assert_eq!(encode_key(key(KeyCode::End), true), Some(b"\x1bOF".to_vec()));
        assert_eq!(
            encode_key(key(KeyCode::PageUp), false),
            Some(b"\x1b[5~".to_vec())
        );
        assert_eq!(
            encode_key(key(KeyCode::PageDown), false),
            Some(b"\x1b[6~".to_vec())
        );
        assert_eq!(
            encode_key(key(KeyCode::Delete), false),
            Some(b"\x1b[3~".to_vec())
        );
        assert_eq!(
            encode_key(key(KeyCode::Insert), false),
            Some(b"\x1b[2~".to_vec())
        );
    }

    #[test]
    fn function_keys() {
        for (n, seq) in [
            (1u8, &b"\x1bOP"[..]),
            (2, b"\x1bOQ"),
            (3, b"\x1bOR"),
            (4, b"\x1bOS"),
            (5, b"\x1b[15~"),
            (6, b"\x1b[17~"),
            (7, b"\x1b[18~"),
            (8, b"\x1b[19~"),
            (9, b"\x1b[20~"),
            (10, b"\x1b[21~"),
            (11, b"\x1b[23~"),
            (12, b"\x1b[24~"),
        ] {
            assert_eq!(
                encode_key(key(KeyCode::F(n)), false),
                Some(seq.to_vec()),
                "F{n}"
            );
        }
        assert_eq!(encode_key(key(KeyCode::F(13)), false), None);
    }

    #[test]
    fn unmapped_returns_none() {
        assert_eq!(encode_key(key(KeyCode::Null), false), None);
        assert_eq!(encode_key(key(KeyCode::CapsLock), false), None);
    }

    #[test]
    fn wheel_reports_in_each_encoding() {
        use vt100::MouseProtocolEncoding::*;
        let none = KeyModifiers::NONE;
        assert_eq!(encode_wheel(true, none, 4, 9, Sgr), Some(b"\x1b[<64;5;10M".to_vec()));
        assert_eq!(encode_wheel(false, none, 0, 0, Sgr), Some(b"\x1b[<65;1;1M".to_vec()));
        assert_eq!(
            encode_wheel(true, KeyModifiers::CONTROL | KeyModifiers::SHIFT, 0, 0, Sgr),
            Some(b"\x1b[<84;1;1M".to_vec())
        );
        assert_eq!(
            encode_wheel(true, none, 4, 9, Default),
            Some(vec![0x1b, b'[', b'M', 96, 37, 42])
        );
        // X10 can't reach column 224+.
        assert_eq!(encode_wheel(true, none, 223, 0, Default), None);
        assert_eq!(
            encode_wheel(false, none, 300, 0, Utf8),
            Some([&b"\x1b[M"[..], "a".as_bytes(), 'ō'.to_string().as_bytes(), b"!"].concat())
        );
    }

    #[test]
    fn test_session_starts_live_and_sized() {
        let s = TermSession::test_session("web-1", "devsandbox-web-1", 24, 80);
        assert_eq!(s.title, "web-1");
        assert_eq!(s.container, "devsandbox-web-1");
        assert_eq!(s.size(), (24, 80));
        assert!(!s.exited());
    }

    #[test]
    fn kitty_query_is_answered_on_the_pty() {
        let s = TermSession::test_session("web-1", "devsandbox-web-1", 24, 80);
        s.feed_output(b"\x1b[?1049h\x1b[>1u\x1b[?u");
        assert_eq!(s.take_written(), b"\x1b[?1u");
        // Back on the main screen: the alt stack is dropped, so the next app to
        // enter it starts clean.
        s.feed_output(b"\x1b[?1049l");
        s.feed_output(b"\x1b[?1049h\x1b[?u");
        assert_eq!(s.take_written(), b"\x1b[?0u");
    }

    #[test]
    fn test_session_resize_updates_size() {
        let mut s = TermSession::test_session("web-1", "devsandbox-web-1", 24, 80);
        s.resize(30, 100);
        assert_eq!(s.size(), (30, 100));
        // no-op path: unchanged size stays put
        s.resize(30, 100);
        assert_eq!(s.size(), (30, 100));
    }

    fn sess(title: &str, container: &str) -> TermSession {
        TermSession::test_session(title, container, 24, 80)
    }

    #[test]
    fn tabs_resize_all_sizes_every_session() {
        let mut tabs = TermTabs::default();
        tabs.open(sess("a", "devsandbox-a"));
        tabs.open(sess("b", "devsandbox-b"));
        tabs.resize_all(40, 120);
        for s in tabs.sessions() {
            assert_eq!(s.size(), (40, 120));
        }
    }

    #[test]
    fn tabs_open_sets_active_last() {
        let mut tabs = TermTabs::default();
        assert!(tabs.is_empty());
        tabs.open(sess("a", "devsandbox-a"));
        tabs.open(sess("b", "devsandbox-b"));
        assert_eq!(tabs.sessions().len(), 2);
        assert_eq!(tabs.active(), 1);
        assert_eq!(tabs.active_session().unwrap().title, "b");
    }

    #[test]
    fn tabs_next_prev_wrap() {
        let mut tabs = TermTabs::default();
        tabs.open(sess("a", "devsandbox-a"));
        tabs.open(sess("b", "devsandbox-b"));
        tabs.open(sess("c", "devsandbox-c")); // active = 2
        tabs.next();
        assert_eq!(tabs.active(), 0); // wrapped
        tabs.prev();
        assert_eq!(tabs.active(), 2); // wrapped back
    }

    #[test]
    fn tabs_set_active_clamps() {
        let mut tabs = TermTabs::default();
        tabs.open(sess("a", "devsandbox-a"));
        tabs.open(sess("b", "devsandbox-b"));
        tabs.set_active(99);
        assert_eq!(tabs.active(), 1);
    }

    #[test]
    fn tabs_close_active_clamps() {
        let mut tabs = TermTabs::default();
        tabs.open(sess("a", "devsandbox-a"));
        tabs.open(sess("b", "devsandbox-b"));
        tabs.open(sess("c", "devsandbox-c")); // active = 2 (last)
        tabs.close_active();
        // last removed → active clamps to new last
        assert_eq!(tabs.sessions().len(), 2);
        assert_eq!(tabs.active(), 1);
        assert_eq!(tabs.active_session().unwrap().title, "b");
        tabs.set_active(0);
        tabs.close_active(); // remove "a", "b" shifts to index 0
        assert_eq!(tabs.active(), 0);
        assert_eq!(tabs.active_session().unwrap().title, "b");
        tabs.close_active();
        assert!(tabs.is_empty());
        tabs.close_active(); // no-op on empty
        assert!(tabs.is_empty());
    }

    #[test]
    fn find_dedup_skips_exited() {
        let mut tabs = TermTabs::default();
        let dead = sess("a", "devsandbox-a");
        dead.set_exited();
        tabs.open(dead);
        // exited tab for the container is not a dedup target
        assert_eq!(tabs.find("devsandbox-a"), None);
        tabs.open(sess("a2", "devsandbox-a")); // a live one alongside
        assert_eq!(tabs.find("devsandbox-a"), Some(1));
        assert_eq!(tabs.find("devsandbox-missing"), None);
    }
}
