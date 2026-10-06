//! Mouse text selection routing (docs/tui-selection.md, *Routing*). The
//! regions come from the last draw (`ui` registers them through the hooks
//! here), so a press is hit-tested against what is on screen; the selected
//! text is read off the next frame by `ui::draw` and sent by the event loop.

use std::rc::Rc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use crate::tui::select::{Candidate, Region, RegionId, RowText, Selection, Source};

use super::view::{HelpModal, Modal, Pane};
use super::App;

/// What the next draw should do with the selection's text: [`Copy`] it to
/// the clipboard, or only [`Measure`] it for the `selected N chars` hint
/// (a release with copy on select off). Read off the drawn frame either way,
/// so the count is exactly what a copy would send.
///
/// [`Copy`]: Extract::Copy
/// [`Measure`]: Extract::Measure
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Extract {
    Copy,
    Measure,
}

/// The copy shortcut: ctrl+shift+c, or cmd/super+c. It copies whatever
/// the copy-on-select setting; where the emulator binds it to its own copy
/// instead, the key never reaches us (and the release copies only with the
/// setting on). Crossterm reports the letter as `c` or `C` with
/// SHIFT depending on the kitty flags, and SUPER only under kitty flags;
/// a legacy terminal sends ctrl+shift+c as plain ctrl+c, which this isn't.
pub(super) fn is_copy_key(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('c' | 'C'))
        && (key.modifiers.contains(KeyModifiers::CONTROL | KeyModifiers::SHIFT)
            || key.modifiers.contains(KeyModifiers::SUPER))
}

/// Whether the outer terminal is known to bind ctrl+shift+c to its own copy
/// by default, so that key never reaches us and the dashboard should name
/// ctrl-c instead. A heuristic from the env vars each terminal sets (`env`
/// is `std::env::var(..).ok()` at startup, a closure for tests): under tmux
/// or ssh they may be missing or stale. It only changes wording, never
/// behaviour: every copy key works the same either way.
pub fn copy_key_intercepted(env: impl Fn(&str) -> Option<String>) -> bool {
    let set = |var: &str| env(var).is_some_and(|v| !v.is_empty());
    let term = env("TERM").unwrap_or_default();
    let program = env("TERM_PROGRAM").unwrap_or_default();
    [
        "ALACRITTY_WINDOW_ID",
        "ALACRITTY_SOCKET",
        "KITTY_WINDOW_ID",
        "WEZTERM_PANE",
        "WEZTERM_EXECUTABLE",
        "VTE_VERSION", // GNOME Terminal, Tilix, Terminator, …
        "KONSOLE_VERSION",
        "GHOSTTY_RESOURCES_DIR",
        "WT_SESSION", // Windows Terminal
    ]
    .into_iter()
    .any(set)
        || matches!(term.as_str(), "alacritty" | "xterm-kitty" | "foot" | "foot-extra")
        || matches!(program.as_str(), "ghostty" | "vscode")
}

impl App {
    /// Renderer hook: forget the last frame's regions, before drawing a new one.
    pub fn clear_regions(&self) {
        self.regions.borrow_mut().clear();
    }

    /// Renderer hook: `rect` (a pane's inner text area) is selectable as `id`.
    pub fn add_region(&self, id: RegionId, rect: Rect) {
        self.add_source_region(id, rect, Source::Screen);
    }

    /// Renderer hook: `rect` shows `rows` of a document from row `scroll` on
    /// ([`Source::Rows`]).
    pub fn add_rows_region(&self, id: RegionId, rect: Rect, scroll: usize, rows: Vec<RowText>) {
        self.add_source_region(id, rect, Source::Rows { scroll, rows: Rc::from(rows) });
    }

    /// Renderer hook: the terminal body `rect` shows its screen from
    /// [`Source::Terminal`] row `top` on. Only while the child doesn't track
    /// the mouse: then the mouse is the child's, not a selection's.
    pub fn add_terminal_region(&self, rect: Rect, top: usize) {
        self.add_source_region(RegionId::Terminal, rect, Source::Terminal { top });
    }

    fn add_source_region(&self, id: RegionId, rect: Rect, source: Source) {
        if !rect.is_empty() {
            self.regions.borrow_mut().push(Region { id, rect, source });
        }
    }

    /// Region `id` of the last frame; a cheap clone (a `Rows` document is
    /// shared).
    fn region(&self, id: RegionId) -> Option<Region> {
        self.regions.borrow().iter().find(|r| r.id == id).cloned()
    }

    /// Autoscroll: a drag at screen row `row`, above or below scrolled
    /// region `id`, scrolls its document one row toward the pointer, through
    /// the pane's own scroll state and clamp, and the frame's copy of the
    /// region follows so later drags before the next draw see it. Returns
    /// the region as it is now.
    fn autoscroll(&mut self, id: RegionId, row: u16) -> Option<Region> {
        let region = self.region(id)?;
        let delta: i16 = match region.source {
            Source::Rows { .. } | Source::Terminal { .. } if row < region.rect.y => -1,
            Source::Rows { .. } | Source::Terminal { .. } if row >= region.rect.bottom() => 1,
            _ => return Some(region),
        };
        let scroll = match (id, &mut self.modal) {
            (RegionId::InboxThread, _) => self.inbox.scroll_pane(delta) as usize,
            (RegionId::ConfigLeft, Modal::Config(view)) => view.scroll_pane(Pane::Config, delta) as usize,
            (RegionId::ConfigRight, Modal::Config(view)) => view.scroll_pane(Pane::Inspect, delta) as usize,
            (RegionId::TextModal, Modal::Help(HelpModal { text: view, .. }) | Modal::Logs(view)) => {
                view.scroll_by(delta) as usize
            }
            // Up is older output, a larger scrollback offset; vt100 clamps
            // it (the alternate screen has none: it stays put).
            (RegionId::Terminal, _) => self.scroll_terminal_selection(-delta)?,
            _ => return Some(region),
        };
        let mut regions = self.regions.borrow_mut();
        let region = regions.iter_mut().find(|r| r.id == id)?;
        if let Source::Rows { scroll: s, .. } | Source::Terminal { top: s } = &mut region.source {
            *s = scroll;
        }
        Some(region.clone())
    }

    /// The live selection and its region in the frame being drawn, for the
    /// highlight and the copy. `None` once its region is gone from the screen.
    pub fn selection_in_frame(&self) -> Option<(Region, Selection)> {
        let sel = self.selection?;
        Some((self.region(sel.region)?, sel))
    }

    pub(super) fn clear_selection(&mut self) {
        self.selection = None;
        self.candidate = None;
        self.extract_requested.set(None);
    }

    /// The copy shortcut ([`is_copy_key`]) over a live selection: ask the
    /// next draw to copy its text, whatever the copy-on-select setting.
    /// Returns whether it consumed the key; without a selection the key goes
    /// on as before.
    pub(super) fn selection_key(&mut self, key: &KeyEvent) -> bool {
        if self.selection.is_none() || !is_copy_key(key) {
            return false;
        }
        self.extract_requested.set(Some(Extract::Copy));
        true
    }

    /// Renderer hook: what a release or copy key asked of the selection's
    /// text; asks once.
    pub fn take_extract_request(&self) -> Option<Extract> {
        self.extract_requested.take()
    }

    /// Renderer hook: a measured (not copied) selection's text. Blank counts
    /// as nothing, as for [`Self::set_clipboard`].
    pub fn set_measured(&self, text: &str) {
        if let (false, Some(sel)) = (text.trim().is_empty(), self.selection) {
            self.measured.set(Some((text.chars().count(), sel.region)));
        }
    }

    /// The status hint for a selection released without copying, once,
    /// naming a key that reaches us: where the outer terminal keeps
    /// ctrl+shift+c ([`copy_key_intercepted`]), ctrl+c over a dashboard
    /// pane; in the integrated terminal ctrl+c is the shell's, so no key
    /// copies there and only the setting helps.
    pub fn take_selected_hint(&mut self) -> Option<String> {
        let (n, region) = self.measured.take()?;
        let how = match (self.copy_key_intercepted, region) {
            (false, _) => "ctrl-shift-c copies",
            (true, RegionId::Terminal) => "turn on copy on select (?) to copy here",
            (true, _) => "ctrl-c copies",
        };
        Some(format!("selected {n} chars — {how}"))
    }

    /// Renderer hook: the copied text, for the event loop. An all-blank
    /// selection copies nothing rather than clearing the clipboard.
    pub fn set_clipboard(&self, text: String) {
        if !text.trim().is_empty() {
            *self.clipboard.borrow_mut() = Some(text);
        }
    }

    /// Take the text the event loop should write to the clipboard, if any.
    pub fn take_clipboard(&mut self) -> Option<String> {
        self.clipboard.get_mut().take()
    }

    /// The selection's share of a mouse event, ahead of every other handler:
    /// a drag from a pressed cell to another starts the selection, drags move
    /// its head (clamped to its region), the release ends it and asks for
    /// the copy, or with copy on select off only for its size. Returns whether it consumed the event; it doesn't while no
    /// drag-selection is live, so a press stays a click everywhere.
    pub(super) fn selection_mouse(&mut self, ev: &MouseEvent) -> bool {
        match ev.kind {
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(id) = self.selection.filter(|s| s.dragging).map(|s| s.region) {
                    // Past a scrolled region's top or bottom, the head goes
                    // to the row scrolled in: dragging there extends the
                    // selection one row per event.
                    if let (Some(region), Some(sel)) = (self.autoscroll(id, ev.row), self.selection.as_mut()) {
                        sel.head = region.pos_at(ev.column, ev.row);
                    }
                    return true;
                }
                let Some(c) = self.candidate.filter(|c| c.at != (ev.column, ev.row)) else {
                    return false;
                };
                self.candidate = None;
                // Gone from the screen since the press (a click changed the
                // layout): no selection, the drag goes to the handlers.
                let Some(region) = self.region(c.region) else {
                    return false;
                };
                let head = region.pos_at(ev.column, ev.row);
                self.selection = Some(Selection { region: c.region, anchor: c.anchor, head, dragging: true });
                true
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.candidate = None;
                match self.selection.as_mut().filter(|s| s.dragging) {
                    Some(sel) => {
                        sel.dragging = false;
                        let want = if self.settings.copy_on_select { Extract::Copy } else { Extract::Measure };
                        self.extract_requested.set(Some(want));
                        true
                    }
                    None => false,
                }
            }
            _ => false,
        }
    }

    /// A left press, after the other handlers saw it: drop the old selection
    /// (a click elsewhere clears it) and, inside a region, record a
    /// [`Candidate`]. Not when the press grabbed a divider: that drag resizes;
    /// nor when it went to a mouse-tracking terminal child, whose drag it is.
    pub(super) fn selection_press(&mut self, ev: &MouseEvent) {
        self.clear_selection();
        if self.dragging_divider || self.inbox.dragging || self.term_mouse_down.is_some() {
            return;
        }
        let hit = self.regions.borrow().iter().rev().find(|r| r.contains(ev.column, ev.row)).cloned();
        if let Some(region) = hit {
            self.candidate =
                Some(Candidate { region: region.id, anchor: region.pos_at(ev.column, ev.row), at: (ev.column, ev.row) });
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Modifier;

    use super::super::test_support::*;
    use super::super::*;
    use crate::inbox::{Inbox, Kind};
    use crate::tui::select::{Pos, RegionId};

    /// Draw `app` into a [`FRAME`]-sized test terminal, as the event loop
    /// would between events: registers the regions, reads a pending copy.
    fn draw(app: &App) -> ratatui::buffer::Buffer {
        let mut term = Terminal::new(TestBackend::new(FRAME.width, FRAME.height)).unwrap();
        term.draw(|f| crate::tui::ui::draw(f, app)).unwrap();
        term.backend().buffer().clone()
    }

    fn ev(app: &mut App, kind: MouseEventKind, col: u16, row: u16) {
        app.on_mouse(&mouse_at(kind, col, row), FRAME);
    }

    const DOWN: MouseEventKind = MouseEventKind::Down(MouseButton::Left);
    const DRAG: MouseEventKind = MouseEventKind::Drag(MouseButton::Left);
    const UP: MouseEventKind = MouseEventKind::Up(MouseButton::Left);

    /// The Inbox with two notify threads, drawn once so its regions exist.
    fn inbox_app() -> App {
        notes_app(&["first message", "second message"])
    }

    /// The Inbox with a notify thread per message (ids from 1, the first
    /// selected), drawn once; copy on select on (the default), so a release
    /// copies.
    fn notes_app(msgs: &[&str]) -> App {
        let mut app = new_app();
        let note = |id: u64, msg: &str| {
            let mut t = Thread { id, kind: Kind::Notify, owner_name: "builder".into(), unread: true, ..Thread::default() };
            t.notes = vec![crate::inbox::Note {
                id: 1,
                record: crate::devsbd::notify::Record { level: crate::devsbd::notify::Level::Info, key: None, link: None, msg: msg.into(), at: 100 - id },
            }];
            t
        };
        let mut inbox = Inbox::default();
        inbox.threads = msgs.iter().enumerate().map(|(i, m)| note(i as u64 + 1, m)).collect();
        app.set_inbox(inbox);
        app.on_key(key(KeyCode::Char('4')));
        draw(&app);
        app
    }

    fn rows(buf: &ratatui::buffer::Buffer) -> Vec<String> {
        (0..buf.area.height).map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect()).collect()
    }

    #[test]
    fn drag_on_the_thread_pane_copies_its_text_only() {
        let mut app = inbox_app();
        // From the pane's first content row (col 41 would grab the divider)
        // to past its bottom-right corner: clamped to the content area.
        ev(&mut app, DOWN, 42, PANE_TOP);
        ev(&mut app, DRAG, 45, PANE_TOP + 2);
        ev(&mut app, DRAG, 99, 39);
        assert!(app.selection.unwrap().dragging);
        ev(&mut app, UP, 99, 39);
        let buf = draw(&app);
        let text = app.take_clipboard().expect("copied");
        assert!(text.contains("message"), "{text}");
        for border in ["│", "╭", "╰", "Thread", "Inbox"] {
            assert!(!text.contains(border), "{border} in {text:?}");
        }
        assert!(text.lines().all(|l| !l.ends_with(' ')), "{text:?}");
        assert!(buf[(42, PANE_TOP)].modifier.contains(Modifier::REVERSED), "highlighted");
        assert!(!buf[(42, PANE_TOP - 2)].modifier.contains(Modifier::REVERSED), "the header isn't the feed");
        assert!(buf[(98, 37)].modifier.contains(Modifier::REVERSED), "to the end");
        assert!(!buf[(40, 2)].modifier.contains(Modifier::REVERSED), "border untouched");
        // Asked once: the next frame copies nothing again.
        draw(&app);
        assert_eq!(app.take_clipboard(), None);
        assert!(app.selection.is_some(), "stays shown after release");
    }

    #[test]
    fn the_pinned_header_selects_on_its_own() {
        let mut app = inbox_app();
        ev(&mut app, DOWN, 41, PANE_TOP - 3);
        ev(&mut app, DRAG, 98, PANE_TOP - 2);
        ev(&mut app, DRAG, 98, 30); // into the feed: clamped to the header
        ev(&mut app, UP, 98, 30);
        assert_eq!(app.selection.map(|s| s.region), Some(RegionId::InboxHeader));
        draw(&app);
        let text = app.take_clipboard().expect("copied");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text:?}");
        assert_eq!(lines[0], "first message");
        assert!(lines[1].starts_with("· info  ·  ") && lines[1].ends_with(" builder"), "{text:?}");
    }

    #[test]
    fn a_partial_drag_copies_the_cells_under_it() {
        let mut app = inbox_app();
        let screen = rows(&draw(&app));
        // In the feed (the header repeats it as the title).
        let in_feed = |r: &String| r[r.char_indices().nth(41).unwrap().0..].contains("first message");
        let y = PANE_TOP + screen[PANE_TOP as usize..].iter().position(in_feed).unwrap() as u16;
        let x = 41 + screen[y as usize].chars().skip(41).collect::<String>().find("first message").unwrap() as u16;
        // Backwards: from the end of "message" to the start of "first".
        ev(&mut app, DOWN, x + 12, y);
        ev(&mut app, DRAG, x, y);
        ev(&mut app, UP, x, y);
        draw(&app);
        assert_eq!(app.take_clipboard().as_deref(), Some("first message"));
        let sel = app.selection.unwrap();
        assert_eq!(sel.range(), (Pos { row: (y - PANE_TOP) as usize, col: x - 41 }, Pos { row: (y - PANE_TOP) as usize, col: x + 12 - 41 }));
    }

    #[test]
    fn a_click_without_movement_still_selects_a_card() {
        let mut app = inbox_app();
        let shown = |app: &App| app.selected_inbox_thread().map(|t| t.id);
        assert_eq!(shown(&app), Some(1));
        // Card 1 sits on rows 6-7 of the list.
        ev(&mut app, DOWN, 5, 6);
        ev(&mut app, DRAG, 5, 6); // same cell: not a drag
        ev(&mut app, UP, 5, 6);
        assert_eq!(shown(&app), Some(2));
        assert!(app.selection.is_none());
        draw(&app);
        assert_eq!(app.take_clipboard(), None);
    }

    #[test]
    fn a_click_on_a_tab_title_still_switches() {
        let mut app = new_app();
        draw(&app);
        let r = crate::tui::ui::tab_spans(&app).into_iter().find(|(t, _, _)| *t == Tab::Ports).unwrap().2;
        ev(&mut app, DOWN, r.start, 0);
        ev(&mut app, UP, r.start, 0);
        assert_eq!(app.tab, Tab::Ports);
        assert!(app.selection.is_none() && app.candidate.is_none());
    }

    #[test]
    fn a_drag_starting_on_a_border_or_the_tab_bar_selects_nothing() {
        let mut app = inbox_app();
        for (col, row) in [(70, 1), (99, 10), (10, 0), (10, 39)] {
            ev(&mut app, DOWN, col, row);
            ev(&mut app, DRAG, 60, 10);
            ev(&mut app, UP, 60, 10);
            assert!(app.selection.is_none(), "press at ({col}, {row})");
        }
    }

    #[test]
    fn inbox_divider_drag_resizes_and_never_selects() {
        let mut app = inbox_app();
        // Column 40 is the divider (the thread pane's left border).
        ev(&mut app, DOWN, 40, 10);
        assert!(app.inbox.dragging);
        ev(&mut app, DRAG, 60, 10);
        ev(&mut app, UP, 60, 10);
        assert_eq!(app.inbox.split_pct, 60);
        assert!(app.selection.is_none());
        draw(&app);
        assert_eq!(app.take_clipboard(), None);

        // The pane's first text column, right of its border, selects.
        let mut app = inbox_app();
        ev(&mut app, DOWN, 41, 3);
        assert!(!app.inbox.dragging);
        ev(&mut app, DRAG, 60, 3);
        assert!(app.selection.is_some());
    }

    #[test]
    fn config_divider_drag_resizes_and_never_selects() {
        let mut app = new_app();
        open_modal(&mut app);
        draw(&app);
        // Divider at 50 of 100; 51 is inside the right pane's text.
        ev(&mut app, DOWN, 51, 5);
        ev(&mut app, DRAG, 60, 5);
        ev(&mut app, UP, 60, 5);
        assert_eq!(view(&app).split_pct, 60);
        assert!(app.selection.is_none());
        // Off the divider, the modal's panes select.
        draw(&app);
        ev(&mut app, DOWN, 1, 1);
        ev(&mut app, DRAG, 10, 2);
        ev(&mut app, UP, 10, 2);
        draw(&app);
        assert_eq!(app.take_clipboard().as_deref(), Some("[sandbox.s]\nimage = \"a"));
    }

    #[test]
    fn esc_clears_a_selection_and_only_that() {
        let mut app = inbox_app();
        app.on_key(key(KeyCode::Enter)); // thread focused: esc would go back
        draw(&app);
        ev(&mut app, DOWN, 42, 2);
        ev(&mut app, DRAG, 50, 2);
        ev(&mut app, UP, 50, 2);
        assert!(app.selection.is_some());
        app.on_key(key(KeyCode::Esc));
        assert!(app.selection.is_none());
        assert_eq!(app.inbox.focus, InboxFocus::Thread, "esc consumed");
        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.inbox.focus, InboxFocus::List);
    }

    #[test]
    fn tab_switch_and_modal_clear_a_selection() {
        let mut app = inbox_app();
        let select = |app: &mut App| {
            // In the list, there whatever the view holds.
            draw(app);
            ev(app, DOWN, 2, 3);
            ev(app, DRAG, 10, 4);
            assert!(app.selection.is_some());
        };
        select(&mut app);
        app.set_tab(Tab::Ports);
        assert!(app.selection.is_none());
        app.set_tab(Tab::Inbox);
        select(&mut app);
        ev(&mut app, UP, 10, 4);
        app.on_key(key(KeyCode::Char('?')));
        assert!(app.selection.is_none(), "modal open");
        // A selection in the help modal (its text, below the settings rows)
        // goes when it closes.
        draw(&app);
        ev(&mut app, DOWN, 2, 5);
        ev(&mut app, DRAG, 6, 6);
        ev(&mut app, UP, 6, 6);
        assert!(app.selection.is_some());
        app.on_key(key(KeyCode::Char('q')));
        assert!(app.selection.is_none(), "modal closed");
    }

    #[test]
    fn a_press_elsewhere_clears_and_esc_reaches_a_focused_shell() {
        let mut app = inbox_app();
        ev(&mut app, DOWN, 42, 2);
        ev(&mut app, DRAG, 50, 3);
        ev(&mut app, UP, 50, 3);
        ev(&mut app, DOWN, 45, 5);
        assert!(app.selection.is_none());
        // With a terminal focused, esc is the shell's even over a selection.
        open_test_term(&mut app, "web-1", "devsandbox-web-1");
        draw(&app);
        ev(&mut app, DOWN, 42, 2);
        ev(&mut app, DRAG, 50, 3);
        ev(&mut app, UP, 50, 3);
        app.focus = Focus::Terminal;
        app.on_key(key(KeyCode::Esc));
        assert!(app.selection.is_some());
    }

    // The thread pane's feed at FRAME size for a notify thread: columns
    // 41-98, rows 5-37 (rows 2-3 are its pinned header, 4 the separator).
    const PANE_TOP: u16 = 5;
    const PANE_BOTTOM: u16 = 37;

    /// Drag from the thread pane's first cell to past its bottom until it
    /// stops scrolling, release, and return the copy.
    fn copy_whole_pane(app: &mut App) -> String {
        ev(app, DOWN, 41, PANE_TOP);
        ev(app, DRAG, 98, PANE_BOTTOM);
        for _ in 0..200 {
            ev(app, DRAG, 98, PANE_BOTTOM + 2);
        }
        ev(app, UP, 98, PANE_BOTTOM + 2);
        draw(app);
        app.take_clipboard().expect("copied")
    }

    const DOC: &str = "\
# Build report

The build **finished** with two warnings; see [the PR](https://example.com/pr/1).

- first bullet
- second bullet

1. step one
2. step two

> quoted note

```rust
fn main() { println!(\"hi\"); }
```

| name | state |
|------|-------|
| api  | ok    |
";

    #[test]
    fn a_rendered_markdown_copy_keeps_what_the_pane_shows() {
        let mut app = notes_app(&[DOC]);
        let text = copy_whole_pane(&mut app);
        for want in [
            "Build report",
            "The build finished with two warnings; see the PR (https://example.com/pr/1).",
            "• first bullet\n• second bullet",
            "1. step one\n2. step two",
            "│ quoted note",
            "fn main() { println!(\"hi\"); }",
            "name  state\n───────────\napi   ok",
        ] {
            assert!(text.contains(want), "{want:?} not in {text:?}");
        }
        // No code-block fill or other trailing blanks.
        assert!(text.lines().all(|l| !l.ends_with(' ')), "{text:?}");
    }

    #[test]
    fn soft_wrapped_rows_copy_as_one_line_without_their_decoration() {
        let long = "the quick brown fox jumps over the lazy dog and keeps running past the fence into the far field at dusk while the farmer watches";
        let mut app = notes_app(&[&format!("- {long}\n\n> {long}\n\n{long}")]);
        // 58 columns: each takes 2-3 rows.
        let rows = crate::tui::markdown::render_rows(&format!("> {long}"), 58);
        assert_eq!(rows.len(), 3);
        let text = copy_whole_pane(&mut app);
        assert!(text.contains(&format!("• {long}\n")), "{text:?}");
        assert!(text.contains(&format!("│ {long}\n")), "{text:?}");
        assert!(text.ends_with(&format!("\n{long}")), "{text:?}");
    }

    #[test]
    fn a_hard_wrapped_code_line_copies_without_inserted_spaces() {
        // The break falls right after the space: the first row is the a's
        // and it, the space must neither be lost nor doubled.
        let code = format!("{} {} end", "a".repeat(57), "b".repeat(70));
        let mut app = notes_app(&[&format!("```\n{code}\n  indented\n```")]);
        let text = copy_whole_pane(&mut app);
        assert!(text.contains(&format!("{code}\n  indented")), "{text:?}");
    }

    /// A code block of 80 numbered rows: far taller than the pane.
    fn tall_app() -> App {
        let body: String = (1..=80).map(|i| format!("row {i:02}\n")).collect();
        notes_app(&[&format!("```\n{body}```")])
    }

    #[test]
    fn a_drag_past_the_bottom_autoscrolls_and_copies_off_screen_rows() {
        let mut app = tall_app();
        assert!(!rows(&draw(&app)).iter().any(|r| r.contains("row 80")), "off screen");
        ev(&mut app, DOWN, 41, PANE_TOP);
        ev(&mut app, DRAG, 60, 10);
        assert_eq!(app.inbox.scroll, 0);
        // One row per drag event below the pane, up to the bound.
        ev(&mut app, DRAG, 60, PANE_BOTTOM + 1);
        assert_eq!(app.inbox.scroll, 1);
        assert_eq!(app.selection.unwrap().head.row, (PANE_BOTTOM - PANE_TOP + 1) as usize);
        for _ in 0..200 {
            ev(&mut app, DRAG, 60, PANE_BOTTOM + 1);
        }
        let max = app.inbox.scroll;
        assert!(max > 40, "{max}");
        ev(&mut app, UP, 60, PANE_BOTTOM + 1);
        let buf = draw(&app);
        let text = app.take_clipboard().expect("copied");
        assert!(text.contains("row 01\nrow 02") && text.ends_with("row 79\nrow 80"), "{text:?}");
        // Highlighted where it is on screen now: the bottom row, scrolled in.
        assert!(buf[(41, PANE_BOTTOM)].modifier.contains(Modifier::REVERSED));

        // The wheel scrolls the pane and keeps the selection (content rows).
        let sel = app.selection.unwrap();
        ev(&mut app, MouseEventKind::ScrollUp, 60, 10);
        assert_eq!(app.inbox.scroll, max - 3);
        assert_eq!(app.selection, Some(sel));
        draw(&app);
        assert_eq!(app.selection, Some(sel), "a redraw keeps it too");
    }

    #[test]
    fn a_drag_above_the_top_autoscrolls_up() {
        let mut app = tall_app();
        for _ in 0..30 {
            ev(&mut app, MouseEventKind::ScrollDown, 60, 10);
        }
        assert!(app.inbox.scroll > 40);
        draw(&app);
        ev(&mut app, DOWN, 98, PANE_BOTTOM);
        ev(&mut app, DRAG, 60, 10);
        for _ in 0..200 {
            ev(&mut app, DRAG, 60, PANE_TOP - 1); // the separator row
        }
        assert_eq!(app.inbox.scroll, 0);
        ev(&mut app, UP, 60, PANE_TOP - 1);
        draw(&app);
        let text = app.take_clipboard().expect("copied");
        assert!(text.contains("row 01\nrow 02") && text.contains("row 79"), "{text:?}");
    }

    #[test]
    fn a_raw_mode_copy_equals_the_source_lines() {
        let long = "a long **source** line that the raw view wraps over two rows, at least, at this width";
        let src = format!("# Title\n\n- item `code`\n{long}\n> quote");
        let mut app = notes_app(&[&src]);
        app.on_key(key(KeyCode::Enter)); // the thread focused: `m` is its key
        draw(&app);
        ev(&mut app, DOWN, 42, PANE_TOP);
        ev(&mut app, DRAG, 50, PANE_TOP + 1);
        app.on_key(key(KeyCode::Char('m')));
        assert!(app.inbox.raw);
        assert!(app.selection.is_none(), "`m` re-renders the rows under it");
        draw(&app);
        let text = copy_whole_pane(&mut app);
        assert!(text.ends_with(&src), "{text:?}");
    }

    #[test]
    fn a_config_line_wider_than_its_pane_copies_whole_from_the_right_edge() {
        let mut app = new_app();
        open_modal(&mut app);
        let wide = format!("image = \"{}\"", "x".repeat(100));
        if let Modal::Config(v) = &mut app.modal {
            v.original = format!("[sandbox.s]\n{wide}\nnext = 1\n");
        }
        draw(&app);
        // The left pane's text is columns 1-48; 49 is its right border.
        let copy = |app: &mut App, from: (u16, u16), to: (u16, u16)| {
            ev(app, DOWN, from.0, from.1);
            ev(app, DRAG, to.0, to.1);
            ev(app, UP, to.0, to.1);
            draw(app);
            app.take_clipboard().expect("copied")
        };
        assert_eq!(copy(&mut app, (1, 2), (49, 2)), wide);
        assert_eq!(copy(&mut app, (1, 2), (48, 2)), wide, "the last visible column");
        assert_eq!(copy(&mut app, (1, 2), (5, 2)), "image", "short of it: what's under the drag");
        // A middle row is whole whatever the columns.
        assert_eq!(copy(&mut app, (2, 1), (3, 3)), format!("sandbox.s]\n{wide}\nnex"));

        // Scrolled: the positions are content rows, so the copy is too.
        ev(&mut app, MouseEventKind::ScrollDown, 10, 10);
        assert_eq!(view(&app).scroll, 2);
        draw(&app);
        assert_eq!(copy(&mut app, (1, 1), (4, 1)), "next");
    }

    /// A terminal sized to its panel's body (as the event loop does before
    /// a draw) that printed `output`, and the body; copy on select on (the
    /// default).
    fn term_app(output: &str) -> (App, ratatui::layout::Rect) {
        let mut app = new_app();
        app.terms.open(term_sess("web-1", "devsandbox-web-1"));
        let body = ratatui::widgets::Block::bordered().inner(crate::tui::ui::terminal_panel_rect(FRAME, false));
        let session = app.terms.active_session_mut().unwrap();
        session.resize(body.height, body.width);
        session.feed_output(output.as_bytes());
        draw(&app);
        (app, body)
    }

    #[test]
    fn a_shell_without_mouse_tracking_selects_locally() {
        let (mut app, b) = term_app("hello world\r\nsecond line\r\n$ ");
        // From the body's first cell to "second", dragging from below.
        ev(&mut app, DOWN, b.x + 5, b.y + 1);
        ev(&mut app, DRAG, b.x, b.y);
        ev(&mut app, UP, b.x, b.y);
        let buf = draw(&app);
        assert_eq!(app.take_clipboard().as_deref(), Some("hello world\nsecond"));
        assert!(buf[(b.x, b.y)].modifier.contains(Modifier::REVERSED));
        assert!(!buf[(b.x - 1, b.y)].modifier.contains(Modifier::REVERSED), "border untouched");
        // Nothing went to the shell.
        assert!(app.terms.active_session().unwrap().take_written().is_empty());
        // Past the panel's edges: clamped to the body, no border or tab title.
        ev(&mut app, DOWN, b.x + 2, b.y);
        ev(&mut app, DRAG, 0, 0);
        ev(&mut app, DRAG, 200, 200);
        ev(&mut app, UP, 200, 200);
        draw(&app);
        assert_eq!(app.take_clipboard().as_deref(), Some("llo world\nsecond line\n$"));
    }

    #[test]
    fn switching_terminal_tabs_clears_a_terminal_selection() {
        let (mut app, b) = term_app("hello world\r\n");
        app.terms.open(term_sess("api-2", "devsandbox-api-2"));
        app.terms.set_active(0);
        draw(&app);
        ev(&mut app, DOWN, b.x, b.y);
        ev(&mut app, DRAG, b.x + 4, b.y);
        ev(&mut app, UP, b.x + 4, b.y);
        assert!(app.selection.is_some());
        app.focus = Focus::Dashboard;
        app.on_key(key(KeyCode::Char(']')));
        assert_eq!(app.terms.active(), 1);
        assert!(app.selection.is_none(), "would read the other session's rows");
    }

    #[test]
    fn a_terminal_selection_spans_the_scrollback() {
        let out: String = (0..100).map(|i| format!("line {i:02}\r\n")).collect();
        let (mut app, b) = term_app(&out);
        let offset = |app: &App| app.terms.active_session().unwrap().parser().lock().unwrap().screen().scrollback();
        // "line 99" sits right above the cursor's empty last row.
        ev(&mut app, DOWN, b.x + 6, b.bottom() - 2);
        ev(&mut app, DRAG, b.x + 3, b.y + 1);
        // One line of scrollback per drag above the body, to its oldest.
        ev(&mut app, DRAG, b.x, b.y - 1);
        assert_eq!(offset(&app), 1);
        for _ in 0..200 {
            ev(&mut app, DRAG, b.x, b.y - 1);
        }
        let max = offset(&app);
        assert_eq!(max, 100 + 1 - b.height as usize);
        ev(&mut app, UP, b.x, b.y - 1);
        let buf = draw(&app);
        let text = app.take_clipboard().expect("copied");
        let want: Vec<String> = (0..100).map(|i| format!("line {i:02}")).collect();
        assert_eq!(text, want.join("\n"));
        assert!(buf[(b.x, b.y)].modifier.contains(Modifier::REVERSED), "the top, scrolled in");
        // The wheel scrolls back down and keeps the selection; the copy
        // still reads rows out of view.
        let sel = app.selection.unwrap();
        ev(&mut app, MouseEventKind::ScrollDown, b.x + 2, b.y + 2);
        assert_eq!(offset(&app), max - 3);
        assert_eq!(app.selection, Some(sel));
        // New output doesn't drift it: rows count from the oldest line.
        app.terms.active_session().unwrap().feed_output(b"more\r\n");
        draw(&app);
        assert_eq!(app.selection, Some(sel));
        app.extract_requested.set(Some(Extract::Copy));
        draw(&app);
        assert_eq!(app.take_clipboard().as_deref(), Some(&*want.join("\n")));
    }

    #[test]
    fn a_press_forwarded_to_a_tracking_child_forms_no_selection() {
        // Drawn while not tracking: the body was a region. The child turns
        // tracking on before the next frame; the press is the child's.
        let (mut app, b) = term_app("text\r\n");
        app.terms.active_session().unwrap().feed_output(b"\x1b[?1002h\x1b[?1006h");
        ev(&mut app, DOWN, b.x, b.y);
        ev(&mut app, DRAG, b.x + 3, b.y);
        ev(&mut app, UP, b.x + 3, b.y);
        assert!(app.selection.is_none() && app.candidate.is_none());
        assert_eq!(app.terms.active_session().unwrap().take_written(), b"\x1b[<0;1;1M\x1b[<32;4;1M\x1b[<0;4;1m");
        draw(&app);
        assert_eq!(app.take_clipboard(), None);
        // And while it tracks, the body is no region at all.
        assert!(app.selection_in_frame().is_none());
        assert!(!app.regions.borrow().iter().any(|r| r.id == RegionId::Terminal));
    }

    fn mods(code: char, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(code), modifiers)
    }

    #[test]
    fn the_copy_shortcut_copies_the_selection_again() {
        let mut app = inbox_app();
        ev(&mut app, DOWN, 42, 2);
        ev(&mut app, DRAG, 60, 4);
        ev(&mut app, UP, 60, 4);
        draw(&app);
        let first = app.take_clipboard().expect("copied on release");
        let ctrl_shift = KeyModifiers::CONTROL | KeyModifiers::SHIFT;
        for key in [mods('C', ctrl_shift), mods('c', ctrl_shift), mods('c', KeyModifiers::SUPER)] {
            app.on_key(key);
            assert!(!app.should_quit, "{key:?}");
            draw(&app);
            assert_eq!(app.take_clipboard().as_ref(), Some(&first), "{key:?}");
            assert!(app.selection.is_some());
        }
        // Plain `c` is no copy key.
        app.on_key(mods('c', KeyModifiers::NONE));
        draw(&app);
        assert_eq!(app.take_clipboard(), None);
        // Ctrl+c (a legacy ctrl+shift+c) copies over a selection, quits without.
        app.on_key(mods('c', KeyModifiers::CONTROL));
        assert!(!app.should_quit);
        draw(&app);
        assert_eq!(app.take_clipboard().as_ref(), Some(&first));
        app.on_key(key(KeyCode::Esc));
        app.on_key(mods('c', KeyModifiers::CONTROL));
        assert!(app.should_quit);
    }

    #[test]
    fn the_copy_shortcut_in_a_focused_terminal() {
        let (mut app, b) = term_app("hello world\r\n");
        ev(&mut app, DOWN, b.x, b.y);
        ev(&mut app, DRAG, b.x + 4, b.y);
        ev(&mut app, UP, b.x + 4, b.y);
        assert_eq!(app.focus, Focus::Terminal);
        draw(&app);
        assert_eq!(app.take_clipboard().as_deref(), Some("hello"));
        let written = |app: &App| app.terms.active_session().unwrap().take_written();
        let copy = mods('C', KeyModifiers::CONTROL | KeyModifiers::SHIFT);
        // Over a selection: copied again, the shell sees nothing.
        app.on_key(copy);
        draw(&app);
        assert_eq!(app.take_clipboard().as_deref(), Some("hello"));
        assert!(written(&app).is_empty());
        // Without one the key is the shell's, as before.
        app.clear_selection();
        app.on_key(copy);
        draw(&app);
        assert_eq!(app.take_clipboard(), None);
        assert!(!written(&app).is_empty(), "reaches the PTY");
        assert_eq!(app.focus, Focus::Terminal);
    }

    #[test]
    fn a_thread_change_clears_the_selection() {
        let mut app = notes_app(&["one", "two", "three"]);
        let select = |app: &mut App| {
            draw(app);
            ev(app, DOWN, 42, PANE_TOP);
            ev(app, DRAG, 50, PANE_TOP + 1);
            ev(app, UP, 50, PANE_TOP + 1);
            assert!(app.selection.is_some());
        };
        select(&mut app);
        // The wheel over the list moves the cursor to the next thread.
        ev(&mut app, MouseEventKind::ScrollDown, 5, 10);
        assert_eq!(app.selected_inbox_thread().map(|t| t.id), Some(2));
        assert!(app.selection.is_none());
        draw(&app);
        assert_eq!(app.take_clipboard(), None);
        // So does a key moving it.
        select(&mut app);
        app.inbox.focus = InboxFocus::List;
        app.on_key(key(KeyCode::Char('j')));
        assert_eq!(app.selected_inbox_thread().map(|t| t.id), Some(3));
        assert!(app.selection.is_none());
    }

    #[test]
    fn without_copy_on_select_a_release_only_counts_and_the_keys_copy() {
        let mut app = inbox_app();
        app.settings.copy_on_select = false;
        let screen = rows(&draw(&app));
        let y = screen.iter().position(|r| r[r.char_indices().nth(41).unwrap().0..].contains("first message")).unwrap() as u16;
        let x = 41 + screen[y as usize].chars().skip(41).collect::<String>().find("first message").unwrap() as u16;
        ev(&mut app, DOWN, x, y);
        ev(&mut app, DRAG, x + 12, y);
        ev(&mut app, UP, x + 12, y);
        let buf = draw(&app);
        assert_eq!(app.take_clipboard(), None, "nothing copied");
        assert_eq!(app.take_selected_hint().as_deref(), Some("selected 13 chars — ctrl-shift-c copies"));
        assert_eq!(app.take_selected_hint(), None, "once");
        assert!(app.selection.is_some_and(|s| !s.dragging), "kept");
        assert!(buf[(x, y)].modifier.contains(Modifier::REVERSED), "still highlighted");
        // Every copy key copies whatever the setting.
        for k in [mods('C', KeyModifiers::CONTROL | KeyModifiers::SHIFT), mods('c', KeyModifiers::SUPER), mods('c', KeyModifiers::CONTROL)] {
            app.on_key(k);
            draw(&app);
            assert_eq!(app.take_clipboard().as_deref(), Some("first message"), "{k:?}");
            assert_eq!(app.take_selected_hint(), None, "{k:?}");
        }
    }

    #[test]
    fn with_copy_on_select_a_release_copies_and_shows_no_count() {
        let mut app = inbox_app();
        assert!(app.settings.copy_on_select, "the default");
        ev(&mut app, DOWN, 42, 2);
        ev(&mut app, DRAG, 60, 4);
        ev(&mut app, UP, 60, 4);
        draw(&app);
        assert!(app.take_clipboard().is_some());
        assert_eq!(app.take_selected_hint(), None);
    }

    #[test]
    fn the_help_text_stays_selectable_below_the_settings() {
        let mut app = new_app();
        // Off, so the copy below is the key's, not the release's.
        app.settings.copy_on_select = false;
        let before = app.settings;
        app.on_key(key(KeyCode::Char('?')));
        let screen = rows(&draw(&app));
        let y = screen.iter().position(|r| r.contains("Global")).unwrap() as u16;
        assert!(y > crate::tui::settings::SETTINGS.len() as u16, "under the settings rows");
        ev(&mut app, DOWN, 1, y);
        ev(&mut app, DRAG, 6, y);
        ev(&mut app, UP, 6, y);
        app.on_key(mods('C', KeyModifiers::CONTROL | KeyModifiers::SHIFT));
        draw(&app);
        assert_eq!(app.take_clipboard().as_deref(), Some("Global"));
        assert_eq!(app.settings, before, "no toggle");
        // A drag from a settings row toggles it and selects nothing.
        ev(&mut app, DOWN, 3, 1);
        ev(&mut app, DRAG, 10, y);
        ev(&mut app, UP, 10, y);
        assert!(app.selection.is_none());
        assert!(app.settings.copy_on_select);
    }

    #[test]
    fn copy_key_detection_from_the_terminal_env() {
        let detect = |vars: &[(&str, &str)]| {
            let vars: Vec<(String, String)> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
            copy_key_intercepted(move |var| vars.iter().find(|(k, _)| k == var).map(|(_, v)| v.clone()))
        };
        for env in [
            ("ALACRITTY_WINDOW_ID", "94371851"),
            ("ALACRITTY_SOCKET", "/run/user/1000/Alacritty-wayland-1-42.sock"),
            ("TERM", "alacritty"),
            ("KITTY_WINDOW_ID", "1"),
            ("TERM", "xterm-kitty"),
            ("WEZTERM_PANE", "0"),
            ("WEZTERM_EXECUTABLE", "/usr/bin/wezterm-gui"),
            ("VTE_VERSION", "7600"),
            ("KONSOLE_VERSION", "230805"),
            ("TERM", "foot"),
            ("TERM", "foot-extra"),
            ("TERM_PROGRAM", "ghostty"),
            ("GHOSTTY_RESOURCES_DIR", "/usr/share/ghostty"),
            ("WT_SESSION", "0f7b9c2e-0000-0000-0000-000000000000"),
            ("TERM_PROGRAM", "vscode"),
        ] {
            assert!(detect(&[env, ("TERM_PROGRAM_VERSION", "1")]), "{env:?}");
        }
        assert!(!detect(&[]), "empty env");
        assert!(!detect(&[("TERM", "xterm-256color")]));
        assert!(!detect(&[("VTE_VERSION", "")]), "set but empty");
    }

    /// What the hint a copy-on-select-off release shows says after the
    /// `selected N chars — ` count, over the terminal or a dashboard pane.
    fn hint(intercepted: bool, terminal: bool) -> Option<String> {
        let (mut app, (col, row)) = if terminal {
            let (app, b) = term_app("hello world\r\n");
            (app, (b.x, b.y))
        } else {
            (inbox_app(), (42, 2))
        };
        app.settings.copy_on_select = false;
        app.copy_key_intercepted = intercepted;
        ev(&mut app, DOWN, col, row);
        ev(&mut app, DRAG, col + 4, row);
        ev(&mut app, UP, col + 4, row);
        draw(&app);
        assert_eq!(app.take_clipboard(), None);
        assert_eq!(app.selection.map(|s| s.region == RegionId::Terminal), Some(terminal));
        let hint = app.take_selected_hint()?;
        assert!(hint.starts_with("selected ") && hint.contains(" chars — "), "{hint}");
        hint.split_once(" — ").map(|(_, how)| how.to_string())
    }

    #[test]
    fn the_hint_names_a_copy_key_that_reaches_us() {
        assert_eq!(hint(false, false).as_deref(), Some("ctrl-shift-c copies"));
        assert_eq!(hint(false, true).as_deref(), Some("ctrl-shift-c copies"));
        assert_eq!(hint(true, false).as_deref(), Some("ctrl-c copies"));
        // In the terminal ctrl-c is the shell's: no key copies there.
        assert_eq!(hint(true, true).as_deref(), Some("turn on copy on select (?) to copy here"));
    }
}
