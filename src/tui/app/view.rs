//! Full-screen modals: the config/inspect explorer (`ConfigView`), help and
//! log text modals, and the split-pane scroll/resize geometry they share.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;

use crate::config::Config;
use crate::runtime::backend;
use crate::tui::data::{ContainerStatus, InstanceRow, Node};

use super::{App, Tab};

/// Keybinding reference shown by the `?` overlay, grouped by context.
const HELP_BODY: &str = "\
Global
  q, ctrl-c   quit
  tab / S-tab switch tab      1-4  jump to tab
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
  p           forward a port (instance / service selection prefills the prompt)

Ports tab
  ↑/k ↓/j     move selection
  d           stop the selected forward

Inbox tab (container notifications: devsbd notify)
  ↑/k ↓/j     move selection   (entering the tab marks all read)
  enter       open the selected notification's link
  d           dismiss          D   clear all

Process rows (expanded instance)
  ←           jump to the parent instance
  t           SIGTERM          K   SIGKILL
  (the instance shortcuts above are disabled on a process row;
   signals need `ps` in the image — a `top` fallback listing hides them)

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
  run <sandbox> [--name n] [--branch b] [--base ref]
  exec <instance> <cmd…>
  code <instance>   rm <instance>   rename <instance> <new-name>
  stop <instance>   start <instance>
  port <instance> [--service s] [--address a] <[host:]port>
              (an instance is required; a global service is reached by
               naming any instance that references it)
  tab         complete / cycle
  ↑ ↓         history
  ctrl-u      clear line       ctrl-w  delete word
  esc         cancel";

/// A scrollable full-screen text overlay (help, logs). Body is captured once at
/// open time; scrolling is the only interaction. Shared scroll math lives in
/// [`scroll_key`]/[`line_count`] so the config modal and this stay in sync.
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
pub(super) fn col_near(col: u16, divider: u16) -> bool {
    col.abs_diff(divider) <= 1
}

/// Whether `(col, row)` falls inside `rect` (border included). Pure so the
/// terminal-panel hit-testing stays unit-testable.
pub(super) fn point_in(rect: Rect, col: u16, row: u16) -> bool {
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

impl App {
    /// Scroll the pane under `col` (left of `divider` = config, else inspect) by
    /// `delta` lines, clamped to that pane's line count.
    pub(super) fn wheel_scroll(view: &mut ConfigView, col: u16, divider: u16, delta: i16) {
        if col < divider {
            let max = view.line_count().saturating_sub(1);
            view.scroll = apply_delta(view.scroll, delta, max);
        } else {
            let max = line_count(&view.inspect).saturating_sub(1);
            view.inspect_scroll = apply_delta(view.inspect_scroll, delta, max);
        }
    }

    /// Key handling while any modal is open. `esc`/`q` (and `?` for the help
    /// overlay) close; the config modal additionally toggles sides on `tab`;
    /// scroll keys are shared across all three. Assumes `self.modal` is not
    /// `None`.
    pub(super) fn on_key_modal(&mut self, key: KeyEvent) {
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
    pub(super) fn open_help(&mut self) {
        self.modal = Modal::Help(TextModal::new(" help — keys ".to_string(), HELP_BODY.to_string()));
    }

    /// Open a full-screen log tail for the selected instance's container.
    /// Fetches the last 50 log lines (stdout+stderr merged) at open time; a
    /// missing container or runtime error renders as the body. Instances tab only;
    /// no-op with no selectable row.
    pub(super) fn open_logs(&mut self) {
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
    pub(super) fn open_config(&mut self) {
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
            // The Ports and Inbox tabs have no config to explore; `e` is a
            // no-op (`enter` on the Inbox opens a link instead).
            Tab::Ports | Tab::Inbox => return,
        };
        let placeholder = match self.tab {
            Tab::Instances => "(no running instance)",
            Tab::Services => "(no containers)",
            Tab::Ports | Tab::Inbox => "",
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
    use crate::tui::app::test_support::*;
    use crate::tui::app::*;

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
        use crate::tui::data::{ContainerStatus, InstanceRow};
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
            instance_id: String::new(),
            dispatcher: None,
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
        use crate::tui::data::ContainerStatus;
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
}
