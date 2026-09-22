//! Dashboard state machine. Deliberately free of terminal I/O so key handling
//! and selection logic stay unit-testable; `mod.rs` owns the crossterm/ratatui
//! side and feeds decoded key events in here.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;

use crate::commands::run::DEFAULT_WORKTREE_BRANCH;
use crate::config::Config;
use crate::runtime::backend;

use super::data::{visible_nodes, ContainerStatus, InstanceRow, Node, Snapshot, ORPHANS_NAME};
use super::procs::ProcState;
use super::prompt::{Prompt, PromptAction, COMMANDS};
use super::term::{encode_key, TermSession, TermTabs, SHELL_FALLBACK_CMD};
use crate::runtime::NAME_PREFIX;

/// Keybinding reference shown by the `?` overlay, grouped by context.
const HELP_BODY: &str = "\
Global
  q, ctrl-c   quit
  tab / S-tab switch tab      1/2  jump to tab
  :           command prompt  ?    this help

Tables (Instances / Services)
  ↑/k ↓/j     move selection
  →/space     expand (sandbox instances, instance processes)
  ←           collapse / jump to parent
  enter, e    open config explorer
  r           run (sandbox) /   o   open in VS Code (Instances)
              rename (instance)
  s           stop / start     l   logs (Instances tab)
              (stops running / starts exited; s-start is a bare start —
               :start runs services + postStartCommand too; a drifted
               exited instance is rebuilt instead)
  (process rows act on their parent instance)

Terminals
  t           open terminal (instance / service)
  T           force a new terminal for the same target
  [ / ]       previous / next terminal tab
  x           close the active terminal
  ctrl-] / F12  focus terminal ⇄ back to dashboard
  (while focused, all other keys go to the shell)

Config modal
  t           toggle original / resolved
  tab         switch pane (config ⇄ inspect)
  < / >       resize the divider (drag it too)
  ↑/k ↓/j     scroll focused pane   pgup/pgdn  page
  g / G       top / bottom
  esc, q      close

Logs modal
  ↑/k ↓/j     scroll    pgup/pgdn  page
  g / G       top / bottom
  esc, q      close

Command prompt (:)
  run <sandbox> [--name n] [--branch b]   exec <instance> <cmd…>
  code <instance>   rm <instance>   rename <instance> <new-name>
  stop <instance>   start <instance>
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

/// Which side of a [`ConfigView`]'s config pane is currently shown.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Side {
    Original,
    Resolved,
}

/// Which pane of the split config modal has focus (receives scroll keys).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pane {
    Config,
    Inspect,
}

/// Lower/upper bounds for the split percentage, inclusive.
const SPLIT_MIN: u16 = 20;
const SPLIT_MAX: u16 = 80;

/// Full-screen config explorer state. Built once at open time (fs + config +
/// `docker inspect` are read then, not during rendering); on error the bodies
/// carry the error text so the modal still renders.
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
    /// Pretty-printed `docker inspect` (or explanatory text) for the target
    /// container, fetched at open time.
    pub inspect: String,
    pub inspect_scroll: u16,
    /// Container the inspect pane is showing, for the pane title. Empty when
    /// there is no target container (the body is then explanatory text).
    pub inspect_container: String,
    /// Which pane has focus for scroll keys.
    pub focus: Pane,
    /// Left (config) pane width as a percentage, clamped to [SPLIT_MIN, SPLIT_MAX].
    pub split_pct: u16,
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

    /// Route a scroll key to the focused pane, clamped to that pane's line count.
    fn scroll_focused(&mut self, key: KeyEvent) {
        match self.focus {
            Pane::Config => {
                let lines = self.line_count();
                scroll_key(&mut self.scroll, lines, key);
            }
            Pane::Inspect => {
                let lines = line_count(&self.inspect);
                scroll_key(&mut self.inspect_scroll, lines, key);
            }
        }
    }

    /// Move the divider by `delta` percent, clamped to [SPLIT_MIN, SPLIT_MAX].
    fn resize(&mut self, delta: i16) {
        self.split_pct = clamp_split(self.split_pct as i16 + delta);
    }
}

/// Clamp a split percentage into the allowed range.
fn clamp_split(pct: i16) -> u16 {
    pct.clamp(SPLIT_MIN as i16, SPLIT_MAX as i16) as u16
}

/// Divider percentage for a mouse at column `col` over a modal `width` cells
/// wide, clamped to [SPLIT_MIN, SPLIT_MAX]. Pure so the drag math is testable.
pub fn divider_pct(col: u16, width: u16) -> u16 {
    if width == 0 {
        return SPLIT_MIN;
    }
    let pct = (col as u32 * 100 / width as u32) as i16;
    clamp_split(pct)
}

/// Whether `col` is on or within one cell of the divider column.
fn col_near(col: u16, divider: u16) -> bool {
    col.abs_diff(divider) <= 1
}

/// Whether `(col, row)` falls inside `rect` (border included). Pure so the
/// terminal-panel hit-testing stays unit-testable.
fn point_in(rect: Rect, col: u16, row: u16) -> bool {
    col >= rect.x
        && col < rect.x + rect.width
        && row >= rect.y
        && row < rect.y + rect.height
}

/// Add `delta` (may be negative) to `scroll`, clamped to `[0, max]`.
fn apply_delta(scroll: u16, delta: i16, max: u16) -> u16 {
    let next = scroll as i32 + delta as i32;
    next.clamp(0, max as i32) as u16
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

/// Where keystrokes go. `Dashboard` is the classic table/tree navigation;
/// `Terminal` forwards nearly every key to the active integrated terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Dashboard,
    Terminal,
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
    /// Instances (by name) whose process layer is expanded on the Instances tab.
    /// Default empty (all collapsed); survives snapshot refreshes.
    expanded_procs: BTreeSet<String>,
    /// Cached process state per instance name. Kept across collapse (stale is
    /// fine); a fetch overwrites only the keys it fetched. Absent = not yet
    /// fetched, which renders as a placeholder row.
    pub procs: BTreeMap<String, ProcState>,
    /// Set when an instance's procs are freshly expanded so the event loop
    /// fetches immediately instead of waiting for the next proc tick; the loop
    /// clears it.
    pub needs_proc_fetch: bool,
    /// Latest data collected off-thread; `None` until the first snapshot lands.
    pub snapshot: Option<Snapshot>,
    /// Active overlay, if any.
    pub modal: Modal,
    /// Command prompt, when open (`:`). Occupies the bottom bar and swallows keys.
    pub prompt: Option<Prompt>,
    /// Action awaiting execution by the event loop (it owns the terminal
    /// suspend + command call, keeping [`App`] I/O-free).
    pub pending_action: Option<PromptAction>,
    /// Instance name whose stop the event loop should spawn on a background
    /// thread (the `s` shortcut on a running instance; runs without suspending
    /// the TUI).
    pub pending_stop: Option<String>,
    /// Instances with a stop in flight, so repeated `s` presses don't spawn a
    /// second stop for the same instance. Cleared by the loop on completion.
    pub stopping: BTreeSet<String>,
    /// Instance name whose start the event loop should spawn on a background
    /// thread (the `s` shortcut on an exited instance).
    pub pending_start: Option<String>,
    /// Instances with a start in flight (same dedup rule as `stopping`).
    pub starting: BTreeSet<String>,
    /// One-line status shown in the help-bar area (e.g. `code` launch outcome).
    pub status: Option<String>,
    /// True while the config-modal divider is being dragged with the mouse.
    dragging_divider: bool,
    /// Whether keys drive the dashboard or the active terminal.
    pub focus: Focus,
    /// Open integrated-terminal tabs, shared across both top-level tabs. Empty
    /// until the user opens one with `t`.
    pub terms: TermTabs,
    pub should_quit: bool,
}

impl App {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            tab: Tab::Instances,
            selected: [0, 0],
            collapsed: BTreeSet::new(),
            expanded_procs: BTreeSet::new(),
            procs: BTreeMap::new(),
            needs_proc_fetch: false,
            snapshot: None,
            modal: Modal::None,
            prompt: None,
            pending_action: None,
            pending_stop: None,
            stopping: BTreeSet::new(),
            pending_start: None,
            starting: BTreeSet::new(),
            status: None,
            dragging_divider: false,
            focus: Focus::Dashboard,
            terms: TermTabs::default(),
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
            Some(s) => visible_nodes(
                &s.sandboxes,
                &s.instances,
                &self.collapsed,
                &self.expanded_procs,
                &self.procs,
            ),
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
        // Moving the cursor may land on a new instance; fetch its procs now so
        // the Detail agent count appears without waiting for the next proc tick.
        self.needs_proc_fetch = true;
    }

    fn select_down(&mut self) {
        let rows = self.row_count();
        if rows == 0 {
            return;
        }
        let slot = self.tab.index();
        self.selected[slot] = (self.selected[slot] + 1).min(rows - 1);
        self.needs_proc_fetch = true;
    }

    /// The collapse-set key for a collapsible node (sandbox name / orphan group),
    /// or `None` for leaf nodes (instances, empty markers).
    fn collapse_key(&self, node: Node) -> Option<String> {
        let snapshot = self.snapshot.as_ref()?;
        match node {
            Node::Sandbox(i) => snapshot.sandboxes.get(i).map(|s| s.name.clone()),
            Node::Orphans => Some(ORPHANS_NAME.to_string()),
            Node::Instance(_) | Node::Empty(_) | Node::Proc { .. } => None,
        }
    }

    /// `→`: expand the node under the cursor. On a sandbox / orphan group this
    /// unfolds its children; on an instance it expands the process layer (and
    /// signals the event loop to fetch now). No-op on other leaves.
    fn tree_expand(&mut self) {
        match self.selected_node() {
            Some(Node::Instance(_)) => self.expand_procs(),
            Some(node) => {
                if let Some(key) = self.collapse_key(node) {
                    self.collapsed.remove(&key);
                    self.clamp_selection();
                }
            }
            None => {}
        }
    }

    /// `space`: toggle the node under the cursor. Collapsible groups fold/unfold;
    /// an instance toggles its process layer.
    fn tree_toggle(&mut self) {
        match self.selected_node() {
            Some(Node::Instance(_)) => {
                if let Some(name) = self.selected_instance_name() {
                    if !self.expanded_procs.remove(&name) {
                        self.expanded_procs.insert(name);
                        self.needs_proc_fetch = true;
                    }
                    self.clamp_selection();
                }
            }
            Some(node) => {
                if let Some(key) = self.collapse_key(node) {
                    if !self.collapsed.remove(&key) {
                        self.collapsed.insert(key);
                    }
                    self.clamp_selection();
                }
            }
            None => {}
        }
    }

    /// Mark the selected instance's process layer expanded and request a fetch.
    fn expand_procs(&mut self) {
        if let Some(name) = self.selected_instance_name() {
            if self.expanded_procs.insert(name) {
                self.needs_proc_fetch = true;
            }
            self.clamp_selection();
        }
    }

    /// `←`: fold one level. On a collapsible node collapse it. On a process row
    /// jump to its instance. On an instance with procs expanded collapse the
    /// procs (staying on the instance); otherwise jump to the parent sandbox.
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
            Node::Proc { instance, .. } => {
                if let Some(pos) = self.instance_node_position(instance) {
                    self.selected[Tab::Instances.index()] = pos;
                }
            }
            Node::Instance(_) => {
                // First press collapses an expanded process layer (staying put);
                // otherwise fall through to the parent-sandbox jump.
                if let Some(name) = self.selected_instance_name() {
                    if self.expanded_procs.remove(&name) {
                        self.clamp_selection();
                        return;
                    }
                }
                if let Some(parent) = self.parent_index(self.selected[Tab::Instances.index()]) {
                    self.selected[Tab::Instances.index()] = parent;
                }
            }
            Node::Empty(_) => {
                if let Some(parent) = self.parent_index(self.selected[Tab::Instances.index()]) {
                    self.selected[Tab::Instances.index()] = parent;
                }
            }
        }
    }

    /// Instance index under the cursor: the instance row itself or the parent of
    /// a selected process row. `None` on sandbox / empty / orphan-group nodes.
    fn selected_instance_index(&self) -> Option<usize> {
        match self.selected_node()? {
            Node::Instance(i) => Some(i),
            Node::Proc { instance, .. } => Some(instance),
            _ => None,
        }
    }

    /// Name of the instance under the cursor, whether the selected node is the
    /// instance row itself or one of its process rows. `None` otherwise.
    fn selected_instance_name(&self) -> Option<String> {
        let snapshot = self.snapshot.as_ref()?;
        let idx = self.selected_instance_index()?;
        snapshot.instances.get(idx).map(|r| r.name.clone())
    }

    /// Visible-node position of `Node::Instance(instance)`, for jumping a process
    /// row's selection back onto its instance row.
    fn instance_node_position(&self, instance: usize) -> Option<usize> {
        self.visible_nodes()
            .iter()
            .position(|n| matches!(n, Node::Instance(i) if *i == instance))
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
        // A focused terminal is next in line, ahead of the dashboard bindings:
        // nearly every key belongs to the shell, not the tables.
        if self.focus == Focus::Terminal {
            self.on_key_terminal(key);
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
            // Enter a terminal: ctrl-] or F12. No-op with a status hint when none
            // are open.
            KeyCode::Char(']') if ctrl => self.enter_terminal(),
            KeyCode::F(12) => self.enter_terminal(),
            KeyCode::Char('t') => self.open_terminal(false),
            KeyCode::Char('T') => self.open_terminal(true),
            KeyCode::Char(']') => self.terms.next(),
            KeyCode::Char('[') => self.terms.prev(),
            KeyCode::Char('x') => self.terms.close_active(),
            KeyCode::Up | KeyCode::Char('k') => self.select_up(),
            KeyCode::Down | KeyCode::Char('j') => self.select_down(),
            KeyCode::Right if self.tab == Tab::Instances => self.tree_expand(),
            KeyCode::Char(' ') if self.tab == Tab::Instances => self.tree_toggle(),
            KeyCode::Left if self.tab == Tab::Instances => self.tree_collapse(),
            KeyCode::Enter | KeyCode::Char('e') => self.open_config(),
            KeyCode::Char('r') if self.tab == Tab::Instances => self.open_rename_or_run_prompt(),
            KeyCode::Char('o') if self.tab == Tab::Instances => self.attach_code(),
            KeyCode::Char('s') if self.tab == Tab::Instances => self.stop_or_start_instance(),
            KeyCode::Char('l') => self.open_logs(),
            KeyCode::Char('?') => self.open_help(),
            _ => {}
        }
    }

    /// Route a key to the active terminal. Only reached with `focus ==
    /// Terminal`. `ctrl-]` / `F12` leave; on an exited session every other key
    /// is swallowed; otherwise the key is encoded (honoring the shell's
    /// application-cursor mode) and written to the PTY.
    fn on_key_terminal(&mut self, key: KeyEvent) {
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
        // above still work).
        if session.exited() {
            return;
        }
        // DECCKM: the parser tracks whether the shell wants SS3 cursor keys.
        let application_cursor = session
            .parser()
            .lock()
            .map(|p| p.screen().application_cursor())
            .unwrap_or(false);
        if let Some(bytes) = encode_key(key, application_cursor) {
            session.write_key_bytes(&bytes);
        }
    }

    /// Focus the active terminal, or leave a status hint when none are open.
    fn enter_terminal(&mut self) {
        if self.terms.is_empty() {
            self.status = Some("no terminal open — press t".into());
        } else {
            self.focus = Focus::Terminal;
        }
    }

    /// Apply a mouse event over the full frame `area`. The config modal (when
    /// open) owns the mouse: a press/drag on or near the divider column resizes
    /// the split; the scroll wheel scrolls the pane under the cursor. Otherwise,
    /// with terminals open, clicks and the wheel drive the terminal panel
    /// ([`Self::terminal_mouse`]). I/O-free.
    pub fn on_mouse(&mut self, ev: &MouseEvent, area: Rect) {
        let Modal::Config(view) = &mut self.modal else {
            self.dragging_divider = false;
            self.terminal_mouse(ev, area);
            return;
        };
        // Divider column: the boundary between the left (config) pane and the
        // right (inspect) pane, i.e. split_pct of the modal width.
        let divider = (area.width as u32 * view.split_pct as u32 / 100) as u16;
        match ev.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if col_near(ev.column, divider) {
                    self.dragging_divider = true;
                    view.split_pct = divider_pct(ev.column, area.width);
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if self.dragging_divider {
                    view.split_pct = divider_pct(ev.column, area.width);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.dragging_divider = false;
            }
            MouseEventKind::ScrollDown => {
                Self::wheel_scroll(view, ev.column, divider, 3);
            }
            MouseEventKind::ScrollUp => {
                Self::wheel_scroll(view, ev.column, divider, -3);
            }
            _ => {}
        }
    }

    /// Mouse routing for the integrated-terminal panel, active only when no
    /// config modal is open and at least one terminal exists. Left-click inside
    /// the panel focuses the terminal; a click on the title row's tab labels
    /// activates that tab; a left-click outside the panel while the terminal is
    /// focused returns focus to the dashboard. The wheel scrolls the active
    /// session's vt100 scrollback. All layout math is borrowed from `ui` so it
    /// tracks exactly what the draw path lays out. I/O-free.
    fn terminal_mouse(&mut self, ev: &MouseEvent, area: Rect) {
        if self.terms.is_empty() {
            return;
        }
        let prompt_open = self.prompt.is_some();
        let panel = super::ui::terminal_panel_rect(area, prompt_open);
        let inside = point_in(panel, ev.column, ev.row);
        match ev.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if inside {
                    let focused = self.focus == Focus::Terminal;
                    // Title row: hit-test the tab labels; a hit activates that tab.
                    if let Some(i) =
                        super::ui::terminal_tab_hit(self, panel, focused, ev.column, ev.row)
                    {
                        self.terms.set_active(i);
                    }
                    self.focus = Focus::Terminal;
                } else if self.focus == Focus::Terminal {
                    // Click-away unfocuses; dashboard click handling is out of scope.
                    self.focus = Focus::Dashboard;
                }
            }
            MouseEventKind::ScrollUp if inside => self.scroll_active_terminal(3),
            MouseEventKind::ScrollDown if inside => self.scroll_active_terminal(-3),
            _ => {}
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

    /// Scroll the pane under `col` (left of `divider` = config, else inspect) by
    /// `delta` lines, clamped to that pane's line count.
    fn wheel_scroll(view: &mut ConfigView, col: u16, divider: u16, delta: i16) {
        if col < divider {
            let max = view.line_count().saturating_sub(1);
            view.scroll = apply_delta(view.scroll, delta, max);
        } else {
            let max = line_count(&view.inspect).saturating_sub(1);
            view.inspect_scroll = apply_delta(view.inspect_scroll, delta, max);
        }
    }

    /// Open the command prompt, loading persisted history. Clears any status.
    fn open_prompt(&mut self) {
        self.status = None;
        self.prompt = Some(Prompt::new(super::prompt::load_history()));
    }

    /// `r` (Instances tab): open the prompt pre-filled `run <sandbox> ` for the
    /// sandbox under the cursor (a sandbox / empty node's sandbox, or the sandbox
    /// behind an instance). The orphan group, its children, and no selection fall
    /// back to a plain empty prompt (identical to `:`).
    fn open_run_prompt(&mut self) {
        self.status = None;
        let sandbox = self.snapshot.as_ref().and_then(|snapshot| {
            match self.selected_node() {
                Some(Node::Sandbox(i)) | Some(Node::Empty(i)) => {
                    snapshot.sandboxes.get(i).map(|s| s.name.clone())
                }
                Some(Node::Instance(i)) => snapshot.instances.get(i).map(|r| r.sandbox.clone()),
                Some(Node::Proc { instance, .. }) => {
                    snapshot.instances.get(instance).map(|r| r.sandbox.clone())
                }
                Some(Node::Orphans) | None => None,
            }
        });
        let history = super::prompt::load_history();
        self.prompt = Some(match sandbox {
            Some(name) => Prompt::with_input(history, format!("run {name} ")),
            None => Prompt::new(history),
        });
    }

    /// `r` (Instances tab): rename the instance under the cursor (or its process
    /// row's parent) via the command prompt pre-filled `rename <instance> `.
    /// Sandbox / empty / orphan-group selections have no instance, so they fall
    /// back to [`Self::open_run_prompt`] — `r` keeps its run behavior there.
    fn open_rename_or_run_prompt(&mut self) {
        let Some(name) = self.selected_instance_name() else {
            self.open_run_prompt();
            return;
        };
        self.status = None;
        let history = super::prompt::load_history();
        self.prompt = Some(Prompt::with_input(history, format!("rename {name} ")));
    }

    /// `o` (Instances tab): VS Code attach for the instance under the cursor,
    /// routed through the same [`PromptAction::Code`] path the `code` command
    /// uses. Works for orphan-group instance children and process rows (routing
    /// to the parent instance); a no-op on sandbox / empty / orphan-group nodes.
    fn attach_code(&mut self) {
        let Some(i) = self.selected_instance_index() else {
            return;
        };
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let Some(row) = snapshot.instances.get(i) else {
            return;
        };
        self.pending_action = Some(PromptAction::Code { instance: row.name.clone() });
    }

    /// `s` (Instances tab): stop the running instance under the cursor, or
    /// start it when its container is exited, on a background thread (the event
    /// loop owns the docker work, keeping [`App`] I/O-free). Works for
    /// orphan-group instance children and process rows (routing to the parent
    /// instance); a no-op on sandbox / empty / orphan-group nodes and while a
    /// stop/start for the same instance is in flight.
    ///
    /// Two cases can't be served by a bare `docker start` and are *rebuilt*
    /// instead, by queuing a [`PromptAction::Rebuild`] through the same suspend
    /// path the `:` prompt uses (the loop restores the terminal, runs `rebuild`
    /// with inherited stdio, then re-enters): an exited instance whose
    /// container drifted from the current config, and a missing container —
    /// `rebuild` is the worktree-preserving way to recreate it (`run` refuses
    /// the taken name, `rm` destroys the worktree). Rebuild is modal, so it
    /// needs no `starting` in-flight guard — the guard is only for the
    /// background stop/start ops.
    fn stop_or_start_instance(&mut self) {
        let Some(i) = self.selected_instance_index() else {
            return;
        };
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let Some(row) = snapshot.instances.get(i) else {
            return;
        };
        let name = row.name.clone();
        if self.stopping.contains(&name) || self.starting.contains(&name) {
            return;
        }
        match row.status {
            ContainerStatus::Running(_) => {
                self.status = Some(format!("stopping {name}…"));
                self.stopping.insert(name.clone());
                self.pending_stop = Some(name);
            }
            ContainerStatus::Exited(_) if row.drift => {
                // Config drifted: recreate the container (worktree preserved) via
                // the modal suspend path instead of a bare start. No `starting`
                // guard — that's for background ops; the suspend is modal.
                self.pending_action =
                    Some(PromptAction::Rebuild { instance: name, force: false });
            }
            ContainerStatus::Exited(_) => {
                self.status = Some(format!("starting {name}…"));
                self.starting.insert(name.clone());
                self.pending_start = Some(name);
            }
            ContainerStatus::Missing => {
                // No container to start: recreate it via the CLI rebuild on the
                // suspend path (its "config label gone → rebuild anyway" rule
                // covers exactly this).
                self.pending_action =
                    Some(PromptAction::Rebuild { instance: name, force: false });
            }
        }
    }

    /// Help-bar verb for the `r` key: `rename` when the cursor is on an
    /// instance (or one of its process rows), `run` otherwise — mirrors what
    /// [`Self::open_rename_or_run_prompt`] would do.
    pub fn run_rename_hint(&self) -> &'static str {
        if self.selected_instance_index().is_some() {
            "rename"
        } else {
            "run"
        }
    }

    /// Help-bar verb for the `s` key, matching what
    /// [`Self::stop_or_start_instance`] would actually do to the instance under
    /// the cursor: `start` when it is exited or missing (drifted/missing ones
    /// rebuild, which is still a start from the user's seat), `stop` otherwise.
    pub fn stop_start_hint(&self) -> &'static str {
        let startable = self
            .selected_instance_index()
            .and_then(|i| self.snapshot.as_ref()?.instances.get(i))
            .is_some_and(|row| {
                matches!(
                    row.status,
                    ContainerStatus::Exited(_) | ContainerStatus::Missing
                )
            });
        if startable { "start" } else { "stop" }
    }

    /// Take the pending background stop for the event loop to spawn, if any.
    pub fn take_pending_stop(&mut self) -> Option<String> {
        self.pending_stop.take()
    }

    /// Take the pending background start for the event loop to spawn, if any.
    pub fn take_pending_start(&mut self) -> Option<String> {
        self.pending_start.take()
    }

    /// Agent-process count for a cached instance: the number of forest rows whose
    /// args name a known coding agent (see [`super::procs::is_agent`]). `None`
    /// when the instance has no fetched forest yet (not running, error, or the
    /// fetch is still in flight), which the Detail panel renders as `…`/`-`.
    pub fn agent_count(&self, instance: &str) -> Option<usize> {
        match self.procs.get(instance) {
            Some(ProcState::Rows(rows)) => {
                Some(rows.iter().filter(|r| super::procs::is_agent(&r.args)).count())
            }
            _ => None,
        }
    }

    /// The selected instance as a `(name, container)` fetch target, but only when
    /// its container is running. Drives the on-demand proc fetch that backs the
    /// Detail agent count for instances that aren't expanded. `None` on non-
    /// instance nodes or a non-running selection.
    fn selected_running_instance(&self) -> Option<(String, String)> {
        let snapshot = self.snapshot.as_ref()?;
        let idx = self.selected_instance_index()?;
        let row = snapshot.instances.get(idx)?;
        matches!(row.status, ContainerStatus::Running(_))
            .then(|| (row.name.clone(), row.container.clone()))
    }

    /// Consume the "fetch now" signal set on expand.
    pub fn take_needs_proc_fetch(&mut self) -> bool {
        std::mem::take(&mut self.needs_proc_fetch)
    }

    /// The `docker top` fetch targets: `(instance name, container)` for every
    /// expanded instance whose container is running. Expanded instances that are
    /// not running (or absent from the snapshot) get a `(not running)` message
    /// row stored directly here — no fetch — and are omitted from the returned
    /// list. Instances no longer in the snapshot are dropped from the cache. The
    /// selected running instance is appended (deduped) so the Detail agent count
    /// has a fresh forest even when its process layer isn't expanded.
    pub fn proc_fetch_targets(&mut self) -> Vec<(String, String)> {
        let Some(snapshot) = &self.snapshot else {
            return Vec::new();
        };
        let mut targets = Vec::new();
        let mut not_running: Vec<String> = Vec::new();
        for name in &self.expanded_procs {
            match snapshot.instances.iter().find(|r| &r.name == name) {
                Some(row) if matches!(row.status, ContainerStatus::Running(_)) => {
                    targets.push((row.name.clone(), row.container.clone()));
                }
                _ => not_running.push(name.clone()),
            }
        }
        for name in not_running {
            self.procs
                .insert(name, ProcState::Message("(not running)".to_string()));
        }
        // The selected running instance is fetched too (for the Detail agent
        // count), even when its process layer isn't expanded. Deduped against the
        // expanded targets so it's never fetched twice in one batch.
        if let Some((name, container)) = self.selected_running_instance() {
            if !targets.iter().any(|(n, _)| n == &name) {
                targets.push((name, container));
            }
        }
        targets
    }

    /// Merge a completed proc fetch into the cache, overwriting only fetched
    /// keys (other instances' cached rows are left untouched).
    pub fn apply_proc_fetch(&mut self, fetched: BTreeMap<String, ProcState>) {
        self.procs.extend(fetched);
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
                    // `rebuild`/`recreate` parses tab-agnostically as an instance
                    // action; on the Services tab it targets the named service
                    // instead. Rewrite here where the tab is known, keeping the
                    // parser pure.
                    let action = match action {
                        // `force` is dropped: service rebuild always recreates.
                        PromptAction::Rebuild { instance, .. } if self.tab == Tab::Services => {
                            PromptAction::ServiceRebuild { name: instance }
                        }
                        other => other,
                    };
                    self.pending_action = Some(action);
                    self.prompt = None;
                }
            }
            KeyCode::Tab => {
                let instances = self.instance_names();
                let services = self.service_names();
                let tab = self.tab;
                // One config load per tab: sandbox names for the positional
                // argument plus `worktree-branch` lookups for `--branch` values.
                let config = Config::load(&self.dir).ok();
                let sandboxes: Vec<String> = config
                    .as_ref()
                    .map(|c| c.sandboxes.keys().cloned().collect())
                    .unwrap_or_default();
                if let Some(prompt) = &mut self.prompt {
                    prompt.complete(|idx, tokens| {
                        Self::candidates_for(
                            tab,
                            idx,
                            tokens,
                            &sandboxes,
                            &instances,
                            &services,
                            config.as_ref(),
                        )
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

    /// Candidate list for the token at `idx` of the whitespace-split `tokens`
    /// (the token being completed is absent when empty): command names for the
    /// first token, sandbox names / flags for `run`, instance names for the rest.
    /// `rebuild`/`recreate` complete against service names on the Services tab
    /// (where they rebuild a service) and instance names elsewhere.
    fn candidates_for(
        tab: Tab,
        idx: usize,
        tokens: &[String],
        sandboxes: &[String],
        instances: &[String],
        services: &[String],
        config: Option<&Config>,
    ) -> Vec<String> {
        if idx == 0 {
            return COMMANDS.iter().map(|s| s.to_string()).collect();
        }
        match tokens.first().map(String::as_str) {
            Some("run") => Self::run_candidates(idx, tokens, sandboxes, config),
            Some("rebuild" | "recreate") if idx == 1 && tab == Tab::Services => services.to_vec(),
            Some("exec" | "code" | "rm" | "rename" | "stop" | "start" | "rebuild" | "recreate")
                if idx == 1 =>
            {
                instances.to_vec()
            }
            _ => Vec::new(),
        }
    }

    /// Candidates for `run` arguments past the command. Positional: sandbox
    /// names while none is on the line. Otherwise the flags not already used.
    /// After `--branch`, the branch the run would use anyway (the sandbox's
    /// `worktree-branch`, else the default pattern) so the user edits a base
    /// instead of typing from scratch; `${…}` variables stay unsubstituted,
    /// exactly as `run` would receive them.
    fn run_candidates(
        idx: usize,
        tokens: &[String],
        sandboxes: &[String],
        config: Option<&Config>,
    ) -> Vec<String> {
        match tokens.get(idx - 1).map(String::as_str) {
            Some("--branch") => {
                let branch = Self::run_sandbox_token(tokens, idx)
                    .and_then(|s| config?.resolve_sandbox(s).ok()?.properties.worktree_branch)
                    .unwrap_or_else(|| DEFAULT_WORKTREE_BRANCH.to_string());
                return vec![branch];
            }
            Some("--name") => return Vec::new(), // free-form value
            _ => {}
        }
        let stem = tokens.get(idx).map(String::as_str).unwrap_or("");
        if !stem.starts_with('-') && Self::run_sandbox_token(tokens, idx).is_none() {
            return sandboxes.to_vec();
        }
        ["--name", "--branch"]
            .into_iter()
            .filter(|flag| !tokens.iter().enumerate().any(|(i, t)| i != idx && t == flag))
            .map(str::to_string)
            .collect()
    }

    /// The positional (sandbox) token of a `run` line, if any: the first token
    /// after the command that is neither a flag, a flag's value, nor the token
    /// currently being completed (at index `skip`).
    fn run_sandbox_token(tokens: &[String], skip: usize) -> Option<&str> {
        let mut i = 1;
        while i < tokens.len() {
            match tokens[i].as_str() {
                "--name" | "--branch" => i += 2,
                _ if i == skip => i += 1,
                other => return Some(other),
            }
        }
        None
    }

    /// Instance names from the latest snapshot (empty until one lands).
    fn instance_names(&self) -> Vec<String> {
        self.snapshot
            .as_ref()
            .map(|s| s.instances.iter().map(|r| r.name.clone()).collect())
            .unwrap_or_default()
    }

    fn service_names(&self) -> Vec<String> {
        self.snapshot
            .as_ref()
            .map(|s| s.services.iter().map(|r| r.name.clone()).collect())
            .unwrap_or_default()
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
                KeyCode::Char('t') => {
                    view.showing = match view.showing {
                        Side::Original => Side::Resolved,
                        Side::Resolved => Side::Original,
                    };
                    view.clamp_scroll();
                }
                KeyCode::Tab => {
                    view.focus = match view.focus {
                        Pane::Config => Pane::Inspect,
                        Pane::Inspect => Pane::Config,
                    };
                }
                KeyCode::Char('<') => view.resize(-5),
                KeyCode::Char('>') => view.resize(5),
                _ => view.scroll_focused(key),
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
        }
    }

    /// Open (or focus) an integrated terminal for the current selection. `t`
    /// dedups against a live terminal for the same container; `T`
    /// (`force_new`) always spawns a fresh one.
    ///
    /// This does I/O inline — a state load plus a PTY spawn — same precedent as
    /// [`Self::open_logs`] / [`Self::open_config`] doing user-triggered work
    /// without going through the event loop. Failures land in `self.status`.
    fn open_terminal(&mut self, force_new: bool) {
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
        match TermSession::spawn(title, container, argv, rows, cols) {
            Ok(session) => {
                self.terms.open(session);
                self.focus = Focus::Terminal;
                self.status = None;
            }
            Err(e) => self.status = Some(format!("terminal: {e:#}")),
        }
    }

    /// Open a full-screen log tail for the selected instance's container.
    /// Fetches the last 50 log lines (stdout+stderr merged) at open time; a
    /// missing container or runtime error renders as the body. Instances tab only;
    /// no-op with no selectable row.
    fn open_logs(&mut self) {
        if self.tab != Tab::Instances {
            return;
        }
        // Logs route to an instance; a process row uses its parent instance. A
        // no-op on sandbox / empty / orphan-group nodes.
        let Some(i) = self.selected_instance_index() else {
            return;
        };
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let Some(row) = snapshot.instances.get(i) else {
            return;
        };
        let container = row.container.clone();
        let body = match backend().logs_tail(&container, 50) {
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
        let (mut view, target) = match self.tab {
            Tab::Instances => {
                let (name, target) = match self.selected_node() {
                    Some(Node::Sandbox(i)) | Some(Node::Empty(i)) => {
                        let name = snapshot.sandboxes.get(i).map(|s| s.name.clone());
                        let target = name
                            .as_deref()
                            .and_then(|n| inspect_target(&snapshot.instances, Target::Sandbox(n)));
                        (name, target)
                    }
                    Some(Node::Instance(i)) => {
                        let name = snapshot.instances.get(i).map(|r| r.sandbox.clone());
                        let target = inspect_target(&snapshot.instances, Target::Instance(i));
                        (name, target)
                    }
                    Some(Node::Proc { instance, .. }) => {
                        let name = snapshot.instances.get(instance).map(|r| r.sandbox.clone());
                        let target =
                            inspect_target(&snapshot.instances, Target::Instance(instance));
                        (name, target)
                    }
                    Some(Node::Orphans) | None => (None, None),
                };
                let Some(name) = name else {
                    return;
                };
                (Self::build_sandbox_view(&self.dir, &name), target)
            }
            Tab::Services => {
                let Some(row) = snapshot.services.get(self.selected()) else {
                    return;
                };
                let target = service_inspect_target(&row.containers);
                (Self::build_service_view(&self.dir, &row.name), target)
            }
        };
        let placeholder = match self.tab {
            Tab::Instances => "(no running instance)",
            Tab::Services => "(no containers)",
        };
        set_inspect(&mut view, target, placeholder);
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
            inspect: String::new(),
            inspect_scroll: 0,
            inspect_container: String::new(),
            focus: Pane::Config,
            split_pct: 50,
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
            inspect: String::new(),
            inspect_scroll: 0,
            inspect_container: String::new(),
            focus: Pane::Config,
            split_pct: 50,
        }
    }
}

/// What the inspect pane targets, keyed off the selected Instances-tab node.
enum Target<'a> {
    /// An instance row by index: inspect its own container.
    Instance(usize),
    /// A sandbox by name: inspect its first running instance's container.
    Sandbox(&'a str),
}

/// Container to `docker inspect` for an Instances-tab target, or `None` when
/// there is none (an unknown index, or a sandbox with no running instance).
/// Pure over the instance rows so the per-node-kind pick is testable.
fn inspect_target(instances: &[InstanceRow], target: Target) -> Option<String> {
    match target {
        Target::Instance(i) => instances.get(i).map(|r| r.container.clone()),
        Target::Sandbox(name) => instances
            .iter()
            .find(|r| r.sandbox == name && matches!(r.status, ContainerStatus::Running(_)))
            .map(|r| r.container.clone()),
    }
}

/// Container to `docker inspect` for a Services-tab row: the first running
/// backing container, else the first container, else `None`.
fn service_inspect_target(containers: &[(String, ContainerStatus)]) -> Option<String> {
    containers
        .iter()
        .find(|(_, s)| matches!(s, ContainerStatus::Running(_)))
        .or_else(|| containers.first())
        .map(|(name, _)| name.clone())
}

/// Fill `view`'s inspect pane: fetch + pretty-print `docker inspect <container>`
/// when there is a target, else store the given placeholder text. On docker
/// error the error text is the body; on JSON parse failure the raw output is
/// kept with the parse error as the first line.
fn set_inspect(view: &mut ConfigView, target: Option<String>, placeholder: &str) {
    let Some(container) = target else {
        view.inspect = placeholder.to_string();
        return;
    };
    view.inspect_container = container.clone();
    view.inspect = match backend().inspect_json(&container) {
        Ok(out) => crate::commands::inspect::pretty_inspect(&out),
        Err(e) => format!("{e:#}"),
    };
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
            build_hash: String::new(),
            issues: Vec::new(),
        }];
        Snapshot {
            instances,
            sandboxes,
            services: Vec::new(),
            sandbox_count: 1,
            runtime_name: "docker",
            runtime_version: None,
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
            // Twenty inspect lines so the inspect pane scrolls independently.
            inspect: (0..20).map(|i| format!("insp{i}\n")).collect(),
            inspect_scroll: 0,
            inspect_container: "devsandbox-s".into(),
            focus: Pane::Config,
            split_pct: 50,
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

        // `t` toggles the config side; `tab` no longer touches it.
        app.on_key(key(KeyCode::Char('t')));
        assert_eq!(view(&app).showing, Side::Resolved);
        app.on_key(key(KeyCode::Char('t')));
        assert_eq!(view(&app).showing, Side::Original);
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
        app.on_key(key(KeyCode::Char('t')));

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

    /// A snapshot with no configured sandboxes and one instance whose sandbox is
    /// not in config, so it groups under the orphan node:
    /// tree = [Orphans, Instance(0)].
    fn orphan_snapshot() -> Snapshot {
        use super::super::data::{ContainerStatus, InstanceRow};
        let instances = vec![InstanceRow {
            name: "orphan0".into(),
            sandbox: "gone".into(),
            container: "devsandbox-orphan0".into(),
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
        }];
        Snapshot {
            instances,
            sandboxes: Vec::new(),
            services: Vec::new(),
            sandbox_count: 0,
            runtime_name: "docker",
            runtime_version: None,
            collected_at: std::time::Instant::now(),
            error: None,
        }
    }

    fn prompt(app: &App) -> &Prompt {
        app.prompt.as_ref().expect("prompt open")
    }

    #[test]
    fn r_on_sandbox_prefills_run() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with(1)); // [Sandbox(0), inst0], cursor on sandbox
        app.on_key(key(KeyCode::Char('r')));
        let p = prompt(&app);
        assert_eq!(p.input(), "run s ");
        // Cursor at the end (6 chars).
        assert_eq!(p.cursor(), "run s ".chars().count());
    }

    #[test]
    fn r_on_empty_marker_prefills_run() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with(0)); // [Sandbox(0), Empty(0)]
        app.on_key(key(KeyCode::Down)); // onto Empty(0)
        assert_eq!(app.selected_node(), Some(Node::Empty(0)));
        app.on_key(key(KeyCode::Char('r')));
        assert_eq!(prompt(&app).input(), "run s ");
    }

    #[test]
    fn r_on_instance_prefills_rename() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with(2)); // [Sandbox(0), inst0, inst1]
        app.on_key(key(KeyCode::Down)); // onto inst0
        assert_eq!(app.selected_node(), Some(Node::Instance(0)));
        app.on_key(key(KeyCode::Char('r')));
        assert_eq!(prompt(&app).input(), "rename inst0 ");
    }

    #[test]
    fn r_on_orphans_gives_empty_prompt() {
        let mut app = new_app();
        app.set_snapshot(orphan_snapshot()); // [Orphans, inst0], cursor on Orphans
        assert_eq!(app.selected_node(), Some(Node::Orphans));
        app.on_key(key(KeyCode::Char('r')));
        assert_eq!(prompt(&app).input(), "");
    }

    #[test]
    fn r_with_no_snapshot_gives_empty_prompt() {
        let mut app = new_app();
        app.on_key(key(KeyCode::Char('r')));
        assert_eq!(prompt(&app).input(), "");
    }

    #[test]
    fn prefilled_prompt_history_up_stashes_prefill() {
        // Seed one history entry, prefill, then Up shows history and Down restores
        // the prefill as the stashed live line.
        let mut app = new_app();
        app.set_snapshot(snapshot_with(1));
        app.prompt = Some(Prompt::with_input(vec!["run other".into()], "run s ".into()));
        app.on_key(key(KeyCode::Up));
        assert_eq!(prompt(&app).input(), "run other");
        app.on_key(key(KeyCode::Down));
        assert_eq!(prompt(&app).input(), "run s ");
    }

    #[test]
    fn o_on_instance_sets_pending_code() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with(1)); // [Sandbox(0), inst0]
        app.on_key(key(KeyCode::Down)); // onto inst0
        app.on_key(key(KeyCode::Char('o')));
        assert_eq!(
            app.take_pending_action(),
            Some(PromptAction::Code { instance: "inst0".into() }),
        );
    }

    #[test]
    fn o_on_orphan_instance_sets_pending_code() {
        let mut app = new_app();
        app.set_snapshot(orphan_snapshot()); // [Orphans, orphan0]
        app.on_key(key(KeyCode::Down)); // onto orphan0
        assert_eq!(app.selected_node(), Some(Node::Instance(0)));
        app.on_key(key(KeyCode::Char('o')));
        assert_eq!(
            app.take_pending_action(),
            Some(PromptAction::Code { instance: "orphan0".into() }),
        );
    }

    #[test]
    fn o_on_sandbox_is_noop() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with(1)); // cursor on Sandbox(0)
        app.on_key(key(KeyCode::Char('o')));
        assert_eq!(app.take_pending_action(), None);
    }

    #[test]
    fn prompt_rebuild_on_services_tab_queues_service_rebuild() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with(1));
        app.tab = Tab::Services;
        app.on_key(key(KeyCode::Char(':')));
        for c in "rebuild db".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Enter));
        // On the Services tab, `rebuild <name>` targets the service, not an
        // instance.
        assert_eq!(
            app.take_pending_action(),
            Some(PromptAction::ServiceRebuild { name: "db".into() }),
        );
    }

    #[test]
    fn prompt_rebuild_on_instances_tab_stays_instance_rebuild() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with(1));
        app.tab = Tab::Instances;
        app.on_key(key(KeyCode::Char(':')));
        for c in "rebuild inst0".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Enter));
        assert_eq!(
            app.take_pending_action(),
            Some(PromptAction::Rebuild { instance: "inst0".into(), force: false }),
        );
    }

    #[test]
    fn r_and_o_ignored_on_services_tab() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with(1));
        app.tab = Tab::Services;
        app.on_key(key(KeyCode::Char('r')));
        assert!(app.prompt.is_none());
        app.on_key(key(KeyCode::Char('o')));
        assert_eq!(app.take_pending_action(), None);
    }

    /// `snapshot_with(n)` with every instance's container reporting `status`.
    fn snapshot_with_status(n: usize, status: ContainerStatus) -> Snapshot {
        let mut snap = snapshot_with(n);
        for row in &mut snap.instances {
            row.status = status.clone();
        }
        snap
    }

    fn running() -> ContainerStatus {
        ContainerStatus::Running("Up".into())
    }

    #[test]
    fn s_on_running_instance_sets_pending_stop() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with_status(1, running())); // [Sandbox(0), inst0]
        app.on_key(key(KeyCode::Down)); // onto inst0
        app.on_key(key(KeyCode::Char('s')));
        assert!(app.stopping.contains("inst0"));
        assert_eq!(app.status.as_deref(), Some("stopping inst0…"));
        assert_eq!(app.take_pending_stop(), Some("inst0".into()));
        assert_eq!(app.take_pending_start(), None);
    }

    #[test]
    fn s_on_exited_instance_sets_pending_start() {
        let mut app = new_app();
        let status = ContainerStatus::Exited("Exited (0)".into());
        app.set_snapshot(snapshot_with_status(1, status));
        app.on_key(key(KeyCode::Down)); // onto inst0
        app.on_key(key(KeyCode::Char('s')));
        assert!(app.starting.contains("inst0"));
        assert_eq!(app.status.as_deref(), Some("starting inst0…"));
        assert_eq!(app.take_pending_start(), Some("inst0".into()));
        assert_eq!(app.take_pending_stop(), None);
    }

    #[test]
    fn s_on_exited_drifted_instance_queues_rebuild() {
        let mut app = new_app();
        let status = ContainerStatus::Exited("Exited (0)".into());
        let mut snap = snapshot_with_status(1, status);
        snap.instances[0].drift = true;
        app.set_snapshot(snap);
        app.on_key(key(KeyCode::Down)); // onto inst0
        app.on_key(key(KeyCode::Char('s')));
        assert_eq!(
            app.take_pending_action(),
            Some(PromptAction::Rebuild { instance: "inst0".into(), force: false })
        );
        // Drift takes the suspend path, not the background start guard.
        assert_eq!(app.take_pending_start(), None);
        assert!(app.starting.is_empty());
        assert_eq!(app.take_pending_stop(), None);
    }

    #[test]
    fn s_on_exited_undrifted_instance_still_bare_starts() {
        let mut app = new_app();
        let status = ContainerStatus::Exited("Exited (0)".into());
        let snap = snapshot_with_status(1, status); // drift: false
        app.set_snapshot(snap);
        app.on_key(key(KeyCode::Down)); // onto inst0
        app.on_key(key(KeyCode::Char('s')));
        assert!(app.starting.contains("inst0"));
        assert_eq!(app.take_pending_start(), Some("inst0".into()));
        assert_eq!(app.take_pending_action(), None);
    }

    #[test]
    fn s_on_running_drifted_instance_still_stops() {
        let mut app = new_app();
        let mut snap = snapshot_with_status(1, running());
        snap.instances[0].drift = true;
        app.set_snapshot(snap);
        app.on_key(key(KeyCode::Down)); // onto inst0
        app.on_key(key(KeyCode::Char('s')));
        assert!(app.stopping.contains("inst0"));
        assert_eq!(app.take_pending_stop(), Some("inst0".into()));
        assert_eq!(app.take_pending_action(), None);
        assert_eq!(app.take_pending_start(), None);
    }

    #[test]
    fn s_on_missing_container_queues_rebuild() {
        // A missing container can't be bare-started; `s` recreates it via the
        // modal CLI rebuild (worktree preserved), like the drifted-exited case.
        let mut app = new_app();
        app.set_snapshot(snapshot_with(1)); // status Missing
        app.on_key(key(KeyCode::Down)); // onto inst0
        app.on_key(key(KeyCode::Char('s')));
        assert_eq!(
            app.take_pending_action(),
            Some(PromptAction::Rebuild { instance: "inst0".into(), force: false })
        );
        assert_eq!(app.take_pending_stop(), None);
        assert_eq!(app.take_pending_start(), None);
        assert!(app.stopping.is_empty());
        assert!(app.starting.is_empty());
    }

    #[test]
    fn s_on_orphan_instance_sets_pending_stop() {
        let mut app = new_app();
        let mut snap = orphan_snapshot(); // [Orphans, orphan0]
        snap.instances[0].status = running();
        app.set_snapshot(snap);
        app.on_key(key(KeyCode::Down)); // onto orphan0
        assert_eq!(app.selected_node(), Some(Node::Instance(0)));
        app.on_key(key(KeyCode::Char('s')));
        assert_eq!(app.take_pending_stop(), Some("orphan0".into()));
    }

    #[test]
    fn s_on_sandbox_is_noop() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with(1)); // cursor on Sandbox(0)
        app.on_key(key(KeyCode::Char('s')));
        assert_eq!(app.take_pending_stop(), None);
        assert!(app.stopping.is_empty());
    }

    #[test]
    fn s_dedupes_while_stop_in_flight() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with_status(1, running()));
        app.on_key(key(KeyCode::Down)); // onto inst0
        app.on_key(key(KeyCode::Char('s')));
        assert_eq!(app.take_pending_stop(), Some("inst0".into()));
        // Still in flight (loop hasn't cleared `stopping`): a second `s` is a no-op.
        app.on_key(key(KeyCode::Down)); // keep cursor on inst0
        app.on_key(key(KeyCode::Char('s')));
        assert_eq!(app.take_pending_stop(), None);
    }

    #[test]
    fn s_dedupes_while_start_in_flight() {
        let mut app = new_app();
        let status = ContainerStatus::Exited("Exited (0)".into());
        app.set_snapshot(snapshot_with_status(1, status));
        app.on_key(key(KeyCode::Down)); // onto inst0
        app.on_key(key(KeyCode::Char('s')));
        assert_eq!(app.take_pending_start(), Some("inst0".into()));
        // Still in flight (loop hasn't cleared `starting`): a second `s` is a no-op.
        app.on_key(key(KeyCode::Down)); // keep cursor on inst0
        app.on_key(key(KeyCode::Char('s')));
        assert_eq!(app.take_pending_start(), None);
    }

    #[test]
    fn s_ignored_on_services_tab() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with(1));
        app.tab = Tab::Services;
        app.on_key(key(KeyCode::Char('s')));
        assert_eq!(app.take_pending_stop(), None);
    }

    /// Seed the proc cache + expansion for an instance so proc rows are visible
    /// without a fetch.
    fn expand_with_rows(app: &mut App, instance: &str, pids: &[&str]) {
        use super::super::procs::{ProcRow, ProcState};
        let rows = pids
            .iter()
            .map(|p| ProcRow { pid: p.to_string(), depth: 0, args: "x".into() })
            .collect();
        app.expanded_procs.insert(instance.to_string());
        app.procs.insert(instance.to_string(), ProcState::Rows(rows));
    }

    #[test]
    fn right_on_instance_expands_procs_and_signals_fetch() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with(1)); // [Sandbox(0), inst0]
        app.on_key(key(KeyCode::Down)); // onto inst0
        app.on_key(key(KeyCode::Right));
        assert!(app.expanded_procs.contains("inst0"));
        assert!(app.take_needs_proc_fetch());
        // Consumed once.
        assert!(!app.take_needs_proc_fetch());
    }

    #[test]
    fn space_toggles_procs_on_instance() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with(1));
        app.on_key(key(KeyCode::Down)); // onto inst0
        app.on_key(key(KeyCode::Char(' ')));
        assert!(app.expanded_procs.contains("inst0"));
        app.on_key(key(KeyCode::Char(' ')));
        assert!(!app.expanded_procs.contains("inst0"));
    }

    #[test]
    fn left_ladder_proc_to_instance_to_sandbox() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with(1)); // [Sandbox(0), inst0]
        expand_with_rows(&mut app, "inst0", &["10", "20"]);
        // Tree: [Sandbox(0), Instance(0), Proc r0, Proc r1].
        app.on_key(key(KeyCode::Down)); // inst0 (pos 1)
        app.on_key(key(KeyCode::Down)); // proc r0 (pos 2)
        app.on_key(key(KeyCode::Down)); // proc r1 (pos 3)
        assert_eq!(app.selected_node(), Some(Node::Proc { instance: 0, row: 1 }));

        // First Left: proc → its instance row.
        app.on_key(key(KeyCode::Left));
        assert_eq!(app.selected_node(), Some(Node::Instance(0)));
        // Procs still expanded (jump didn't collapse them).
        assert!(app.expanded_procs.contains("inst0"));

        // Second Left: instance with procs expanded → collapse procs, stay put.
        app.on_key(key(KeyCode::Left));
        assert_eq!(app.selected_node(), Some(Node::Instance(0)));
        assert!(!app.expanded_procs.contains("inst0"));

        // Third Left: instance → parent sandbox.
        app.on_key(key(KeyCode::Left));
        assert_eq!(app.selected_node(), Some(Node::Sandbox(0)));
    }

    #[test]
    fn s_on_proc_row_stops_parent_instance() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with_status(1, running()));
        expand_with_rows(&mut app, "inst0", &["10"]);
        app.on_key(key(KeyCode::Down)); // inst0
        app.on_key(key(KeyCode::Down)); // proc row
        assert_eq!(app.selected_node(), Some(Node::Proc { instance: 0, row: 0 }));
        app.on_key(key(KeyCode::Char('s')));
        assert!(app.stopping.contains("inst0"));
        assert_eq!(app.take_pending_stop(), Some("inst0".into()));
    }

    #[test]
    fn o_on_proc_row_codes_parent_instance() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with(1));
        expand_with_rows(&mut app, "inst0", &["10"]);
        app.on_key(key(KeyCode::Down)); // inst0
        app.on_key(key(KeyCode::Down)); // proc row
        app.on_key(key(KeyCode::Char('o')));
        assert_eq!(
            app.take_pending_action(),
            Some(PromptAction::Code { instance: "inst0".into() }),
        );
    }

    #[test]
    fn proc_fetch_targets_running_and_not_running() {
        use super::super::data::{ContainerStatus, InstanceRow, SandboxRow};
        let mut app = new_app();
        let mk = |name: &str, status: ContainerStatus| InstanceRow {
            name: name.into(),
            sandbox: "s".into(),
            container: format!("devsandbox-{name}"),
            status,
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
        };
        let snap = Snapshot {
            instances: vec![
                mk("up", ContainerStatus::Running("Up".into())),
                mk("down", ContainerStatus::Exited("Exited".into())),
            ],
            sandboxes: vec![SandboxRow {
                name: "s".into(),
                source: "image x".into(),
                folder: None,
                services: Vec::new(),
                extends: Vec::new(),
                config_hash: "h".into(),
                build_hash: String::new(),
                issues: Vec::new(),
            }],
            services: Vec::new(),
            sandbox_count: 1,
            runtime_name: "docker",
            runtime_version: None,
            collected_at: std::time::Instant::now(),
            error: None,
        };
        app.set_snapshot(snap);
        app.expanded_procs.insert("up".into());
        app.expanded_procs.insert("down".into());

        let targets = app.proc_fetch_targets();
        assert_eq!(targets, vec![("up".to_string(), "devsandbox-up".to_string())]);
        // Non-running instance got a (not running) message row, no fetch.
        assert_eq!(
            app.procs.get("down"),
            Some(&super::super::procs::ProcState::Message("(not running)".into())),
        );
    }

    #[test]
    fn agent_count_counts_agent_rows_in_cache() {
        use super::super::procs::{ProcRow, ProcState};
        let mut app = new_app();
        // Nothing cached → unknown.
        assert_eq!(app.agent_count("x"), None);

        let rows = vec![
            ProcRow { pid: "1".into(), depth: 0, args: "/sbin/init".into() },
            ProcRow { pid: "2".into(), depth: 1, args: "node /usr/local/bin/claude".into() },
            ProcRow { pid: "3".into(), depth: 1, args: "zidane --resume".into() },
        ];
        app.procs.insert("x".into(), ProcState::Rows(rows));
        assert_eq!(app.agent_count("x"), Some(2));

        // A message state (not running / error) has no countable forest.
        app.procs.insert("y".into(), ProcState::Message("(not running)".into()));
        assert_eq!(app.agent_count("y"), None);
    }

    #[test]
    fn proc_fetch_targets_includes_selected_running_instance() {
        use super::super::data::{ContainerStatus, InstanceRow, SandboxRow};
        let mut app = new_app();
        let row = InstanceRow {
            name: "up".into(),
            sandbox: "s".into(),
            container: "devsandbox-up".into(),
            status: ContainerStatus::Running("Up".into()),
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
        };
        app.set_snapshot(Snapshot {
            instances: vec![row],
            sandboxes: vec![SandboxRow {
                name: "s".into(),
                source: "image x".into(),
                folder: None,
                services: Vec::new(),
                extends: Vec::new(),
                config_hash: "h".into(),
                build_hash: String::new(),
                issues: Vec::new(),
            }],
            services: Vec::new(),
            sandbox_count: 1,
            runtime_name: "docker",
            runtime_version: None,
            collected_at: std::time::Instant::now(),
            error: None,
        });

        // Cursor on the sandbox node: no instance selected, nothing expanded.
        assert!(app.proc_fetch_targets().is_empty());

        // Move onto the running instance: it becomes a fetch target for the
        // Detail agent count, without being expanded.
        app.on_key(key(KeyCode::Down));
        assert_eq!(
            app.proc_fetch_targets(),
            vec![("up".to_string(), "devsandbox-up".to_string())],
        );
        assert!(app.expanded_procs.is_empty());
    }

    #[test]
    fn scroll_reclamps_on_side_toggle() {
        let mut app = new_app();
        open_modal(&mut app);
        // Scroll to bottom of the 10-line resolved side.
        app.on_key(key(KeyCode::Char('t')));
        app.on_key(key(KeyCode::Char('G')));
        assert_eq!(view(&app).scroll, 9);

        // Toggling back to the 2-line original re-clamps the offset.
        app.on_key(key(KeyCode::Char('t')));
        assert_eq!(view(&app).showing, Side::Original);
        assert_eq!(view(&app).scroll, 1);
    }

    fn mouse(kind: MouseEventKind, column: u16) -> MouseEvent {
        MouseEvent { kind, column, row: 0, modifiers: KeyModifiers::NONE }
    }

    fn mouse_at(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE }
    }

    fn term_sess(title: &str, container: &str) -> TermSession {
        TermSession::test_session(title, container, 24, 80)
    }

    // A frame large enough that the terminal panel is laid out; the panel Rect
    // is derived from the same helper the real code uses so tests can't drift.
    const FRAME: Rect = Rect { x: 0, y: 0, width: 100, height: 40 };

    // The config modal is full-screen, so its divider math only reads the width;
    // a 100-wide area keeps the pre-Rect tests' column→percent mapping intact.
    const MODAL_AREA: Rect = Rect { x: 0, y: 0, width: 100, height: 40 };

    #[test]
    fn mouse_click_in_panel_focuses_terminal() {
        let mut app = new_app();
        app.terms.open(term_sess("web-1", "devsandbox-web-1"));
        assert_eq!(app.focus, Focus::Dashboard);
        let panel = super::super::ui::terminal_panel_rect(FRAME, false);
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
        let panel = super::super::ui::terminal_panel_rect(FRAME, false);
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
        let panel = super::super::ui::terminal_panel_rect(FRAME, false);
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
        let panel = super::super::ui::terminal_panel_rect(FRAME, false);
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
    fn tab_switches_focus_between_panes() {
        let mut app = new_app();
        open_modal(&mut app);
        assert_eq!(view(&app).focus, Pane::Config);
        app.on_key(key(KeyCode::Tab));
        assert_eq!(view(&app).focus, Pane::Inspect);
        app.on_key(key(KeyCode::Tab));
        assert_eq!(view(&app).focus, Pane::Config);
        // `t` still toggles the side, never the focus.
        app.on_key(key(KeyCode::Char('t')));
        assert_eq!(view(&app).focus, Pane::Config);
    }

    #[test]
    fn scroll_routes_to_focused_pane() {
        let mut app = new_app();
        open_modal(&mut app);
        // Config focus, resolved side (10 lines): Down moves config scroll only.
        app.on_key(key(KeyCode::Char('t')));
        app.on_key(key(KeyCode::Down));
        assert_eq!(view(&app).scroll, 1);
        assert_eq!(view(&app).inspect_scroll, 0);

        // Focus the inspect pane (20 lines): Down moves inspect scroll only.
        app.on_key(key(KeyCode::Tab));
        app.on_key(key(KeyCode::Down));
        assert_eq!(view(&app).scroll, 1);
        assert_eq!(view(&app).inspect_scroll, 1);

        // G on the inspect pane jumps to its last line (20 → max 19).
        app.on_key(key(KeyCode::Char('G')));
        assert_eq!(view(&app).inspect_scroll, 19);
    }

    #[test]
    fn split_pct_clamps_via_resize_keys() {
        let mut app = new_app();
        open_modal(&mut app);
        assert_eq!(view(&app).split_pct, 50);
        // `<` shrinks by 5, `>` grows by 5.
        app.on_key(key(KeyCode::Char('<')));
        assert_eq!(view(&app).split_pct, 45);
        app.on_key(key(KeyCode::Char('>')));
        app.on_key(key(KeyCode::Char('>')));
        assert_eq!(view(&app).split_pct, 55);

        // Clamp at the lower bound.
        for _ in 0..20 {
            app.on_key(key(KeyCode::Char('<')));
        }
        assert_eq!(view(&app).split_pct, SPLIT_MIN);
        // Clamp at the upper bound.
        for _ in 0..40 {
            app.on_key(key(KeyCode::Char('>')));
        }
        assert_eq!(view(&app).split_pct, SPLIT_MAX);
    }

    #[test]
    fn divider_pct_maps_column_to_clamped_percentage() {
        // Mid-column of a 100-wide modal → 50%.
        assert_eq!(divider_pct(50, 100), 50);
        // Extremes clamp into [SPLIT_MIN, SPLIT_MAX].
        assert_eq!(divider_pct(0, 100), SPLIT_MIN);
        assert_eq!(divider_pct(100, 100), SPLIT_MAX);
        assert_eq!(divider_pct(5, 100), SPLIT_MIN);
        assert_eq!(divider_pct(95, 100), SPLIT_MAX);
        // Zero width never panics.
        assert_eq!(divider_pct(10, 0), SPLIT_MIN);
    }

    #[test]
    fn mouse_drag_resizes_divider() {
        let mut app = new_app();
        open_modal(&mut app);
        // Width 100, split 50 → divider at column 50. Press on it starts a drag.
        app.on_mouse(&mouse(MouseEventKind::Down(MouseButton::Left), 50), MODAL_AREA);
        // Drag to column 30 → 30%.
        app.on_mouse(&mouse(MouseEventKind::Drag(MouseButton::Left), 30), MODAL_AREA);
        assert_eq!(view(&app).split_pct, 30);
        // Release; a later drag with no press does nothing.
        app.on_mouse(&mouse(MouseEventKind::Up(MouseButton::Left), 30), MODAL_AREA);
        app.on_mouse(&mouse(MouseEventKind::Drag(MouseButton::Left), 70), MODAL_AREA);
        assert_eq!(view(&app).split_pct, 30);
    }

    #[test]
    fn mouse_press_far_from_divider_ignored() {
        let mut app = new_app();
        open_modal(&mut app);
        // Divider at 50; press at 10 is far away → no drag started.
        app.on_mouse(&mouse(MouseEventKind::Down(MouseButton::Left), 10), MODAL_AREA);
        app.on_mouse(&mouse(MouseEventKind::Drag(MouseButton::Left), 70), MODAL_AREA);
        assert_eq!(view(&app).split_pct, 50);
    }

    #[test]
    fn wheel_scrolls_pane_under_cursor() {
        let mut app = new_app();
        open_modal(&mut app);
        app.on_key(key(KeyCode::Char('t'))); // resolved side, 10 lines
        // Divider at column 50. Wheel down left of it scrolls config by 3.
        app.on_mouse(&mouse(MouseEventKind::ScrollDown, 10), MODAL_AREA);
        assert_eq!(view(&app).scroll, 3);
        assert_eq!(view(&app).inspect_scroll, 0);
        // Wheel down right of it scrolls inspect by 3.
        app.on_mouse(&mouse(MouseEventKind::ScrollDown, 90), MODAL_AREA);
        assert_eq!(view(&app).inspect_scroll, 3);
        // Wheel up clamps at 0.
        app.on_mouse(&mouse(MouseEventKind::ScrollUp, 10), MODAL_AREA);
        app.on_mouse(&mouse(MouseEventKind::ScrollUp, 10), MODAL_AREA);
        assert_eq!(view(&app).scroll, 0);
    }

    #[test]
    fn inspect_target_picks_per_node_kind() {
        use super::super::data::{ContainerStatus, InstanceRow};
        let mk = |name: &str, sandbox: &str, status: ContainerStatus| InstanceRow {
            name: name.into(),
            sandbox: sandbox.into(),
            container: format!("devsandbox-{name}"),
            status,
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
        };
        let rows = vec![
            mk("a", "s", ContainerStatus::Exited("x".into())),
            mk("b", "s", ContainerStatus::Running("Up".into())),
            mk("c", "t", ContainerStatus::Missing),
        ];

        // Instance node → that instance's own container, regardless of status.
        assert_eq!(
            inspect_target(&rows, Target::Instance(0)),
            Some("devsandbox-a".into()),
        );
        // Sandbox with a running instance → its first running container.
        assert_eq!(
            inspect_target(&rows, Target::Sandbox("s")),
            Some("devsandbox-b".into()),
        );
        // Sandbox with no running instance → None.
        assert_eq!(inspect_target(&rows, Target::Sandbox("t")), None);
        // Unknown instance index → None.
        assert_eq!(inspect_target(&rows, Target::Instance(9)), None);
    }

    #[test]
    fn service_inspect_target_prefers_running() {
        use super::super::data::ContainerStatus;
        let running = vec![
            ("svc-1".to_string(), ContainerStatus::Exited("x".into())),
            ("svc-2".to_string(), ContainerStatus::Running("Up".into())),
        ];
        assert_eq!(service_inspect_target(&running), Some("svc-2".into()));

        // No running → first container.
        let none_running = vec![
            ("svc-1".to_string(), ContainerStatus::Exited("x".into())),
            ("svc-2".to_string(), ContainerStatus::Missing),
        ];
        assert_eq!(service_inspect_target(&none_running), Some("svc-1".into()));

        // Empty → None.
        assert_eq!(service_inspect_target(&[]), None);
    }

    fn toks(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_string).collect()
    }

    fn cfg() -> Config {
        Config::parse(
            "[sandbox.web]\nfolder = \"code\"\nworktree-branch = \"wt/${instance}\"\n\n[sandbox.api]\nfolder = \"code\"\n",
        )
        .unwrap()
    }

    #[test]
    fn run_candidates_sandbox_position() {
        let sandboxes = vec!["api".to_string(), "web".to_string()];
        assert_eq!(
            App::candidates_for(Tab::Instances, 1, &toks("run"), &sandboxes, &[], &[], None),
            sandboxes
        );
        // Sandbox not on the line yet (only a flag pair) → still sandbox names.
        assert_eq!(
            App::candidates_for(Tab::Instances, 3, &toks("run --name x"), &sandboxes, &[], &[], None),
            sandboxes
        );
    }

    #[test]
    fn run_candidates_flags_after_sandbox() {
        let sandboxes = vec!["web".to_string()];
        // Empty token after the sandbox → the flags.
        assert_eq!(
            App::candidates_for(Tab::Instances, 2, &toks("run web"), &sandboxes, &[], &[], None),
            vec!["--name".to_string(), "--branch".to_string()]
        );
        // A `--` stem too (the prompt then filters by the stem).
        assert_eq!(
            App::candidates_for(Tab::Instances, 2, &toks("run web --"), &sandboxes, &[], &[], None),
            vec!["--name".to_string(), "--branch".to_string()]
        );
        // A flag already used is not offered again.
        assert_eq!(
            App::candidates_for(Tab::Instances, 4, &toks("run web --name x"), &sandboxes, &[], &[], None),
            vec!["--branch".to_string()]
        );
    }

    #[test]
    fn run_candidates_branch_value_offers_default() {
        let config = cfg();
        let sandboxes: Vec<String> = config.sandboxes.keys().cloned().collect();
        // Sandbox with `worktree-branch` → its pattern as the editable base.
        assert_eq!(
            App::candidates_for(Tab::Instances, 3, &toks("run web --branch"), &sandboxes, &[], &[], Some(&config)),
            vec!["wt/${instance}".to_string()]
        );
        // Without one → the built-in default pattern.
        assert_eq!(
            App::candidates_for(Tab::Instances, 3, &toks("run api --branch"), &sandboxes, &[], &[], Some(&config)),
            vec![DEFAULT_WORKTREE_BRANCH.to_string()]
        );
        // `--name` values are free-form: no candidates.
        assert!(App::candidates_for(Tab::Instances, 3, &toks("run web --name"), &sandboxes, &[], &[], Some(&config))
            .is_empty());
    }

    #[test]
    fn rebuild_completes_instance_names() {
        let instances = vec!["a".to_string(), "b".to_string()];
        assert_eq!(
            App::candidates_for(Tab::Instances, 1, &toks("rebuild"), &[], &instances, &[], None),
            instances
        );
        assert_eq!(
            App::candidates_for(Tab::Instances, 1, &toks("recreate"), &[], &instances, &[], None),
            instances
        );
    }

    #[test]
    fn rebuild_completes_service_names_on_services_tab() {
        let instances = vec!["a".to_string(), "b".to_string()];
        let services = vec!["cache".to_string(), "db".to_string()];
        // Services tab: `rebuild`/`recreate` offer service names, not instances.
        assert_eq!(
            App::candidates_for(
                Tab::Services,
                1,
                &toks("rebuild"),
                &[],
                &instances,
                &services,
                None,
            ),
            services
        );
        assert_eq!(
            App::candidates_for(
                Tab::Services,
                1,
                &toks("recreate"),
                &[],
                &instances,
                &services,
                None,
            ),
            services
        );
    }

    // --- integrated-terminal state (step 2) -----------------------------------

    use super::super::data::ServiceRow;

    /// A snapshot whose single service `svc` has `containers`.
    fn service_snapshot(containers: Vec<(String, ContainerStatus)>) -> Snapshot {
        Snapshot {
            instances: Vec::new(),
            sandboxes: Vec::new(),
            services: vec![ServiceRow {
                name: "svc".into(),
                scope: "isolated",
                source: "image x".into(),
                ports: Vec::new(),
                containers,
                used_by: Vec::new(),
                env_len: 0,
                command: None,
                config_hash: "hash".into(),
                drift: false,
            }],
            sandbox_count: 0,
            runtime_name: "docker",
            runtime_version: None,
            collected_at: std::time::Instant::now(),
            error: None,
        }
    }

    /// Open a test terminal directly (no PTY) and focus it, bypassing the
    /// I/O-bound `open_terminal`.
    fn open_test_term(app: &mut App, title: &str, container: &str) {
        app.terms
            .open(TermSession::test_session(title, container, 24, 80));
        app.focus = Focus::Terminal;
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
    fn stop_start_hint_tracks_selection_status() {
        let mut app = new_app();
        // No snapshot / nothing selected → the default verb.
        assert_eq!(app.stop_start_hint(), "stop");
        // Exited instance under the cursor → `start`.
        app.set_snapshot(snapshot_with_status(
            1,
            ContainerStatus::Exited("Exited (0)".into()),
        ));
        app.on_key(key(KeyCode::Down)); // onto inst0
        assert_eq!(app.stop_start_hint(), "start");
        // Running → `stop`; Missing (s rebuilds — a start from the user's
        // seat) → `start`.
        app.set_snapshot(snapshot_with_status(1, running()));
        assert_eq!(app.stop_start_hint(), "stop");
        app.set_snapshot(snapshot_with_status(1, ContainerStatus::Missing));
        assert_eq!(app.stop_start_hint(), "start");
    }

    #[test]
    fn run_rename_hint_tracks_selection() {
        let mut app = new_app();
        // No snapshot / sandbox row selected → `run`.
        assert_eq!(app.run_rename_hint(), "run");
        app.set_snapshot(snapshot_with(2)); // [Sandbox(0), inst0, inst1]
        assert_eq!(app.run_rename_hint(), "run");
        // Instance row under the cursor → `rename`.
        app.on_key(key(KeyCode::Down)); // onto inst0
        assert_eq!(app.run_rename_hint(), "rename");
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
