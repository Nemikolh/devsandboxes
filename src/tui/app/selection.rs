//! Mouse text selection routing (docs/tui-selection.md, *Routing*). The
//! regions come from the last draw (`ui` registers them through the hooks
//! here), so a press is hit-tested against what is on screen; the selected
//! text is read off the next frame by `ui::draw` and sent by the event loop.

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use crate::tui::select::{Candidate, Region, RegionId, Selection, Source};

use super::App;

impl App {
    /// Renderer hook: forget the last frame's regions, before drawing a new one.
    pub fn clear_regions(&self) {
        self.regions.borrow_mut().clear();
    }

    /// Renderer hook: `rect` (a pane's inner text area) is selectable as `id`.
    pub fn add_region(&self, id: RegionId, rect: Rect) {
        if !rect.is_empty() {
            self.regions.borrow_mut().push(Region { id, rect, source: Source::Screen });
        }
    }

    fn region(&self, id: RegionId) -> Option<Region> {
        self.regions.borrow().iter().find(|r| r.id == id).cloned()
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
        self.copy_requested.set(false);
    }

    /// Renderer hook: whether a release asked for the selection's text; asks
    /// once.
    pub fn take_copy_request(&self) -> bool {
        self.copy_requested.take()
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
    /// the copy. Returns whether it consumed the event; it doesn't while no
    /// drag-selection is live, so a press stays a click everywhere.
    pub(super) fn selection_mouse(&mut self, ev: &MouseEvent) -> bool {
        match ev.kind {
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(sel) = self.selection.as_mut().filter(|s| s.dragging) {
                    let Some(region) = self.regions.borrow().iter().find(|r| r.id == sel.region).cloned() else {
                        return true;
                    };
                    sel.head = region.pos_at(ev.column, ev.row);
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
                        self.copy_requested.set(true);
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
    /// [`Candidate`]. Not when the press grabbed a divider: that drag resizes.
    pub(super) fn selection_press(&mut self, ev: &MouseEvent) {
        self.clear_selection();
        if self.dragging_divider || self.inbox.dragging {
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
    use crate::tui::select::Pos;

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
        inbox.threads = vec![note(1, "first message"), note(2, "second message")];
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
        ev(&mut app, DOWN, 42, 2);
        ev(&mut app, DRAG, 45, 4);
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
        assert!(buf[(42, 2)].modifier.contains(Modifier::REVERSED), "highlighted");
        assert!(buf[(98, 37)].modifier.contains(Modifier::REVERSED), "to the end");
        assert!(!buf[(40, 2)].modifier.contains(Modifier::REVERSED), "border untouched");
        // Asked once: the next frame copies nothing again.
        draw(&app);
        assert_eq!(app.take_clipboard(), None);
        assert!(app.selection.is_some(), "stays shown after release");
    }

    #[test]
    fn a_partial_drag_copies_the_cells_under_it() {
        let mut app = inbox_app();
        let screen = rows(&draw(&app));
        let y = screen.iter().position(|r| r[r.char_indices().nth(41).unwrap().0..].contains("first message")).unwrap() as u16;
        let x = 41 + screen[y as usize].chars().skip(41).collect::<String>().find("first message").unwrap() as u16;
        // Backwards: from the end of "message" to the start of "first".
        ev(&mut app, DOWN, x + 12, y);
        ev(&mut app, DRAG, x, y);
        ev(&mut app, UP, x, y);
        draw(&app);
        assert_eq!(app.take_clipboard().as_deref(), Some("first message"));
        let sel = app.selection.unwrap();
        assert_eq!(sel.range(), (Pos { row: (y - 2) as usize, col: x - 41 }, Pos { row: (y - 2) as usize, col: x + 12 - 41 }));
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
        // A selection in the help modal goes when it closes.
        draw(&app);
        ev(&mut app, DOWN, 2, 2);
        ev(&mut app, DRAG, 6, 3);
        ev(&mut app, UP, 6, 3);
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
}
