//! Dashboard state machine. Deliberately free of terminal I/O so key handling
//! and selection logic stay unit-testable; `mod.rs` owns the crossterm/ratatui
//! side and feeds decoded key events in here.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Mutex, Weak};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use super::data::{visible_nodes, ContainerStatus, Node, Snapshot};
use super::procs::ProcState;
use super::prompt::{Prompt, PromptAction};
use super::select::{Candidate, Region, RegionId, Selection};
use super::settings::Settings;
use super::term::{TermParser, TermTabs};

mod actions;
mod command_line;
mod inbox;
mod procs;
mod selection;
mod terminal;
mod thread_actions;
#[cfg(test)]
mod test_support;
mod tree;
mod view;

pub use crate::inbox::Thread;
pub use inbox::{
    chip, pane_feed, pane_header, parse_utc_offset, short_age, title_of, HeaderRow, InboxFocus, InboxView, PaneLine, Tone,
    View,
};
pub use actions::PendingDone;
pub use procs::{PendingSignal, Signal};
pub use selection::{copy_key_intercepted, Extract};
pub use view::{ConfigView, HelpModal, Modal, Pane, Side, TextModal};
use view::{col_near, divider_pct};

/// The four top-level views.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tab {
    Instances,
    Services,
    Ports,
    Inbox,
}

impl Tab {
    pub const ALL: [Tab; 4] = [Tab::Instances, Tab::Services, Tab::Ports, Tab::Inbox];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Instances => "Instances",
            Tab::Services => "Services",
            Tab::Ports => "Ports",
            Tab::Inbox => "Inbox",
        }
    }

    fn index(self) -> usize {
        match self {
            Tab::Instances => 0,
            Tab::Services => 1,
            Tab::Ports => 2,
            Tab::Inbox => 3,
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

/// One row of the Ports tab: a live forward, projected from the forwarder's
/// `ForwardStatus` by the event loop (step 10). Plain data with no dependency on
/// the unix-only forward module, so [`App`] compiles on every platform.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortRow {
    /// Stable forward id, used to target a `d` (stop) at the right forward.
    pub id: u64,
    /// Host side, e.g. `"127.0.0.1:3000"`.
    pub local: String,
    /// Route label, e.g. `"api:3000"` or `"postgres:5432 (via instance api)"`.
    /// Already carries the `(via …)` suffix, so the table needs no VIA column.
    pub target: String,
    /// Listening process (`node (pid 412)`), when known.
    pub process: Option<String>,
    /// Coarse state: `"active"`, `"connecting"`, or `"error: …"`.
    pub state: String,
    /// Open connection count.
    pub conns: usize,
    /// Started from a sandbox's `forwardPorts`, not by hand.
    pub configured: bool,
}

/// A forward the user asked for, for the event loop (step 10) to start on its
/// worker thread. Mirrors the CLI's `(instance, service, address, spec)` inputs;
/// `spec` is a validated `[host:]port` string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortRequest {
    pub instance: String,
    pub service: Option<String>,
    pub address: Option<String>,
    pub spec: String,
}

pub struct App {
    pub dir: PathBuf,
    pub tab: Tab,
    /// Selected row per tab, indexed by `Tab::index`.
    selected: [usize; 4],
    /// Collapsed tree groups on the Instances tab, keyed by sandbox name (and
    /// [`ORPHANS_NAME`](super::data::ORPHANS_NAME) for the orphan group). Empty
    /// means all expanded; survives snapshot refreshes.
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
    /// Appended to the status line the event loop writes after launching a
    /// [`PromptAction::Code`]: an Inbox `vscode` action's caveats (a goto it
    /// couldn't honor, the event and child done queued), which would
    /// otherwise be overwritten by the launch outcome.
    pub code_note: Option<String>,
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
    /// Done-flag changes the event loop should write to state on a
    /// background thread (`d`/`u` on an instance row, and a thread's child
    /// on the Inbox thread `done`/reopen).
    pub pending_done: Vec<PendingDone>,
    /// A `kill` the event loop should spawn on a background thread (the
    /// SIGTERM/SIGKILL shortcuts on a process row). Runs without suspending.
    pub pending_signal: Option<PendingSignal>,
    /// Live forwards shown on the Ports tab, fed by the forwarder worker (step
    /// 10). Empty until the worker reports; set via [`Self::set_ports`].
    pub ports: Vec<PortRow>,
    /// A forward the event loop should start on its worker thread (the `p`
    /// shortcut / `port` prompt). Consumed by step 10; sits here until then.
    pub pending_port: Option<PortRequest>,
    /// Id of a forward the event loop should stop (the `d` shortcut on the Ports
    /// tab). Consumed by step 10.
    pub pending_unport: Option<u64>,
    /// Container notifications shown on the Inbox tab (docs/automations.md),
    /// as last loaded from the shared store (`crate::inbox::store`).
    pub inbox: InboxView,
    /// Changes the event loop owes the store (`d`, `D`, mark-read); already
    /// applied to `inbox`, which the reload after them confirms.
    pub pending_inbox: Vec<crate::inbox::Op>,
    /// A notification link the event loop should hand to the desktop opener
    /// (`enter` in the Inbox thread pane).
    pub pending_open: Option<String>,
    /// Instances (state keys) whose daemon bridge the event loop should ask
    /// for (`bridges.ensure`): a terminal just opened on them, relaying the
    /// ssh-agent.
    pub pending_bridges: Vec<String>,
    /// Every dispatcher's children by key, owner id → key → instance name
    /// (`dispatch::thread_children`), refreshed by the event loop with each
    /// snapshot so the thread pane resolves a `child` without reading state.
    pub thread_children: BTreeMap<String, BTreeMap<String, String>>,
    /// Seconds east of UTC for Inbox timestamps; the event loop sets it once
    /// at startup (0 = UTC when unknown).
    pub utc_offset: i64,
    /// One-line status shown in the help-bar area (e.g. `code` launch outcome).
    pub status: Option<String>,
    /// True while the config-modal divider is being dragged with the mouse.
    dragging_divider: bool,
    /// Whether keys drive the dashboard or the active terminal.
    pub focus: Focus,
    /// Open integrated-terminal tabs, shared across both top-level tabs. Empty
    /// until the user opens one with `t`.
    pub terms: TermTabs,
    /// Whether the outer terminal speaks the kitty keyboard protocol, so new
    /// terminals offer it to their child. Set once by the event loop.
    pub kitty: bool,
    /// The mouse text selection, shown until a press elsewhere, `esc`, a tab
    /// switch or a modal opening/closing clears it (`selection.rs`).
    selection: Option<Selection>,
    /// A left press inside a region that a drag would turn into a selection.
    candidate: Option<Candidate>,
    /// Selectable regions of the last frame, registered by `ui::draw` (hence
    /// the `RefCell`: drawing borrows the app immutably), so a press is
    /// hit-tested against what is on screen.
    regions: RefCell<Vec<Region>>,
    /// Set by a release ending a selection drag or a copy key: the next draw
    /// reads the text off the frame it drew, into `clipboard` to copy it, or
    /// into `measured` for the `selected N chars` hint.
    extract_requested: Cell<Option<Extract>>,
    /// Text the event loop owes the clipboard (OSC 52), filled by the draw.
    clipboard: RefCell<Option<String>>,
    /// Char count of a selection released without copying (copy on select
    /// off), filled by the draw for the event loop's status hint.
    measured: Cell<Option<(usize, RegionId)>>,
    /// Dashboard settings (`settings.rs`), loaded by the event loop at
    /// startup; the `?` modal flips them.
    pub settings: Settings,
    /// A toggle changed `settings` since the event loop last saved them.
    settings_dirty: bool,
    /// The outer terminal keeps ctrl+shift+c for its own copy
    /// (`selection::copy_key_intercepted`, detected by the event loop at
    /// startup): hints and help name ctrl-c instead. Wording only.
    pub copy_key_intercepted: bool,
    /// The button of a press forwarded to a mouse-tracking terminal child,
    /// until its release: its drags and release go to the child too (clamped
    /// to the body when they leave it), and the press forms no selection.
    term_mouse_down: Option<MouseButton>,
    /// The last hover report sent to an `AnyMotion` terminal child: which
    /// session (a `Weak` so a closed tab's slot can't be mistaken for a new
    /// one) and the body cell. crossterm reports a `Moved` per pixel-cell
    /// change in the *outer* terminal, so the same body cell repeats; this
    /// sends each cell once. Cleared off the body and on any press, so the
    /// next hover after either is always reported.
    term_hover: Option<(Weak<Mutex<TermParser>>, (u16, u16))>,
    pub should_quit: bool,
}

impl App {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            tab: Tab::Instances,
            selected: [0; 4],
            collapsed: BTreeSet::new(),
            expanded_procs: BTreeSet::new(),
            procs: BTreeMap::new(),
            needs_proc_fetch: false,
            snapshot: None,
            modal: Modal::None,
            prompt: None,
            pending_action: None,
            code_note: None,
            pending_stop: None,
            stopping: BTreeSet::new(),
            pending_start: None,
            starting: BTreeSet::new(),
            pending_signal: None,
            pending_done: Vec::new(),
            ports: Vec::new(),
            pending_port: None,
            pending_unport: None,
            inbox: InboxView::default(),
            pending_inbox: Vec::new(),
            pending_open: None,
            pending_bridges: Vec::new(),
            thread_children: BTreeMap::new(),
            utc_offset: 0,
            status: None,
            dragging_divider: false,
            focus: Focus::Dashboard,
            terms: TermTabs::default(),
            kitty: false,
            selection: None,
            candidate: None,
            regions: RefCell::new(Vec::new()),
            extract_requested: Cell::new(None),
            clipboard: RefCell::new(None),
            measured: Cell::new(None),
            settings: Settings::default(),
            settings_dirty: false,
            copy_key_intercepted: false,
            term_mouse_down: None,
            term_hover: None,
            should_quit: false,
        }
    }

    /// Install a freshly collected snapshot and re-clamp the Instances selection
    /// in case rows shrank. I/O-free: the caller does the collecting. A snapshot
    /// collected without stats keeps the previous cpu/mem of containers that
    /// are still running, so the columns don't blank between stats rounds.
    pub fn set_snapshot(&mut self, mut snapshot: Snapshot) {
        if let (false, Some(prev)) = (snapshot.stats, &self.snapshot) {
            for row in &mut snapshot.instances {
                if !matches!(row.status, ContainerStatus::Running(_)) {
                    continue;
                }
                if let Some(old) = prev.instances.iter().find(|o| o.container == row.container) {
                    row.cpu.clone_from(&old.cpu);
                    row.mem.clone_from(&old.mem);
                }
            }
        }
        self.snapshot = Some(snapshot);
        self.clamp_selection();
    }

    /// Install the forwarder's latest rows and re-clamp the Ports selection in
    /// case it shrank. I/O-free: the event loop (step 10) does the collecting.
    #[cfg_attr(not(unix), allow(dead_code))] // fed by the unix-only forward worker
    pub fn set_ports(&mut self, ports: Vec<PortRow>) {
        self.ports = ports;
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
            Tab::Ports => self.ports.len(),
            Tab::Inbox => self.inbox.rows().len(),
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

    /// Whether a tree group (sandbox name /
    /// [`ORPHANS_NAME`](super::data::ORPHANS_NAME)) is collapsed. Used by the
    /// renderer to pick the ▸/▾ marker.
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
        let i = self.tab.index();
        self.set_tab(Tab::ALL[(i + 1) % Tab::ALL.len()]);
    }

    pub fn prev_tab(&mut self) {
        let i = self.tab.index();
        self.set_tab(Tab::ALL[(i + Tab::ALL.len() - 1) % Tab::ALL.len()]);
    }

    /// Apply a key event to the state. No terminal I/O here (the modal open path
    /// reads config/fs, which is local and user-triggered — see [`Self::open_config`]).
    pub fn on_key(&mut self, key: KeyEvent) {
        let modal = std::mem::discriminant(&self.modal);
        let term = (self.terms.active(), self.terms.sessions().len());
        self.dispatch_key(key);
        // A modal opening or closing, or another terminal tab (`[`/`]`/`x`,
        // `t`), changes what's on screen under the selection: a terminal
        // selection would otherwise read the next session's rows.
        if std::mem::discriminant(&self.modal) != modal || (self.terms.active(), self.terms.sessions().len()) != term {
            self.clear_selection();
        }
        // Whatever the key did to the Inbox cursor (moved it, dismissed the
        // row under it, switched views), the pane follows in one place.
        self.sync_inbox_selection();
    }

    fn dispatch_key(&mut self, key: KeyEvent) {
        // `esc` drops a selection first, and only that, wherever the keys go
        // except a focused terminal's shell, which owns its `esc`.
        let to_shell = self.prompt.is_none() && matches!(self.modal, Modal::None) && self.focus == Focus::Terminal;
        if key.code == KeyCode::Esc && self.selection.is_some() && !to_shell {
            self.clear_selection();
            return;
        }
        // The copy shortcut over a selection, even into a focused terminal
        // (this runs ahead of `on_key_terminal`): it re-copies rather than
        // reaching the PTY. Without a selection it goes on as before.
        if self.selection_key(&key) {
            return;
        }
        // A legacy terminal sends ctrl+shift+c as plain ctrl+c, which would
        // quit: over a dashboard selection it copies (a second one, with the
        // selection cleared by `esc` or a click, still quits). A focused shell
        // keeps its ctrl+c (SIGINT), the prompt its own.
        if self.selection.is_some()
            && !to_shell
            && self.prompt.is_none()
            && key.code == KeyCode::Char('c')
            && key.modifiers == KeyModifiers::CONTROL
        {
            self.extract_requested.set(Some(Extract::Copy));
            return;
        }
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
        // A focused Inbox thread (or its input) shadows the dashboard keys:
        // the `1`-`9` action keys over the tab keys, text over everything.
        if self.tab == Tab::Inbox && self.inbox.focus != InboxFocus::List {
            self.on_key_inbox_pane(key);
            return;
        }
        // Any dashboard key dismisses a lingering status line.
        self.status = None;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // A selected process row has its own action set: signals only. The
        // instance-forwarding shortcuts (rename/vscode/stop/logs/config/terminal)
        // are disabled here, leaving navigation and the SIGTERM/SIGKILL keys.
        let on_proc = self.tab == Tab::Instances
            && matches!(self.selected_node(), Some(Node::Proc { .. }));
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('c') if ctrl => self.should_quit = true,
            KeyCode::Char(':') => self.open_prompt(),
            KeyCode::Tab => self.next_tab(),
            KeyCode::BackTab => self.prev_tab(),
            KeyCode::Char('1') => self.set_tab(Tab::Instances),
            KeyCode::Char('2') => self.set_tab(Tab::Services),
            KeyCode::Char('3') => self.set_tab(Tab::Ports),
            KeyCode::Char('4') => self.set_tab(Tab::Inbox),
            // Enter a terminal: ctrl-] or F12. No-op with a status hint when none
            // are open.
            KeyCode::Char(']') if ctrl => self.enter_terminal(),
            KeyCode::F(12) => self.enter_terminal(),
            // Inbox list: `o`/`t`/`l`/`p`/`d`/`u` act on the selected thread,
            // ahead of the instance keys they share letters with.
            KeyCode::Char('o' | 't' | 'l' | 'p' | 'd' | 'u') if self.tab == Tab::Inbox && !ctrl => {
                self.on_inbox_thread_key(key.code);
            }
            // Process-row signals; `t` doubles as SIGTERM there, otherwise it
            // opens a terminal.
            KeyCode::Char('t') if on_proc => self.signal_selected_proc(Signal::Term),
            KeyCode::Char('K') if on_proc => self.signal_selected_proc(Signal::Kill),
            KeyCode::Char('t') => self.open_terminal(false),
            KeyCode::Char('T') if !on_proc => self.open_terminal(true),
            KeyCode::Char(']') => self.terms.next(),
            KeyCode::Char('[') => self.terms.prev(),
            KeyCode::Char('x') => self.terms.close_active(),
            KeyCode::Up | KeyCode::Char('k') => self.select_up(),
            KeyCode::Down | KeyCode::Char('j') => self.select_down(),
            KeyCode::Right if self.tab == Tab::Instances => self.tree_expand(),
            KeyCode::Char(' ') if self.tab == Tab::Instances => self.tree_toggle(),
            KeyCode::Left if self.tab == Tab::Instances => self.tree_collapse(),
            // Inbox list: `enter` focuses the selected thread (whose own
            // `enter` opens the link), `r`/`i` its input; `←`/`→` step the
            // views.
            KeyCode::Enter if self.tab == Tab::Inbox => self.focus_inbox_thread(),
            KeyCode::Char('r' | 'i') if self.tab == Tab::Inbox => self.focus_inbox_input(),
            KeyCode::Left if self.tab == Tab::Inbox => self.step_inbox_view(-1),
            KeyCode::Right if self.tab == Tab::Inbox => self.step_inbox_view(1),
            KeyCode::Enter | KeyCode::Char('e') if !on_proc => self.open_config(),
            KeyCode::Char('r') if self.tab == Tab::Instances && !on_proc => {
                self.open_rename_or_run_prompt()
            }
            KeyCode::Char('o') if self.tab == Tab::Instances && !on_proc => self.attach_code(),
            KeyCode::Char('s') if self.tab == Tab::Instances && !on_proc => {
                self.stop_or_start_instance()
            }
            KeyCode::Char('d') if self.tab == Tab::Instances && !on_proc => self.set_selected_done(true),
            KeyCode::Char('u') if self.tab == Tab::Instances && !on_proc => self.set_selected_done(false),
            KeyCode::Char('l') if !on_proc => self.open_logs(),
            // Forwarding: `p` opens the `port` prompt prefilled from the selected
            // instance (Instances, not a process row) or service (Services); `d`
            // stops the selected forward on the Ports tab.
            KeyCode::Char('p') if self.tab == Tab::Instances && !on_proc => {
                self.open_port_prompt_instance()
            }
            KeyCode::Char('p') if self.tab == Tab::Services => self.open_port_prompt_service(),
            KeyCode::Char('d') if self.tab == Tab::Ports => self.stop_selected_forward(),
            KeyCode::Char('D') if self.tab == Tab::Inbox => self.clear_notifications(),
            KeyCode::Char('?') => self.open_help(),
            _ => {}
        }
    }

    /// Apply a mouse event over the full frame `area`. The config modal (when
    /// open) owns the mouse: a press/drag on or near the divider column resizes
    /// the split; the scroll wheel scrolls the pane under the cursor. Otherwise,
    /// with terminals open, clicks and the wheel drive the terminal panel
    /// ([`Self::terminal_mouse`]); on the Inbox tab, the list/thread divider
    /// drags and the list and pane take clicks ([`Self::inbox_mouse`]); a
    /// click on a tab title switches to it ([`Self::tab_bar_mouse`]). Text
    /// selection comes first ([`Self::selection_mouse`]): a drag it owns
    /// never reaches them, and a press reaches both. I/O-free.
    pub fn on_mouse(&mut self, ev: &MouseEvent, area: Rect) {
        let modal = std::mem::discriminant(&self.modal);
        if !self.selection_mouse(ev) {
            self.route_mouse(ev, area);
            if ev.kind == MouseEventKind::Down(MouseButton::Left) {
                self.selection_press(ev);
            }
        }
        if std::mem::discriminant(&self.modal) != modal {
            self.clear_selection();
        }
    }

    /// [`Self::on_mouse`] for everything but text selection.
    fn route_mouse(&mut self, ev: &MouseEvent, area: Rect) {
        let Modal::Config(view) = &mut self.modal else {
            self.dragging_divider = false;
            self.help_mouse(ev, area);
            self.inbox_mouse(ev, area);
            self.tab_bar_mouse(ev, area);
            self.terminal_mouse(ev, area);
            // As after a key: a click or wheel may have moved the Inbox cursor.
            self.sync_inbox_selection();
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

    /// A left press on a tab title switches to that tab, through [`Self::set_tab`]
    /// like the `1`-`4` keys (so leaving the Inbox reads what it showed). Not
    /// under a modal or the prompt, which own the input. Unlike the keys it
    /// works while an Inbox thread has focus: the click names its target.
    /// The terminal panel's click-away unfocuses a focused terminal on the
    /// same press. The hit-test is `ui`'s, shared with the drawing.
    fn tab_bar_mouse(&mut self, ev: &MouseEvent, area: Rect) {
        if ev.kind != MouseEventKind::Down(MouseButton::Left)
            || self.prompt.is_some()
            || !matches!(self.modal, Modal::None)
        {
            return;
        }
        if let Some(tab) = super::ui::tab_hit(self, area, ev.column, ev.row) {
            self.set_tab(tab);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    #[test]
    fn tab_switching() {
        let mut app = new_app();
        assert_eq!(app.tab, Tab::Instances);

        // `tab` cycles forward over all four tabs and wraps.
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.tab, Tab::Services);
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.tab, Tab::Ports);
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.tab, Tab::Inbox);
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.tab, Tab::Instances);

        // `S-tab` cycles backward and wraps (no longer a toggle).
        app.on_key(key(KeyCode::BackTab));
        assert_eq!(app.tab, Tab::Inbox);
        app.on_key(key(KeyCode::BackTab));
        assert_eq!(app.tab, Tab::Ports);
        app.on_key(key(KeyCode::BackTab));
        assert_eq!(app.tab, Tab::Services);
        app.on_key(key(KeyCode::BackTab));
        assert_eq!(app.tab, Tab::Instances);

        // Number keys jump directly.
        app.on_key(key(KeyCode::Char('2')));
        assert_eq!(app.tab, Tab::Services);
        app.on_key(key(KeyCode::Char('3')));
        assert_eq!(app.tab, Tab::Ports);
        app.on_key(key(KeyCode::Char('4')));
        assert_eq!(app.tab, Tab::Inbox);
        app.on_key(key(KeyCode::Char('1')));
        assert_eq!(app.tab, Tab::Instances);
    }

    #[test]
    fn click_tab_title_switches_tabs() {
        let mut app = new_app();
        let press = |col, row| mouse_at(MouseEventKind::Down(MouseButton::Left), col, row);
        let start = |app: &App, tab: Tab| {
            super::super::ui::tab_spans(app).into_iter().find(|(t, _, _)| *t == tab).unwrap().2
        };
        for tab in [Tab::Services, Tab::Ports, Tab::Inbox, Tab::Instances] {
            let r = start(&app, tab);
            app.on_mouse(&press(r.end - 1, 0), FRAME);
            assert_eq!(app.tab, tab);
        }
        // The divider after a title and the row below hit nothing.
        let r = start(&app, Tab::Services);
        app.on_mouse(&press(r.end, 0), FRAME);
        app.on_mouse(&press(r.start, 1), FRAME);
        assert_eq!(app.tab, Tab::Instances);
        // Not under the prompt or a modal.
        app.on_key(key(KeyCode::Char(':')));
        app.on_mouse(&press(r.start, 0), FRAME);
        assert_eq!(app.tab, Tab::Instances);
        app.on_key(key(KeyCode::Esc));
        app.on_key(key(KeyCode::Char('?')));
        app.on_mouse(&press(r.start, 0), FRAME);
        assert_eq!(app.tab, Tab::Instances);
    }

    #[test]
    fn click_tab_unfocuses_a_terminal() {
        let mut app = new_app();
        open_test_term(&mut app, "web-1", "devsandbox-web-1");
        assert_eq!(app.focus, Focus::Terminal);
        let r = super::super::ui::tab_spans(&app).into_iter().find(|(t, _, _)| *t == Tab::Ports).unwrap().2;
        app.on_mouse(&mouse_at(MouseEventKind::Down(MouseButton::Left), r.start, 0), FRAME);
        assert_eq!(app.tab, Tab::Ports);
        assert_eq!(app.focus, Focus::Dashboard);
    }

    #[test]
    fn set_ports_reclamps_selection() {
        let mut app = new_app();
        app.tab = Tab::Ports;
        app.set_ports(port_rows(3));
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Down)); // last row (idx 2)
        assert_eq!(app.selected(), 2);

        // Shrinking to one row re-clamps to the last valid index.
        app.set_ports(port_rows(1));
        assert_eq!(app.selected(), 0);

        // Emptying re-clamps to 0.
        app.set_ports(port_rows(0));
        assert_eq!(app.selected(), 0);
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
}
