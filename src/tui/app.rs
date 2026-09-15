//! Dashboard state machine. Deliberately free of terminal I/O so key handling
//! and selection logic stay unit-testable; `mod.rs` owns the crossterm/ratatui
//! side and feeds decoded key events in here.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config::Config;

use super::data::Snapshot;
use super::prompt::{Prompt, PromptAction, COMMANDS};

/// Which side of a [`ConfigView`] is currently shown.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Side {
    Original,
    Resolved,
}

/// Full-screen config explorer state. Built once at open time (fs + config are
/// read then, not during rendering); on error `original`/`resolved` carry the
/// error text so the modal still renders.
pub struct ConfigView {
    /// Sandbox (or service) name, shown in the title.
    pub title: String,
    /// Raw table TOML as written (plus `extends` for sandboxes).
    pub original: String,
    /// Merged table TOML. Equal to `original` for services.
    pub resolved: String,
    pub showing: Side,
    pub scroll: u16,
    /// Short config hash of the resolved table, empty when unavailable.
    pub hash: String,
}

impl ConfigView {
    /// The body for the current side.
    pub fn body(&self) -> &str {
        match self.showing {
            Side::Original => &self.original,
            Side::Resolved => &self.resolved,
        }
    }

    /// Line count of the current side, for scroll clamping.
    fn line_count(&self) -> u16 {
        self.body().lines().count().min(u16::MAX as usize) as u16
    }

    /// Clamp `scroll` into `[0, line_count)` (0 when empty).
    fn clamp_scroll(&mut self) {
        let max = self.line_count().saturating_sub(1);
        if self.scroll > max {
            self.scroll = max;
        }
    }
}

/// Overlay state. `None` is the normal dashboard; `Config` is the explorer modal.
pub enum Modal {
    None,
    Config(ConfigView),
}

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
    /// Latest data collected off-thread; `None` until the first snapshot lands.
    pub snapshot: Option<Snapshot>,
    /// Active overlay, if any.
    pub modal: Modal,
    /// Command prompt, when open (`:`). Occupies the bottom bar and swallows keys.
    pub prompt: Option<Prompt>,
    /// Action awaiting execution by the event loop (it owns the terminal
    /// suspend + command call, keeping [`App`] I/O-free).
    pub pending_action: Option<PromptAction>,
    /// One-line status shown in the help-bar area (e.g. `code` launch outcome).
    pub status: Option<String>,
    pub should_quit: bool,
}

impl App {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            tab: Tab::Instances,
            selected: [0, 0],
            snapshot: None,
            modal: Modal::None,
            prompt: None,
            pending_action: None,
            status: None,
            should_quit: false,
        }
    }

    /// Install a freshly collected snapshot and re-clamp the Instances selection
    /// in case rows shrank. I/O-free: the caller does the collecting.
    pub fn set_snapshot(&mut self, snapshot: Snapshot) {
        self.snapshot = Some(snapshot);
        self.clamp_selection();
    }

    /// Clamp every tab's selection into `[0, row_count)` (or 0 when empty).
    fn clamp_selection(&mut self) {
        for tab in Tab::ALL {
            let rows = self.row_count_for(tab);
            let slot = tab.index();
            if rows == 0 {
                self.selected[slot] = 0;
            } else if self.selected[slot] >= rows {
                self.selected[slot] = rows - 1;
            }
        }
    }

    /// Selected row index for the active tab.
    pub fn selected(&self) -> usize {
        self.selected[self.tab.index()]
    }

    /// Row count for the active tab. Zero means "nothing to select".
    fn row_count(&self) -> usize {
        self.row_count_for(self.tab)
    }

    fn row_count_for(&self, tab: Tab) -> usize {
        match tab {
            Tab::Instances => {
                self.snapshot.as_ref().map_or(0, |s| s.instances.len())
            }
            Tab::Services => self.snapshot.as_ref().map_or(0, |s| s.services.len()),
        }
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

    /// Apply a key event to the state. No terminal I/O here (the modal open path
    /// reads config/fs, which is local and user-triggered — see [`Self::open_config`]).
    pub fn on_key(&mut self, key: KeyEvent) {
        // The prompt swallows every key while open, ahead of the modal and the
        // dashboard bindings.
        if self.prompt.is_some() {
            self.on_key_prompt(key);
            return;
        }
        // The modal swallows every key while it is up; the tab bar and tables
        // must not react underneath it.
        if let Modal::Config(_) = self.modal {
            self.on_key_modal(key);
            return;
        }
        // Any dashboard key dismisses a lingering status line.
        self.status = None;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('c') if ctrl => self.should_quit = true,
            KeyCode::Char(':') => self.open_prompt(),
            KeyCode::Tab => self.next_tab(),
            KeyCode::BackTab => self.prev_tab(),
            KeyCode::Char('1') => self.tab = Tab::Instances,
            KeyCode::Char('2') => self.tab = Tab::Services,
            KeyCode::Up | KeyCode::Char('k') => self.select_up(),
            KeyCode::Down | KeyCode::Char('j') => self.select_down(),
            KeyCode::Enter | KeyCode::Char('e') => self.open_config(),
            _ => {}
        }
    }

    /// Open the command prompt, loading persisted history. Clears any status.
    fn open_prompt(&mut self) {
        self.status = None;
        self.prompt = Some(Prompt::new(super::prompt::load_history()));
    }

    /// Key handling while the prompt is open. `esc` cancels, `enter` parses
    /// (a parse error stays inline and keeps the prompt open), `tab` completes,
    /// everything else edits the line. Assumes `self.prompt` is `Some`.
    fn on_key_prompt(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // Candidate lists are read here (config/snapshot) so the prompt stays
        // data-agnostic; computed only on tab.
        match key.code {
            KeyCode::Esc => {
                self.prompt = None;
            }
            KeyCode::Char('c') if ctrl => {
                self.prompt = None;
            }
            KeyCode::Enter => {
                let Some(prompt) = &mut self.prompt else {
                    return;
                };
                if let Some(action) = prompt.parse() {
                    let line = prompt.input().to_string();
                    super::prompt::append_history(&line);
                    self.pending_action = Some(action);
                    self.prompt = None;
                }
            }
            KeyCode::Tab => {
                let instances = self.instance_names();
                let sandboxes = self.sandbox_names();
                if let Some(prompt) = &mut self.prompt {
                    prompt.complete(|idx, first| {
                        Self::candidates_for(idx, first, &sandboxes, &instances)
                    });
                }
            }
            KeyCode::Up => {
                if let Some(prompt) = &mut self.prompt {
                    prompt.history_prev();
                }
            }
            KeyCode::Down => {
                if let Some(prompt) = &mut self.prompt {
                    prompt.history_next();
                }
            }
            KeyCode::Left => self.with_prompt(Prompt::left),
            KeyCode::Right => self.with_prompt(Prompt::right),
            KeyCode::Home => self.with_prompt(Prompt::home),
            KeyCode::End => self.with_prompt(Prompt::end),
            KeyCode::Backspace => self.with_prompt(Prompt::backspace),
            KeyCode::Delete => self.with_prompt(Prompt::delete),
            KeyCode::Char('u') if ctrl => self.with_prompt(Prompt::clear),
            KeyCode::Char('w') if ctrl => self.with_prompt(Prompt::delete_word),
            KeyCode::Char(c) if !ctrl => {
                if let Some(prompt) = &mut self.prompt {
                    prompt.insert_char(c);
                }
            }
            _ => {}
        }
    }

    fn with_prompt(&mut self, f: impl FnOnce(&mut Prompt)) {
        if let Some(prompt) = &mut self.prompt {
            f(prompt);
        }
    }

    /// Candidate list for the token at `idx`: command names for the first
    /// token, then sandbox names for `run` and instance names for the rest.
    fn candidates_for(
        idx: usize,
        first: &str,
        sandboxes: &[String],
        instances: &[String],
    ) -> Vec<String> {
        if idx == 0 {
            return COMMANDS.iter().map(|s| s.to_string()).collect();
        }
        if idx != 1 {
            return Vec::new();
        }
        match first {
            "run" => sandboxes.to_vec(),
            "exec" | "code" | "rm" => instances.to_vec(),
            _ => Vec::new(),
        }
    }

    /// Instance names from the latest snapshot (empty until one lands).
    fn instance_names(&self) -> Vec<String> {
        self.snapshot
            .as_ref()
            .map(|s| s.instances.iter().map(|r| r.name.clone()).collect())
            .unwrap_or_default()
    }

    /// Sandbox names from the on-disk config; best-effort (empty on error).
    fn sandbox_names(&self) -> Vec<String> {
        match Config::load(&self.dir) {
            Ok(cfg) => cfg.sandboxes.keys().cloned().collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Take the pending action for the event loop to execute, if any.
    pub fn take_pending_action(&mut self) -> Option<PromptAction> {
        self.pending_action.take()
    }

    /// Key handling while the config modal is open. Assumes `self.modal` is
    /// `Config`; scroll clamps to content length.
    fn on_key_modal(&mut self, key: KeyEvent) {
        let Modal::Config(view) = &mut self.modal else {
            return;
        };
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.modal = Modal::None;
            }
            KeyCode::Tab => {
                view.showing = match view.showing {
                    Side::Original => Side::Resolved,
                    Side::Resolved => Side::Original,
                };
                view.clamp_scroll();
            }
            KeyCode::Up | KeyCode::Char('k') => {
                view.scroll = view.scroll.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                view.scroll = view.scroll.saturating_add(1);
                view.clamp_scroll();
            }
            KeyCode::PageUp => {
                view.scroll = view.scroll.saturating_sub(20);
            }
            KeyCode::PageDown => {
                view.scroll = view.scroll.saturating_add(20);
                view.clamp_scroll();
            }
            KeyCode::Char('g') => view.scroll = 0,
            KeyCode::Char('G') => {
                view.scroll = view.line_count().saturating_sub(1);
            }
            _ => {}
        }
    }

    /// Open the config explorer for the current selection. On the Instances tab
    /// the target is the sandbox behind the selected instance; on the Services
    /// tab it is the selected service (original == resolved). No-op when there is
    /// no row to key off.
    fn open_config(&mut self) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let sel = self.selected[self.tab.index()];
        let view = match self.tab {
            Tab::Instances => {
                let Some(row) = snapshot.instances.get(sel) else {
                    return;
                };
                Self::build_sandbox_view(&self.dir, &row.sandbox)
            }
            Tab::Services => {
                let Some(row) = snapshot.services.get(sel) else {
                    return;
                };
                Self::build_service_view(&self.dir, &row.name)
            }
        };
        self.modal = Modal::Config(view);
    }

    /// Build a sandbox [`ConfigView`]: original = raw table, resolved = merged
    /// table. On any config/serialization error, both bodies carry the message.
    fn build_sandbox_view(dir: &PathBuf, name: &str) -> ConfigView {
        let mut view = ConfigView {
            title: name.to_string(),
            original: String::new(),
            resolved: String::new(),
            showing: Side::Original,
            scroll: 0,
            hash: String::new(),
        };
        match Config::load(dir) {
            Ok(cfg) => {
                view.original = match cfg.sandbox_table(name) {
                    Ok(t) => to_toml(t),
                    Err(e) => format!("{e:#}"),
                };
                match cfg.resolved_table(name) {
                    Ok((t, hash)) => {
                        view.resolved = to_toml(&t);
                        view.hash = hash;
                    }
                    Err(e) => view.resolved = format!("{e:#}"),
                }
            }
            Err(e) => {
                let msg = format!("{e:#}");
                view.original = msg.clone();
                view.resolved = msg;
            }
        }
        view
    }

    /// Build a service [`ConfigView`]: the raw `[services.<name>]` table.
    /// Original and resolved are identical (services do not `extends`).
    fn build_service_view(dir: &PathBuf, name: &str) -> ConfigView {
        let body = match Config::load(dir) {
            Ok(cfg) => match cfg.services.get(name) {
                Some(t) => to_toml(t),
                None => format!("unknown service `{name}`"),
            },
            Err(e) => format!("{e:#}"),
        };
        ConfigView {
            title: name.to_string(),
            original: body.clone(),
            resolved: body,
            showing: Side::Original,
            scroll: 0,
            hash: String::new(),
        }
    }
}

/// Serialize a table for display, preferring the pretty formatter and falling
/// back to its error text so a malformed table still renders something.
fn to_toml(table: &toml::Table) -> String {
    match toml::to_string_pretty(table) {
        Ok(s) => s,
        Err(e) => format!("cannot serialize config: {e}"),
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

    fn snapshot_with(n: usize) -> Snapshot {
        use super::super::data::{ContainerStatus, InstanceRow};
        let instances = (0..n)
            .map(|i| InstanceRow {
                name: format!("inst{i}"),
                sandbox: "s".into(),
                container: format!("devsandbox-inst{i}"),
                status: ContainerStatus::Missing,
                uptime_secs: 0,
                cpu: None,
                mem: None,
                folder: "/f".into(),
                worktree: false,
                services: Vec::new(),
                workspace: "/w".into(),
                remote_user: None,
                remote_env_len: 0,
                base_folder: "/f".into(),
                drift: false,
            })
            .collect();
        Snapshot {
            instances,
            services: Vec::new(),
            collected_at: std::time::Instant::now(),
            error: None,
        }
    }

    #[test]
    fn selection_clamps_when_snapshot_shrinks() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with(3));
        // Move down to the last row.
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.selected(), 2);

        // Shrinking the snapshot re-clamps selection to the new last row.
        app.set_snapshot(snapshot_with(1));
        assert_eq!(app.selected(), 0);

        // Empty snapshot clamps to 0.
        app.set_snapshot(snapshot_with(0));
        assert_eq!(app.selected(), 0);
    }

    /// Install a small config modal directly, bypassing fs/config reads.
    fn open_modal(app: &mut App) {
        app.modal = Modal::Config(ConfigView {
            title: "s".into(),
            original: "[sandbox.s]\nimage = \"a\"\n".into(),
            // Ten lines so scroll has room to move.
            resolved: (0..10).map(|i| format!("line{i}\n")).collect(),
            showing: Side::Original,
            scroll: 0,
            hash: "deadbeef".into(),
        });
    }

    fn view(app: &App) -> &ConfigView {
        match &app.modal {
            Modal::Config(v) => v,
            Modal::None => panic!("modal not open"),
        }
    }

    #[test]
    fn modal_toggles_side_and_closes() {
        let mut app = new_app();
        open_modal(&mut app);
        assert_eq!(view(&app).showing, Side::Original);

        app.on_key(key(KeyCode::Tab));
        assert_eq!(view(&app).showing, Side::Resolved);
        app.on_key(key(KeyCode::Tab));
        assert_eq!(view(&app).showing, Side::Original);

        // esc closes; so does q.
        app.on_key(key(KeyCode::Esc));
        assert!(matches!(app.modal, Modal::None));

        open_modal(&mut app);
        app.on_key(key(KeyCode::Char('q')));
        assert!(matches!(app.modal, Modal::None));
    }

    #[test]
    fn modal_scroll_clamps() {
        let mut app = new_app();
        open_modal(&mut app);
        // Switch to the 10-line resolved body.
        app.on_key(key(KeyCode::Tab));

        // Up at the top stays at 0.
        app.on_key(key(KeyCode::Up));
        assert_eq!(view(&app).scroll, 0);

        app.on_key(key(KeyCode::Down));
        assert_eq!(view(&app).scroll, 1);

        // G jumps to the last line (10 lines → max index 9).
        app.on_key(key(KeyCode::Char('G')));
        assert_eq!(view(&app).scroll, 9);

        // PageDown past the end clamps, not overflows.
        app.on_key(key(KeyCode::PageDown));
        assert_eq!(view(&app).scroll, 9);

        app.on_key(key(KeyCode::Char('g')));
        assert_eq!(view(&app).scroll, 0);
    }

    #[test]
    fn modal_swallows_dashboard_keys() {
        let mut app = new_app();
        open_modal(&mut app);
        // '2' would switch tabs on the dashboard; the modal must swallow it.
        app.on_key(key(KeyCode::Char('2')));
        assert_eq!(app.tab, Tab::Instances);
        assert!(matches!(app.modal, Modal::Config(_)));
    }

    #[test]
    fn scroll_reclamps_on_side_toggle() {
        let mut app = new_app();
        open_modal(&mut app);
        // Scroll to bottom of the 10-line resolved side.
        app.on_key(key(KeyCode::Tab));
        app.on_key(key(KeyCode::Char('G')));
        assert_eq!(view(&app).scroll, 9);

        // Toggling back to the 2-line original re-clamps the offset.
        app.on_key(key(KeyCode::Tab));
        assert_eq!(view(&app).showing, Side::Original);
        assert_eq!(view(&app).scroll, 1);
    }
}
