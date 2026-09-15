//! Dashboard state machine. Deliberately free of terminal I/O so key handling
//! and selection logic stay unit-testable; `mod.rs` owns the crossterm/ratatui
//! side and feeds decoded key events in here.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// The two top-level views. Real content lands in steps 3–4.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tab {
    Instances,
    Services,
}

impl Tab {
    pub const ALL: [Tab; 2] = [Tab::Instances, Tab::Services];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Instances => "Instances",
            Tab::Services => "Services",
        }
    }

    fn index(self) -> usize {
        match self {
            Tab::Instances => 0,
            Tab::Services => 1,
        }
    }
}

pub struct App {
    pub dir: PathBuf,
    pub tab: Tab,
    /// Selected row per tab, indexed by `Tab::index`.
    selected: [usize; 2],
    pub should_quit: bool,
}

impl App {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir, tab: Tab::Instances, selected: [0, 0], should_quit: false }
    }

    /// Selected row index for the active tab.
    pub fn selected(&self) -> usize {
        self.selected[self.tab.index()]
    }

    /// Placeholder row count for the active tab until real data lands in
    /// steps 3–4. Zero means "nothing to select".
    fn row_count(&self) -> usize {
        0
    }

    pub fn next_tab(&mut self) {
        self.tab = match self.tab {
            Tab::Instances => Tab::Services,
            Tab::Services => Tab::Instances,
        };
    }

    pub fn prev_tab(&mut self) {
        // Only two tabs, so previous is the same toggle as next.
        self.next_tab();
    }

    fn select_up(&mut self) {
        let slot = self.tab.index();
        self.selected[slot] = self.selected[slot].saturating_sub(1);
    }

    fn select_down(&mut self) {
        let rows = self.row_count();
        if rows == 0 {
            return;
        }
        let slot = self.tab.index();
        self.selected[slot] = (self.selected[slot] + 1).min(rows - 1);
    }

    /// Apply a key event to the state. No terminal I/O here.
    pub fn on_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('c') if ctrl => self.should_quit = true,
            KeyCode::Tab => self.next_tab(),
            KeyCode::BackTab => self.prev_tab(),
            KeyCode::Char('1') => self.tab = Tab::Instances,
            KeyCode::Char('2') => self.tab = Tab::Services,
            KeyCode::Up | KeyCode::Char('k') => self.select_up(),
            KeyCode::Down | KeyCode::Char('j') => self.select_down(),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_app() -> App {
        App::new(PathBuf::from("."))
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn tab_switching() {
        let mut app = new_app();
        assert_eq!(app.tab, Tab::Instances);

        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.tab, Tab::Services);

        app.on_key(key(KeyCode::BackTab));
        assert_eq!(app.tab, Tab::Instances);

        app.on_key(key(KeyCode::Char('2')));
        assert_eq!(app.tab, Tab::Services);

        app.on_key(key(KeyCode::Char('1')));
        assert_eq!(app.tab, Tab::Instances);
    }

    #[test]
    fn quit_keys() {
        let mut app = new_app();
        app.on_key(key(KeyCode::Char('q')));
        assert!(app.should_quit);

        let mut app = new_app();
        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.should_quit);

        // Plain 'c' must not quit.
        let mut app = new_app();
        app.on_key(key(KeyCode::Char('c')));
        assert!(!app.should_quit);
    }

    #[test]
    fn selection_clamps() {
        let mut app = new_app();
        // Up at the top stays at 0.
        app.on_key(key(KeyCode::Up));
        assert_eq!(app.selected(), 0);

        // With zero placeholder rows, down never moves past 0.
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Char('j')));
        assert_eq!(app.selected(), 0);
    }
}
