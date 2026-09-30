//! Integrated terminal panel: opening/targeting sessions, focus, and
//! key/mouse routing into the active PTY.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use crate::runtime::{backend, NAME_PREFIX};
use crate::tui::data::ContainerStatus;
use crate::tui::kitty;
use crate::commands::exec::SHELL_FALLBACK_CMD;
use crate::tui::term::{encode_key, encode_wheel, TermSession};

use super::view::point_in;
use super::{App, Focus, Tab};

/// Lines one wheel notch scrolls, as scrollback or as arrow keys.
const WHEEL_LINES: i16 = 3;

impl App {
    /// Route a key to the active terminal. Only reached with `focus ==
    /// Terminal`. `ctrl-]` / `F12` leave; on an exited session every other key
    /// is swallowed; otherwise the key is encoded (honoring the shell's
    /// application-cursor mode and kitty keyboard flags) and written to the PTY.
    pub(super) fn on_key_terminal(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // Leave keys: back to the dashboard.
        if matches!(key.code, KeyCode::Char(']') if ctrl) || key.code == KeyCode::F(12) {
            self.focus = Focus::Dashboard;
            return;
        }
        // The session can vanish (closed elsewhere); fall back to the dashboard
        // rather than forward into nothing.
        let Some(session) = self.terms.active_session_mut() else {
            self.focus = Focus::Dashboard;
            return;
        };
        // An exited shell has nothing to receive keys; swallow them (leave keys
        // above still work). `x` closes it in place, mirroring the dashboard
        // binding; fall back to the dashboard once no sessions remain.
        if session.exited() {
            if key.code == KeyCode::Char('x') {
                self.terms.close_active();
                if self.terms.is_empty() {
                    self.focus = Focus::Dashboard;
                }
            }
            return;
        }
        // DECCKM: the parser tracks whether the shell wants SS3 cursor keys;
        // its callbacks track the kitty flags of the screen the app is on.
        let (application_cursor, kitty_flags) = session
            .parser()
            .lock()
            .map(|p| {
                let screen = p.screen();
                (screen.application_cursor(), p.callbacks().flags(screen.alternate_screen()))
            })
            .unwrap_or((false, 0));
        let bytes =
            kitty::encode_key(key, kitty_flags).or_else(|| encode_key(key, application_cursor));
        if let Some(bytes) = bytes {
            session.write_key_bytes(&bytes);
        }
    }

    /// Focus the active terminal, or leave a status hint when none are open.
    pub(super) fn enter_terminal(&mut self) {
        if self.terms.is_empty() {
            self.status = Some("no terminal open — press t".into());
        } else {
            self.focus = Focus::Terminal;
        }
    }

    /// Mouse routing for the integrated-terminal panel, active only when no
    /// config modal is open and at least one terminal exists. Left-click inside
    /// the panel focuses the terminal; a click on the title row's tab labels
    /// activates that tab; a left-click outside the panel while the terminal is
    /// focused returns focus to the dashboard. The wheel goes to the active
    /// session (see [`Self::wheel_active_terminal`]). All layout math is
    /// borrowed from `ui` so it tracks exactly what the draw path lays out.
    /// I/O-free.
    pub(super) fn terminal_mouse(&mut self, ev: &MouseEvent, area: Rect) {
        if self.terms.is_empty() {
            return;
        }
        let prompt_open = self.prompt.is_some();
        let panel = crate::tui::ui::terminal_panel_rect(area, prompt_open);
        let inside = point_in(panel, ev.column, ev.row);
        match ev.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if inside {
                    let focused = self.focus == Focus::Terminal;
                    // Title row: hit-test the tab labels; a hit activates that tab.
                    if let Some(i) =
                        crate::tui::ui::terminal_tab_hit(self, panel, focused, ev.column, ev.row)
                    {
                        self.terms.set_active(i);
                    }
                    self.focus = Focus::Terminal;
                } else if self.focus == Focus::Terminal {
                    // Click-away unfocuses; dashboard click handling is out of scope.
                    self.focus = Focus::Dashboard;
                }
            }
            MouseEventKind::ScrollUp if inside => self.wheel_active_terminal(true, ev, panel),
            MouseEventKind::ScrollDown if inside => self.wheel_active_terminal(false, ev, panel),
            _ => {}
        }
    }

    /// One wheel notch over the active terminal, routed like xterm does:
    /// - the child enabled mouse tracking (zidane, vim with `mouse=a`, htop):
    ///   forward it as a wheel report at the pane-relative cell, so the app
    ///   scrolls its own view;
    /// - the child is on the alternate screen without mouse tracking (less,
    ///   man): send arrow keys ("alternate scroll"), since that screen has no
    ///   scrollback of its own;
    /// - otherwise (a shell's main screen, or an exited session): scroll the
    ///   vt100 scrollback.
    fn wheel_active_terminal(&mut self, up: bool, ev: &MouseEvent, panel: Rect) {
        let Some(session) = self.terms.active_session_mut() else {
            return;
        };
        let bytes = {
            let Ok(parser) = session.parser().lock() else {
                return;
            };
            let screen = parser.screen();
            let mode = screen.mouse_protocol_mode();
            if session.exited()
                || (mode == vt100::MouseProtocolMode::None && !screen.alternate_screen())
            {
                None
            } else if mode != vt100::MouseProtocolMode::None {
                // Body = panel minus its 1-cell border; clamp so a notch over
                // the border still lands on an edge cell.
                let cols = panel.width.saturating_sub(2).max(1);
                let rows = panel.height.saturating_sub(2).max(1);
                let col = ev.column.saturating_sub(panel.x + 1).min(cols - 1);
                let row = ev.row.saturating_sub(panel.y + 1).min(rows - 1);
                // An unencodable position drops the notch, as xterm does.
                Some(
                    encode_wheel(up, ev.modifiers, col, row, screen.mouse_protocol_encoding())
                        .unwrap_or_default(),
                )
            } else {
                let key = KeyEvent::new(
                    if up { KeyCode::Up } else { KeyCode::Down },
                    KeyModifiers::NONE,
                );
                encode_key(key, screen.application_cursor())
                    .map(|b| b.repeat(WHEEL_LINES as usize))
            }
        };
        match bytes {
            Some(bytes) if !bytes.is_empty() => session.write_key_bytes(&bytes),
            Some(_) => {}
            None => self.scroll_active_terminal(if up { WHEEL_LINES } else { -WHEEL_LINES }),
        }
    }

    /// Scroll the active session's vt100 scrollback by `delta` lines: positive
    /// scrolls up (older output, larger offset), negative scrolls down toward the
    /// live screen (offset 0). vt100 clamps the offset to the actual scrollback,
    /// so overshoot is harmless. New output while scrolled back keeps the offset
    /// (vt100's behavior); it snaps back when the user scrolls down to 0.
    fn scroll_active_terminal(&mut self, delta: i16) {
        let Some(session) = self.terms.active_session() else {
            return;
        };
        let Ok(mut parser) = session.parser().lock() else {
            return;
        };
        let current = parser.screen().scrollback() as i32;
        let next = (current + delta as i32).max(0) as usize;
        parser.screen_mut().set_scrollback(next);
    }

    /// Resolve the current selection to a terminal target: `(title, container,
    /// is_instance)`, or an `Err(status message)` explaining why one can't open.
    /// Pure over the snapshot so it is unit-testable; [`Self::open_terminal`]
    /// does the state load + PTY spawn around it.
    ///
    /// Instances tab: the instance under the cursor (or a process row's parent),
    /// which must be running. Services tab: the selected service's first running
    /// container.
    fn term_target(&self) -> Result<(String, String, bool), String> {
        let snapshot = self
            .snapshot
            .as_ref()
            .ok_or_else(|| "terminal: no data yet".to_string())?;
        match self.tab {
            Tab::Instances => {
                let idx = self
                    .selected_instance_index()
                    .ok_or_else(|| "terminal: select an instance".to_string())?;
                let row = snapshot
                    .instances
                    .get(idx)
                    .ok_or_else(|| "terminal: select an instance".to_string())?;
                if !matches!(row.status, ContainerStatus::Running(_)) {
                    return Err(format!("terminal: `{}` is not running", row.name));
                }
                Ok((row.name.clone(), row.container.clone(), true))
            }
            Tab::Services => {
                let row = snapshot
                    .services
                    .get(self.selected())
                    .ok_or_else(|| "terminal: select a service".to_string())?;
                let container = row
                    .containers
                    .iter()
                    .find(|(_, status)| matches!(status, ContainerStatus::Running(_)))
                    .map(|(name, _)| name.clone())
                    .ok_or_else(|| {
                        format!("terminal: no running container for `{}`", row.name)
                    })?;
                let title = container
                    .strip_prefix(NAME_PREFIX)
                    .unwrap_or(&container)
                    .to_string();
                Ok((title, container, false))
            }
            // The Ports tab has no terminal target: forwards aren't containers.
            Tab::Ports => Err("terminal: not available on the Ports tab".to_string()),
            Tab::Inbox => Err("terminal: not available on the Inbox tab".to_string()),
        }
    }

    /// Open (or focus) an integrated terminal for the current selection. `t`
    /// dedups against a live terminal for the same container; `T`
    /// (`force_new`) always spawns a fresh one.
    ///
    /// This does I/O inline — a state load plus a PTY spawn — same precedent as
    /// [`Self::open_logs`] / [`Self::open_config`] doing user-triggered work
    /// without going through the event loop. Failures land in `self.status`.
    pub(super) fn open_terminal(&mut self, force_new: bool) {
        let (title, container, is_instance) = match self.term_target() {
            Ok(t) => t,
            Err(msg) => {
                self.status = Some(msg);
                return;
            }
        };

        // Dedup: an already-open live terminal for this container just gets focus.
        if !force_new {
            if let Some(idx) = self.terms.find(&container) {
                self.terms.set_active(idx);
                self.focus = Focus::Terminal;
                self.status = None;
                return;
            }
        }

        // Build the runtime argv. Instances reuse the exact `exec` flags the CLI
        // emits (workspace, remoteUser, remoteEnv) so the two can't drift;
        // services get a plain interactive `exec`.
        let shell: Vec<String> = SHELL_FALLBACK_CMD.iter().map(|s| s.to_string()).collect();
        let mut argv = if is_instance {
            let state = match crate::state::State::load() {
                Ok(s) => s,
                Err(e) => {
                    self.status = Some(format!("terminal: {e:#}"));
                    return;
                }
            };
            let Some(instance) = state.instances.get(&title) else {
                self.status = Some(format!("terminal: `{title}` not in state"));
                return;
            };
            crate::commands::exec::exec_argv(instance, true, true, &shell)
        } else {
            let mut a = vec!["exec".to_string(), "-i".to_string(), "-t".to_string()];
            a.push(container.clone());
            a.extend(shell);
            a
        };
        // Prepend the runtime binary: `exec_argv` and the service path both omit it.
        argv.insert(0, backend().bin().to_string());

        // Size doesn't matter yet: the event loop's pre-draw resize corrects the
        // first frame. Reuse the active session's size when there is one, else a
        // sane 24x80 default.
        let (rows, cols) = self
            .terms
            .active_session()
            .map(|s| s.size())
            .unwrap_or((24, 80));
        match TermSession::spawn(title, container, argv, rows, cols, self.kitty) {
            Ok(session) => {
                self.terms.open(session);
                self.focus = Focus::Terminal;
                self.status = None;
            }
            Err(e) => self.status = Some(format!("terminal: {e:#}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::test_support::*;
    use crate::tui::app::*;

    #[test]
    fn mouse_click_in_panel_focuses_terminal() {
        let mut app = new_app();
        app.terms.open(term_sess("web-1", "devsandbox-web-1"));
        assert_eq!(app.focus, Focus::Dashboard);
        let panel = crate::tui::ui::terminal_panel_rect(FRAME, false);
        // A click in the panel body (below the title row) focuses the terminal.
        let click = mouse_at(
            MouseEventKind::Down(MouseButton::Left),
            panel.x + 2,
            panel.y + 2,
        );
        app.on_mouse(&click, FRAME);
        assert_eq!(app.focus, Focus::Terminal);
    }

    #[test]
    fn mouse_click_outside_panel_unfocuses() {
        let mut app = new_app();
        app.terms.open(term_sess("web-1", "devsandbox-web-1"));
        app.focus = Focus::Terminal;
        // Top-left corner is outside the bottom-right panel → back to dashboard.
        let click = mouse_at(MouseEventKind::Down(MouseButton::Left), 0, 0);
        app.on_mouse(&click, FRAME);
        assert_eq!(app.focus, Focus::Dashboard);
    }

    #[test]
    fn mouse_click_on_tab_label_activates_it() {
        let mut app = new_app();
        app.terms.open(term_sess("web-1", "devsandbox-web-1"));
        app.terms.open(term_sess("api-2", "devsandbox-api-2"));
        // Two tabs open; the second is active. Click the FIRST tab's label.
        assert_eq!(app.terms.active(), 1);
        let panel = crate::tui::ui::terminal_panel_rect(FRAME, false);
        // Labels (not focused): "1:web-1" at panel.x+1, "2:api-2" after +2 sep.
        // Land on the first character of the first label.
        let click = mouse_at(
            MouseEventKind::Down(MouseButton::Left),
            panel.x + 1,
            panel.y,
        );
        app.on_mouse(&click, FRAME);
        assert_eq!(app.terms.active(), 0);
        assert_eq!(app.focus, Focus::Terminal);
        // Now focused, so the `▶ ` mark (2 cols) shifts labels right by 2.
        // Tab 2 "2:api-2" starts at x+1 + 2(mark) + 7("1:web-1") + 2(sep) = x+12.
        let click2 = mouse_at(
            MouseEventKind::Down(MouseButton::Left),
            panel.x + 1 + 2 + 7 + 2,
            panel.y,
        );
        app.on_mouse(&click2, FRAME);
        assert_eq!(app.terms.active(), 1);
    }

    #[test]
    fn mouse_click_in_gap_between_tabs_keeps_active() {
        let mut app = new_app();
        app.terms.open(term_sess("web-1", "devsandbox-web-1"));
        app.terms.open(term_sess("api-2", "devsandbox-api-2"));
        app.terms.set_active(0);
        let panel = crate::tui::ui::terminal_panel_rect(FRAME, false);
        // The two-space gap after "1:web-1" (cols x+1+7, x+1+8) hits no label,
        // so the active tab is unchanged — but the click still focuses.
        let click = mouse_at(
            MouseEventKind::Down(MouseButton::Left),
            panel.x + 1 + 7,
            panel.y,
        );
        app.on_mouse(&click, FRAME);
        assert_eq!(app.terms.active(), 0);
        assert_eq!(app.focus, Focus::Terminal);
    }

    #[test]
    fn mouse_in_panel_ignored_when_no_terminal() {
        let mut app = new_app();
        // No terminals: a click never changes focus.
        let click = mouse_at(MouseEventKind::Down(MouseButton::Left), 50, 30);
        app.on_mouse(&click, FRAME);
        assert_eq!(app.focus, Focus::Dashboard);
    }

    #[test]
    fn mouse_wheel_scrolls_active_terminal_scrollback() {
        let mut app = new_app();
        app.terms.open(term_sess("web-1", "devsandbox-web-1"));
        // Build real scrollback: push 60 lines through a 24-row screen so lines
        // scroll off the top and vt100 has somewhere to scroll back to.
        {
            let session = app.terms.active_session().unwrap();
            let mut parser = session.parser().lock().unwrap();
            for i in 0..60 {
                parser.process(format!("line {i}\r\n").as_bytes());
            }
        }
        let panel = crate::tui::ui::terminal_panel_rect(FRAME, false);
        let body = (panel.x + 2, panel.y + 2);
        fn offset(app: &App) -> usize {
            app.terms
                .active_session()
                .unwrap()
                .parser()
                .lock()
                .unwrap()
                .screen()
                .scrollback()
        }
        assert_eq!(offset(&app), 0);
        // Wheel up over the body scrolls back by 3 lines.
        app.on_mouse(&mouse_at(MouseEventKind::ScrollUp, body.0, body.1), FRAME);
        assert_eq!(offset(&app), 3);
        app.on_mouse(&mouse_at(MouseEventKind::ScrollUp, body.0, body.1), FRAME);
        assert_eq!(offset(&app), 6);
        // Wheel down walks it back toward the live screen, saturating at 0.
        app.on_mouse(&mouse_at(MouseEventKind::ScrollDown, body.0, body.1), FRAME);
        assert_eq!(offset(&app), 3);
        for _ in 0..5 {
            app.on_mouse(&mouse_at(MouseEventKind::ScrollDown, body.0, body.1), FRAME);
        }
        assert_eq!(offset(&app), 0);
        // The wheel outside the panel body is ignored (offset stays 0).
        app.on_mouse(&mouse_at(MouseEventKind::ScrollUp, 0, 0), FRAME);
        assert_eq!(offset(&app), 0);
    }

    #[test]
    fn wheel_is_reported_to_a_child_tracking_the_mouse() {
        let mut app = new_app();
        app.terms.open(term_sess("web-1", "devsandbox-web-1"));
        let session = app.terms.active_session().unwrap();
        session.feed_output(b"\x1b[?1049h\x1b[?1000h\x1b[?1006h");
        let panel = crate::tui::ui::terminal_panel_rect(FRAME, false);
        // Body cell (2, 1): one column/row in from the border.
        let (x, y) = (panel.x + 3, panel.y + 2);
        app.on_mouse(&mouse_at(MouseEventKind::ScrollUp, x, y), FRAME);
        app.on_mouse(&mouse_at(MouseEventKind::ScrollDown, x, y), FRAME);
        let session = app.terms.active_session().unwrap();
        assert_eq!(session.take_written(), b"\x1b[<64;3;2M\x1b[<65;3;2M");
        // A notch over the border clamps onto the first body cell.
        app.on_mouse(&mouse_at(MouseEventKind::ScrollUp, panel.x, panel.y), FRAME);
        let session = app.terms.active_session().unwrap();
        assert_eq!(session.take_written(), b"\x1b[<64;1;1M");
    }

    #[test]
    fn wheel_sends_arrows_on_the_alternate_screen_without_mouse_tracking() {
        let mut app = new_app();
        app.terms.open(term_sess("web-1", "devsandbox-web-1"));
        let session = app.terms.active_session().unwrap();
        // less/man style: alternate screen plus application cursor keys.
        session.feed_output(b"\x1b[?1049h\x1b[?1h");
        let panel = crate::tui::ui::terminal_panel_rect(FRAME, false);
        let (x, y) = (panel.x + 2, panel.y + 2);
        app.on_mouse(&mouse_at(MouseEventKind::ScrollDown, x, y), FRAME);
        let session = app.terms.active_session().unwrap();
        assert_eq!(session.take_written(), b"\x1bOB\x1bOB\x1bOB");
        assert_eq!(session.parser().lock().unwrap().screen().scrollback(), 0);
    }

    #[test]
    fn wheel_on_an_exited_session_only_scrolls_back() {
        let mut app = new_app();
        app.terms.open(term_sess("web-1", "devsandbox-web-1"));
        let session = app.terms.active_session().unwrap();
        for i in 0..60 {
            session.feed_output(format!("line {i}\r\n").as_bytes());
        }
        session.feed_output(b"\x1b[?1000h");
        session.set_exited();
        let panel = crate::tui::ui::terminal_panel_rect(FRAME, false);
        app.on_mouse(&mouse_at(MouseEventKind::ScrollUp, panel.x + 2, panel.y + 2), FRAME);
        let session = app.terms.active_session().unwrap();
        assert!(session.take_written().is_empty());
        assert_eq!(session.parser().lock().unwrap().screen().scrollback(), 3);
    }

    #[test]
    fn keys_follow_the_childs_kitty_flags() {
        let mut app = new_app();
        open_test_term(&mut app, "web-1", "devsandbox-web-1");
        let ctrl_m = KeyEvent::new(KeyCode::Char('m'), KeyModifiers::CONTROL);
        let written = |app: &App| app.terms.active_session().unwrap().take_written();
        // No flags pushed: legacy, ctrl+m is the same `\r` as Enter.
        app.on_key(ctrl_m);
        assert_eq!(written(&app), b"\r");
        // The child opts in on the alternate screen, like zidane does.
        app.terms.active_session().unwrap().feed_output(b"\x1b[?1049h\x1b[>1u");
        app.on_key(ctrl_m);
        assert_eq!(written(&app), b"\x1b[109;5u");
        // Plain text is unaffected.
        app.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        assert_eq!(written(&app), b"a");
    }

    #[test]
    fn terminal_focus_swallows_dashboard_keys() {
        let mut app = new_app();
        open_test_term(&mut app, "web-1", "devsandbox-web-1");
        let before = app.tab;
        // Tab must not switch the top-level tab while a terminal is focused.
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.tab, before);
        // `q` forwards to the shell rather than quitting.
        app.on_key(key(KeyCode::Char('q')));
        assert!(!app.should_quit);
        assert_eq!(app.focus, Focus::Terminal);
    }

    #[test]
    fn ctrl_bracket_and_f12_leave_terminal() {
        let mut app = new_app();
        open_test_term(&mut app, "web-1", "devsandbox-web-1");
        app.on_key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::CONTROL));
        assert_eq!(app.focus, Focus::Dashboard);

        open_test_term(&mut app, "web-1", "devsandbox-web-1");
        app.on_key(key(KeyCode::F(12)));
        assert_eq!(app.focus, Focus::Dashboard);
    }

    #[test]
    fn enter_terminal_needs_a_terminal() {
        let mut app = new_app();
        // No terminals: ctrl-] and F12 stay on the dashboard with a hint.
        app.on_key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::CONTROL));
        assert_eq!(app.focus, Focus::Dashboard);
        assert_eq!(app.status.as_deref(), Some("no terminal open — press t"));
        app.on_key(key(KeyCode::F(12)));
        assert_eq!(app.focus, Focus::Dashboard);
        assert_eq!(app.status.as_deref(), Some("no terminal open — press t"));

        // With one open (but unfocused), ctrl-] focuses it.
        app.terms
            .open(TermSession::test_session("web-1", "devsandbox-web-1", 24, 80));
        app.on_key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::CONTROL));
        assert_eq!(app.focus, Focus::Terminal);
    }

    #[test]
    fn dashboard_brackets_cycle_and_x_closes() {
        let mut app = new_app();
        app.terms
            .open(TermSession::test_session("a", "devsandbox-a", 24, 80));
        app.terms
            .open(TermSession::test_session("b", "devsandbox-b", 24, 80)); // active = 1
        // `]` wraps to 0, `[` wraps back to 1.
        app.on_key(key(KeyCode::Char(']')));
        assert_eq!(app.terms.active(), 0);
        app.on_key(key(KeyCode::Char('[')));
        assert_eq!(app.terms.active(), 1);
        // `x` closes the active one.
        app.on_key(key(KeyCode::Char('x')));
        assert_eq!(app.terms.sessions().len(), 1);
        assert_eq!(app.terms.active_session().unwrap().title, "a");
    }

    #[test]
    fn exited_terminal_swallows_keys_but_leaves() {
        let mut app = new_app();
        app.terms
            .open(TermSession::test_session("web-1", "devsandbox-web-1", 24, 80));
        app.terms.active_session().unwrap().set_exited();
        app.focus = Focus::Terminal;
        // Ordinary keys are swallowed without panicking; focus is unchanged.
        app.on_key(key(KeyCode::Char('a')));
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.focus, Focus::Terminal);
        // Leave keys still work on an exited session.
        app.on_key(key(KeyCode::F(12)));
        assert_eq!(app.focus, Focus::Dashboard);
    }

    #[test]
    fn x_closes_exited_terminal_and_returns_to_dashboard() {
        let mut app = new_app();
        app.terms
            .open(TermSession::test_session("web-1", "devsandbox-web-1", 24, 80));
        app.terms.active_session().unwrap().set_exited();
        app.focus = Focus::Terminal;
        // `x` closes the exited session in place; with none left, focus drops
        // back to the dashboard.
        app.on_key(key(KeyCode::Char('x')));
        assert!(app.terms.is_empty());
        assert_eq!(app.focus, Focus::Dashboard);
    }

    #[test]
    fn x_closes_exited_terminal_but_stays_focused_with_others() {
        let mut app = new_app();
        app.terms
            .open(TermSession::test_session("web-1", "devsandbox-web-1", 24, 80));
        app.terms
            .open(TermSession::test_session("web-2", "devsandbox-web-2", 24, 80));
        app.terms.active_session().unwrap().set_exited();
        app.focus = Focus::Terminal;
        app.on_key(key(KeyCode::Char('x')));
        // One session remains, so the terminal stays focused.
        assert_eq!(app.terms.sessions().len(), 1);
        assert_eq!(app.focus, Focus::Terminal);
    }

    #[test]
    fn term_target_instance_requires_running() {
        let mut app = new_app();
        // Not running → Err with the instance name.
        app.set_snapshot(snapshot_with_status(1, ContainerStatus::Missing));
        app.on_key(key(KeyCode::Down)); // onto inst0
        assert_eq!(
            app.term_target(),
            Err("terminal: `inst0` is not running".into())
        );
        // Running → the instance target.
        app.set_snapshot(snapshot_with_status(1, running()));
        app.on_key(key(KeyCode::Down));
        assert_eq!(
            app.term_target(),
            Ok(("inst0".into(), "devsandbox-inst0".into(), true))
        );
    }

    #[test]
    fn term_target_service_picks_first_running() {
        let mut app = new_app();
        app.tab = Tab::Services;
        // No running container → Err.
        app.set_snapshot(service_snapshot(vec![(
            "devsandbox-svc-web-1".into(),
            ContainerStatus::Exited("Exited (0)".into()),
        )]));
        assert_eq!(
            app.term_target(),
            Err("terminal: no running container for `svc`".into())
        );
        // First running container wins; title strips the prefix.
        app.set_snapshot(service_snapshot(vec![
            (
                "devsandbox-svc-web-1".into(),
                ContainerStatus::Exited("Exited (0)".into()),
            ),
            ("devsandbox-svc-api-2".into(), running()),
        ]));
        assert_eq!(
            app.term_target(),
            Ok(("svc-api-2".into(), "devsandbox-svc-api-2".into(), false))
        );
    }

    #[test]
    fn find_dedup_vs_force_new() {
        // TermTabs dedup: an exited tab for a container is not a target, a live
        // one is (mirrors `t` vs `T` at the app layer).
        let mut app = new_app();
        let dead = TermSession::test_session("web-1", "devsandbox-web-1", 24, 80);
        dead.set_exited();
        app.terms.open(dead);
        assert_eq!(app.terms.find("devsandbox-web-1"), None);
        app.terms
            .open(TermSession::test_session("web-1", "devsandbox-web-1", 24, 80));
        assert_eq!(app.terms.find("devsandbox-web-1"), Some(1));
    }
}
