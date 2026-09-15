//! Dashboard state machine. Deliberately free of terminal I/O so key handling
//! and selection logic stay unit-testable; `mod.rs` owns the crossterm/ratatui
//! side and feeds decoded key events in here.

use std::collections::BTreeSet;
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config::Config;
use crate::docker;

use super::data::{visible_nodes, Node, Snapshot, ORPHANS_NAME};
use super::prompt::{Prompt, PromptAction, COMMANDS};

/// Keybinding reference shown by the `?` overlay, grouped by context.
const HELP_BODY: &str = "\
Global
  q, ctrl-c   quit
  tab / S-tab switch tab      1/2  jump to tab
  :           command prompt  ?    this help

Tables (Instances / Services)
  ↑/k ↓/j     move selection
  →/space     expand   ←  collapse / jump to parent
  enter, e    open config explorer
  l           logs (Instances tab)

Config modal
  tab         toggle original / resolved
  ↑/k ↓/j     scroll    pgup/pgdn  page
  g / G       top / bottom
  esc, q      close

Logs modal
  ↑/k ↓/j     scroll    pgup/pgdn  page
  g / G       top / bottom
  esc, q      close

Command prompt (:)
  run <sandbox> [--name n]    exec <instance> <cmd…>
  code <instance>             rm <instance>
  tab         complete / cycle
  ↑ ↓         history
  ctrl-u      clear line       ctrl-w  delete word
  esc         cancel";

/// A scrollable full-screen text overlay (help, logs). Body is captured once at
/// open time; scrolling is the only interaction. Shared scroll math lives in
/// [`clamp_scroll`]/[`line_count`] so the config modal and this stay in sync.
pub struct TextModal {
    /// Rendered in the modal border title.
    pub title: String,
    /// Full body text; rendered as-is (no per-line highlighting).
    pub body: String,
    pub scroll: u16,
}

impl TextModal {
    pub fn new(title: String, body: String) -> Self {
        Self { title, body, scroll: 0 }
    }

    /// Apply a scroll key. Returns `false` for keys that don't scroll (so the
    /// caller can treat esc/q/? as close).
    fn on_scroll_key(&mut self, key: KeyEvent) -> bool {
        scroll_key(&mut self.scroll, line_count(&self.body), key)
    }
}

/// Line count of `body`, clamped to `u16`, for scroll bounds.
fn line_count(body: &str) -> u16 {
    body.lines().count().min(u16::MAX as usize) as u16
}

/// Apply a scroll key to `scroll`, clamped to `[0, lines-1]`. Returns `true`
/// when the key was a scroll key (consumed), `false` otherwise. Shared by the
/// config modal and [`TextModal`].
fn scroll_key(scroll: &mut u16, lines: u16, key: KeyEvent) -> bool {
    let max = lines.saturating_sub(1);
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => *scroll = (*scroll).saturating_add(1).min(max),
        KeyCode::PageUp => *scroll = scroll.saturating_sub(20),
        KeyCode::PageDown => *scroll = (*scroll).saturating_add(20).min(max),
        KeyCode::Char('g') => *scroll = 0,
        KeyCode::Char('G') => *scroll = max,
        _ => return false,
    }
    true
}

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
        line_count(self.body())
    }

    /// Clamp `scroll` into `[0, line_count)` (0 when empty).
    fn clamp_scroll(&mut self) {
        let max = self.line_count().saturating_sub(1);
        if self.scroll > max {
            self.scroll = max;
        }
    }
}

/// Overlay state. `None` is the normal dashboard; the rest are full-screen
/// modals that swallow every key until dismissed.
pub enum Modal {
    None,
    Config(ConfigView),
    /// Keybinding reference (`?`).
    Help(TextModal),
    /// Container log tail (`l` on the Instances tab).
    Logs(TextModal),
}

/// The two top-level views.
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
    /// Collapsed tree groups on the Instances tab, keyed by sandbox name (and
    /// [`ORPHANS_NAME`] for the orphan group). Empty means all expanded; survives
    /// snapshot refreshes.
    collapsed: BTreeSet<String>,
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
            collapsed: BTreeSet::new(),
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
            Tab::Instances => self.visible_nodes().len(),
            Tab::Services => self.snapshot.as_ref().map_or(0, |s| s.services.len()),
        }
    }

    /// Flattened Instances-tree nodes for the current snapshot + collapse state.
    /// Empty until a snapshot lands. Cheap; recomputed on demand.
    pub fn visible_nodes(&self) -> Vec<Node> {
        match &self.snapshot {
            Some(s) => visible_nodes(&s.sandboxes, &s.instances, &self.collapsed),
            None => Vec::new(),
        }
    }

    /// Whether a tree group (sandbox name / [`ORPHANS_NAME`]) is collapsed. Used
    /// by the renderer to pick the ▸/▾ marker.
    pub fn is_collapsed_group(&self, key: &str) -> bool {
        self.collapsed.contains(key)
    }

    /// The tree node under the Instances-tab cursor, if any.
    fn selected_node(&self) -> Option<Node> {
        self.visible_nodes()
            .get(self.selected[Tab::Instances.index()])
            .copied()
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

    /// The collapse-set key for a collapsible node (sandbox name / orphan group),
    /// or `None` for leaf nodes (instances, empty markers).
    fn collapse_key(&self, node: Node) -> Option<String> {
        let snapshot = self.snapshot.as_ref()?;
        match node {
            Node::Sandbox(i) => snapshot.sandboxes.get(i).map(|s| s.name.clone()),
            Node::Orphans => Some(ORPHANS_NAME.to_string()),
            Node::Instance(_) | Node::Empty(_) => None,
        }
    }

    /// Expand the node under the cursor (sandbox / orphan group). No-op on leaves.
    fn tree_expand(&mut self) {
        if let Some(node) = self.selected_node() {
            if let Some(key) = self.collapse_key(node) {
                self.collapsed.remove(&key);
                self.clamp_selection();
            }
        }
    }

    /// Toggle the collapsible node under the cursor (space). No-op on leaves.
    fn tree_toggle(&mut self) {
        if let Some(node) = self.selected_node() {
            if let Some(key) = self.collapse_key(node) {
                if !self.collapsed.remove(&key) {
                    self.collapsed.insert(key);
                }
                self.clamp_selection();
            }
        }
    }

    /// `←`: on a collapsible node collapse it; on an instance/empty child jump
    /// selection to the parent sandbox (or orphan group) node.
    fn tree_collapse(&mut self) {
        let Some(node) = self.selected_node() else {
            return;
        };
        match node {
            Node::Sandbox(_) | Node::Orphans => {
                if let Some(key) = self.collapse_key(node) {
                    self.collapsed.insert(key);
                    self.clamp_selection();
                }
            }
            Node::Instance(_) | Node::Empty(_) => {
                if let Some(parent) = self.parent_index(self.selected[Tab::Instances.index()]) {
                    self.selected[Tab::Instances.index()] = parent;
                }
            }
        }
    }

    /// Index of the enclosing group node (sandbox / orphan header) for the child
    /// at visible-node position `pos`: the nearest preceding `Sandbox`/`Orphans`.
    fn parent_index(&self, pos: usize) -> Option<usize> {
        let nodes = self.visible_nodes();
        nodes[..pos.min(nodes.len())]
            .iter()
            .rposition(|n| matches!(n, Node::Sandbox(_) | Node::Orphans))
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
        if !matches!(self.modal, Modal::None) {
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
            KeyCode::Right if self.tab == Tab::Instances => self.tree_expand(),
            KeyCode::Char(' ') if self.tab == Tab::Instances => self.tree_toggle(),
            KeyCode::Left if self.tab == Tab::Instances => self.tree_collapse(),
            KeyCode::Enter | KeyCode::Char('e') => self.open_config(),
            KeyCode::Char('l') => self.open_logs(),
            KeyCode::Char('?') => self.open_help(),
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

    /// Key handling while any modal is open. `esc`/`q` (and `?` for the help
    /// overlay) close; the config modal additionally toggles sides on `tab`;
    /// scroll keys are shared across all three. Assumes `self.modal` is not
    /// `None`.
    fn on_key_modal(&mut self, key: KeyEvent) {
        match &mut self.modal {
            Modal::None => {}
            Modal::Config(view) => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => self.modal = Modal::None,
                KeyCode::Tab => {
                    view.showing = match view.showing {
                        Side::Original => Side::Resolved,
                        Side::Resolved => Side::Original,
                    };
                    view.clamp_scroll();
                }
                _ => {
                    let lines = view.line_count();
                    scroll_key(&mut view.scroll, lines, key);
                }
            },
            Modal::Help(view) => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?')) {
                    self.modal = Modal::None;
                } else {
                    view.on_scroll_key(key);
                }
            }
            Modal::Logs(view) => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
                    self.modal = Modal::None;
                } else {
                    view.on_scroll_key(key);
                }
            }
        }
    }

    /// Open the help overlay listing all keybindings grouped by context.
    fn open_help(&mut self) {
        self.modal = Modal::Help(TextModal::new(" help — keys ".to_string(), HELP_BODY.to_string()));
    }

    /// Open a full-screen log tail for the selected instance's container.
    /// Fetches `docker logs --tail 50` (stdout+stderr merged) at open time; a
    /// missing container or docker error renders as the body. Instances tab only;
    /// no-op with no selectable row.
    fn open_logs(&mut self) {
        if self.tab != Tab::Instances {
            return;
        }
        // Logs are instance-only; a no-op on sandbox / empty / orphan-group nodes.
        let Some(Node::Instance(i)) = self.selected_node() else {
            return;
        };
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let Some(row) = snapshot.instances.get(i) else {
            return;
        };
        let container = row.container.clone();
        let body = match docker::output_merged(&["logs", "--tail", "50", &container]) {
            Ok(out) if out.trim().is_empty() => "(no log output)".to_string(),
            Ok(out) => out,
            Err(e) => format!("{e:#}"),
        };
        let title = format!(" logs — {container} (last 50) ");
        self.modal = Modal::Logs(TextModal::new(title, body));
    }

    /// Open the config explorer for the current selection. On the Instances tab
    /// the target is the sandbox under the cursor (a sandbox node, or the sandbox
    /// behind an instance / empty node); the orphan group and its children have no
    /// config. On the Services tab it is the selected service (original ==
    /// resolved). No-op when there is no row to key off.
    fn open_config(&mut self) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let view = match self.tab {
            Tab::Instances => {
                let name = match self.selected_node() {
                    Some(Node::Sandbox(i)) | Some(Node::Empty(i)) => {
                        snapshot.sandboxes.get(i).map(|s| s.name.clone())
                    }
                    Some(Node::Instance(i)) => {
                        snapshot.instances.get(i).map(|r| r.sandbox.clone())
                    }
                    Some(Node::Orphans) | None => None,
                };
                let Some(name) = name else {
                    return;
                };
                Self::build_sandbox_view(&self.dir, &name)
            }
            Tab::Services => {
                let Some(row) = snapshot.services.get(self.selected()) else {
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

    /// A snapshot with one sandbox `s` holding `n` instances. Its Instances tree
    /// is `[Sandbox(0), Instance(0)..Instance(n-1)]` when expanded, i.e. node
    /// position of instance `i` is `i + 1`.
    fn snapshot_with(n: usize) -> Snapshot {
        use super::super::data::{ContainerStatus, InstanceRow, SandboxRow};
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
        let sandboxes = vec![SandboxRow {
            name: "s".into(),
            source: "image x".into(),
            folder: None,
            services: Vec::new(),
            extends: Vec::new(),
            config_hash: "hash".into(),
        }];
        Snapshot {
            instances,
            sandboxes,
            services: Vec::new(),
            sandbox_count: 1,
            docker_version: None,
            collected_at: std::time::Instant::now(),
            error: None,
        }
    }

    #[test]
    fn selection_clamps_when_snapshot_shrinks() {
        let mut app = new_app();
        // Tree: [Sandbox(0), inst0, inst1, inst2] → 4 nodes.
        app.set_snapshot(snapshot_with(3));
        // Move down to the last row.
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.selected(), 3);

        // Shrinking to one instance → [Sandbox(0), inst0] re-clamps to last row.
        app.set_snapshot(snapshot_with(1));
        assert_eq!(app.selected(), 1);

        // Empty sandbox → [Sandbox(0), Empty(0)] still has 2 rows; clamp to 1.
        app.set_snapshot(snapshot_with(0));
        assert_eq!(app.selected(), 1);
    }

    #[test]
    fn left_on_instance_jumps_to_parent_sandbox() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with(2)); // [Sandbox(0), inst0, inst1]
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Down)); // on inst1
        assert_eq!(app.selected(), 2);
        assert_eq!(app.selected_node(), Some(Node::Instance(1)));

        app.on_key(key(KeyCode::Left)); // jumps to the sandbox node
        assert_eq!(app.selected(), 0);
        assert_eq!(app.selected_node(), Some(Node::Sandbox(0)));
    }

    #[test]
    fn collapse_hides_children_and_reclamps_selection() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with(2)); // [Sandbox(0), inst0, inst1]
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Down)); // on inst1 (pos 2)
        assert_eq!(app.selected(), 2);

        // Collapse the sandbox from the child: jumps to the parent first.
        app.on_key(key(KeyCode::Left));
        assert_eq!(app.selected(), 0);
        // Now on the sandbox node: Left collapses it, hiding both instances.
        app.on_key(key(KeyCode::Left));
        assert_eq!(app.visible_nodes(), vec![Node::Sandbox(0)]);
        assert_eq!(app.selected(), 0);

        // Space toggles it back open.
        app.on_key(key(KeyCode::Char(' ')));
        assert_eq!(
            app.visible_nodes(),
            vec![Node::Sandbox(0), Node::Instance(0), Node::Instance(1)],
        );
    }

    #[test]
    fn collapse_survives_snapshot_refresh_and_clamps() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with(3)); // 4 nodes
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Down)); // last instance, pos 3
        assert_eq!(app.selected(), 3);
        // Collapse via parent jump then Left.
        app.on_key(key(KeyCode::Left)); // to sandbox, pos 0
        app.on_key(key(KeyCode::Left)); // collapse
        assert_eq!(app.visible_nodes().len(), 1);

        // A refresh keeps the collapse and re-clamps (still 1 node).
        app.set_snapshot(snapshot_with(3));
        assert_eq!(app.visible_nodes().len(), 1);
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
            _ => panic!("config modal not open"),
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
    fn help_overlay_opens_and_closes() {
        let mut app = new_app();
        app.on_key(key(KeyCode::Char('?')));
        assert!(matches!(app.modal, Modal::Help(_)));

        // The help modal swallows dashboard keys.
        app.on_key(key(KeyCode::Char('2')));
        assert_eq!(app.tab, Tab::Instances);
        assert!(matches!(app.modal, Modal::Help(_)));

        // `?` toggles it closed; so would esc/q.
        app.on_key(key(KeyCode::Char('?')));
        assert!(matches!(app.modal, Modal::None));

        app.on_key(key(KeyCode::Char('?')));
        app.on_key(key(KeyCode::Esc));
        assert!(matches!(app.modal, Modal::None));

        app.on_key(key(KeyCode::Char('?')));
        app.on_key(key(KeyCode::Char('q')));
        assert!(matches!(app.modal, Modal::None));
    }

    #[test]
    fn text_modal_scroll_shares_config_logic() {
        // A 10-line body scrolls and clamps identically to the config modal.
        let mut m = TextModal::new("t".into(), (0..10).map(|i| format!("l{i}\n")).collect());
        assert_eq!(m.scroll, 0);
        assert!(m.on_scroll_key(key(KeyCode::Up))); // stays at 0
        assert_eq!(m.scroll, 0);
        assert!(m.on_scroll_key(key(KeyCode::Down)));
        assert_eq!(m.scroll, 1);
        assert!(m.on_scroll_key(key(KeyCode::Char('G'))));
        assert_eq!(m.scroll, 9);
        assert!(m.on_scroll_key(key(KeyCode::PageDown))); // clamps, no overflow
        assert_eq!(m.scroll, 9);
        assert!(m.on_scroll_key(key(KeyCode::Char('g'))));
        assert_eq!(m.scroll, 0);
        // A non-scroll key is not consumed.
        assert!(!m.on_scroll_key(key(KeyCode::Char('x'))));
    }

    #[test]
    fn logs_modal_opens_on_selected_instance() {
        // Instances tab with a row and no docker: open_logs still opens the modal
        // with the docker error as the body (never panics).
        let mut app = new_app();
        app.set_snapshot(snapshot_with(1));
        // Cursor starts on the sandbox node; move down to the instance.
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Char('l')));
        match &app.modal {
            Modal::Logs(v) => {
                assert!(v.title.contains("devsandbox-inst0"));
                assert!(!v.body.is_empty());
            }
            _ => panic!("logs modal not open"),
        }
        app.on_key(key(KeyCode::Char('q')));
        assert!(matches!(app.modal, Modal::None));
    }

    #[test]
    fn logs_key_ignored_on_services_tab() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with(1));
        app.tab = Tab::Services;
        app.on_key(key(KeyCode::Char('l')));
        assert!(matches!(app.modal, Modal::None));
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
