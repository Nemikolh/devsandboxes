//! Full-screen modals: the config/inspect explorer (`ConfigView`), help and
//! log text modals, and the split-pane scroll/resize geometry they share.

use std::cell::Cell;

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use crate::build_log::Tail;
use crate::config::SandboxText;
use crate::snapshot::PhaseRow;
use crate::tui::data::{ContainerStatus, InstanceRow, Node};
use crate::tui::settings::{SETTINGS, Settings};

use super::{App, Tab};

/// Keybinding reference shown by the `?` overlay, grouped by context;
/// `{copy}` is the copy-key lines, [`help_body`] picks them.
const HELP_BODY: &str = "\
Global
  q, ctrl-c   quit
  tab / S-tab switch tab      1-4  jump to tab
  :           command prompt  ?    settings & help
  drag        select (copies on release with \"copy on select\" on, above)
{copy}
  shift-drag  the outer terminal's own selection (whole rows, across panes)

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
               exited or a failed instance is rebuilt instead)
  b           build log of the instance (follows a build in flight;
              G sticks to the bottom again)
  p           forward a port (instance / service selection prefills the prompt)
  d           remove the instance now, no prompt (Instances; like :rm,
              so a dirty worktree still refuses); on an instance still
              building: cancel it, after a y/n (a cancelled run is undone)
  run, rebuild and start (s, :run, :rebuild, :start) run in the background
  and outlive the dashboard: the row shows their phase, b their output

Ports tab
  ↑/k ↓/j     move selection
  d           stop the selected forward

Inbox tab (dispatcher threads + devsbd notify records)
  The list is on the left, the selected thread on the right; selecting a
  thread marks it read. esc steps back: input → thread → list.
Inbox list
  ↑/k ↓/j     move selection
  ← / →       previous / next view: Needs you / Active / Done / All
              (Needs you: threads asking for you + unread notify records;
               leaving the tab marks the notify records it showed read)
  enter       focus the thread
  r / i       focus the reply input (when the thread takes replies)
  d           dismiss a notify record; on a dispatcher thread, mark it done
  u           reopen a done thread
  D           clear all notify records (dispatcher threads stay)
  o t l p     VS Code / terminal / logs / forward prompt on the thread's
              child (or its sender when it has none)
Inbox thread (shadows the dashboard keys while focused)
  The header (title, state, child, action row) stays put; the feed
  under it is newest first and scrolls on its own.
  ↑/k ↓/j     scroll    pgup/pgdn  page   g / G  newest / oldest
  enter       open the thread's link (↗)
  1-9         run an action: host verbs (⌂) run now; the rest is an
              event the dispatcher pulls with `devsbd events` (✓ = done)
  r / i       focus the reply input
  tab         focus the open forms (pinned under the header), else the
              reply input
  d / u       mark done / reopen (each also an event for the dispatcher;
              also marks / clears the thread's child done)
  o t l p     as in the list (the [o] [t] [l] [p] buttons)
  m           markdown: rendered / raw source (every thread, this session)
  esc         back to the list     q quit   : prompt   ? help
Inbox form (the open forms pinned under the thread's header)
  tab / S-tab next / previous question (across every open form)
  ↑ ↓ ← →     move over the options
  space       pick (several-pick: toggle; a text question: edit)
  e           edit a text question in the composer box; enter or esc
              keeps the edit, alt-enter inserts a newline
  enter       confirm: the summary (or what's missing); enter again
              submits, esc cancels
  r / i       the reply input    esc  back to the thread
  (every pick and kept edit is saved as the form's draft)
Inbox reply input
  enter       send (the input stays, empty, for the next message)
  esc         back to the thread

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
  code <instance> [--goto path[:line[:col]]]
  rm <instance>   rename <instance> <new-name>
  stop <instance>   start <instance>
  port <instance> [--service s] [--address a] <[host:]port>
              (an instance is required; a global service is reached by
               naming any instance that references it)
  tab         complete / cycle
  ↑ ↓         history
  ctrl-u      clear line       ctrl-w  delete word
  esc         cancel";

/// [`HELP_BODY`] with copy keys that reach us. Where the outer terminal
/// keeps ctrl+shift+c (`App::copy_key_intercepted`) ctrl+c is the dashboard's
/// copy key, except in a focused shell, which gets it as usual.
fn help_body(copy_key_intercepted: bool) -> String {
    let copy = if copy_key_intercepted {
        "  ctrl-c / cmd-c        copy the selection to the clipboard (OSC 52)\n              \
         (ctrl-c not in a focused terminal: the shell gets it there)"
    } else {
        "  ctrl-shift-c / cmd-c  copy the selection to the clipboard (OSC 52)\n              \
         (ctrl-c too over a dashboard selection: legacy terminals)"
    };
    HELP_BODY.replacen("{copy}", copy, 1)
}

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

    /// Scroll by `delta` lines, clamped like the scroll keys; the new scroll.
    pub(super) fn scroll_by(&mut self, delta: i16) -> u16 {
        self.scroll = apply_delta(self.scroll, delta, line_count(&self.body).saturating_sub(1));
        self.scroll
    }
}

/// The `?` Settings & help modal: the settings rows (from
/// [`SETTINGS`](crate::tui::settings::SETTINGS)) with a cursor on top, the
/// key reference below in a [`TextModal`] that scrolls and selects as
/// before. The settings own `tab`/`S-tab` and `space`/`enter`, keys the
/// text never used, so the help keeps its `↑↓`/`j`/`k` scrolling.
pub struct HelpModal {
    pub text: TextModal,
    /// Index into `SETTINGS` of the row `space`/`enter` toggles.
    pub cursor: usize,
}

/// Lines of a build log the modal shows: the end is what matters (where it
/// failed, what it's doing), `logs --build` has the rest.
pub const BUILD_LOG_LINES: usize = 500;

/// Lines of container output the logs modal (`l`) shows.
pub const LOG_LINES: usize = 50;

/// What a modal's body waits on: a read the event loop asks the daemon for
/// ([`App::take_pending_fetch`]), answered through the matching setter
/// ([`App::set_logs`], [`App::set_config`], [`App::set_inspect`]). The
/// answer names its target, so one for a modal since closed or retargeted
/// is dropped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fetch {
    /// `containers.logs`: the last [`LOG_LINES`] lines.
    Logs { container: String },
    /// `sandboxes.get` / `services.get` in the dashboard's config root.
    Config { kind: ConfigKind, name: String },
    /// `containers.inspect` for the config view's inspect pane.
    Inspect { container: String },
}

/// What a [`ConfigView`] shows: a sandbox or a service (their names may
/// collide).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigKind {
    Sandbox,
    Service,
}

/// A modal body whose answer hasn't come yet.
const LOADING: &str = "(loading…)";

/// An instance's build log (`b`), re-read on each snapshot while the
/// instance is in flight. The event loop does the reading
/// ([`App::take_build_log_read`] / [`App::set_build_log`]), so this stays
/// I/O-free. Sticks to the bottom, following the build, until the user
/// scrolls up; scrolling back to the bottom sticks again.
pub struct BuildLogModal {
    /// The instance's row name (for the title and the hint).
    pub instance: String,
    /// Its id: the row it follows and the log's directory.
    pub instance_id: String,
    pub text: TextModal,
    /// Following the end: the shown top is [`bottom_top`], not `text.scroll`.
    pub stick: bool,
    /// In flight at the last snapshot: the next one re-reads (once more
    /// after it ends, for the last lines).
    live: bool,
    /// Body rows the last draw had, written by `ui` (only it knows them).
    pub height: Cell<u16>,
}

/// The top line that puts the last of `lines` at the bottom of `height`
/// rows.
pub(super) fn bottom_top(lines: u16, height: u16) -> u16 {
    lines.saturating_sub(height)
}

impl BuildLogModal {
    /// The first line shown.
    pub fn top(&self) -> u16 {
        if self.stick { bottom_top(line_count(&self.text.body), self.height.get()) } else { self.text.scroll }
    }

    /// Move from where it is shown, then stick when that's the bottom.
    fn scroll_with(&mut self, f: impl FnOnce(&mut TextModal)) -> u16 {
        self.text.scroll = self.top();
        f(&mut self.text);
        self.stick = self.text.scroll >= bottom_top(line_count(&self.text.body), self.height.get());
        self.top()
    }

    fn on_scroll_key(&mut self, key: KeyEvent) {
        self.scroll_with(|t| {
            t.on_scroll_key(key);
        });
    }

    /// [`TextModal::scroll_by`], from where it is shown.
    pub(super) fn scroll_by(&mut self, delta: i16) -> u16 {
        self.scroll_with(|t| {
            t.scroll_by(delta);
        })
    }
}

/// The build-log modal's title: whose, and how it's going.
fn build_log_title(instance: &str, phase: Option<&PhaseRow>) -> String {
    match phase {
        Some(PhaseRow::InFlight { step }) => format!(" build log — {instance} ({}) ", crate::commands::doing(step)),
        Some(PhaseRow::Failed { step, .. }) => format!(" build log — {instance} (failed at {step}) "),
        None => format!(" build log — {instance} "),
    }
}

/// A yes/no question over the dashboard (`d` on an in-flight row): `y` or
/// `enter` goes ahead, `n`/`esc`/`q` doesn't.
pub struct ConfirmModal {
    pub question: String,
    action: Confirmed,
}

/// What a [`ConfirmModal`]'s yes does.
enum Confirmed {
    /// Cancel the instance's `run`/`rebuild`/`start` (`App::pending_cancel`).
    CancelBuild(String),
}

/// Line count of `body`, clamped to `u16`, for scroll bounds.
fn line_count(body: &str) -> u16 {
    body.lines().count().min(u16::MAX as usize) as u16
}

/// Apply a scroll key to `scroll`, clamped to `[0, lines-1]`. Returns `true`
/// when the key was a scroll key (consumed), `false` otherwise. Shared by the
/// config modal, [`TextModal`] and the Inbox thread pane.
pub(super) fn scroll_key(scroll: &mut u16, lines: u16, key: KeyEvent) -> bool {
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

/// Full-screen config explorer state. Opens with "(loading…)" bodies the
/// daemon's answers fill (`sandboxes.get`/`services.get`, `containers.inspect`);
/// on error the bodies carry the error text so the modal still renders.
pub struct ConfigView {
    /// Sandbox (or service) name, shown in the title.
    pub title: String,
    /// Whether `title` names a sandbox or a service: with it, the key of the
    /// config answer this view takes.
    pub kind: ConfigKind,
    /// Raw table TOML as written (plus `extends` for sandboxes).
    pub original: String,
    /// Merged table TOML. Equal to `original` for services.
    pub resolved: String,
    pub showing: Side,
    pub scroll: u16,
    /// Short config hash of the resolved table, empty when unavailable.
    pub hash: String,
    /// Pretty-printed `docker inspect` (or explanatory text) for the target
    /// container, asked for at open time.
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

    /// Scroll `pane` by `delta` lines, clamped to its line count; its new
    /// scroll.
    pub(super) fn scroll_pane(&mut self, pane: Pane, delta: i16) -> u16 {
        match pane {
            Pane::Config => {
                self.scroll = apply_delta(self.scroll, delta, self.line_count().saturating_sub(1));
                self.scroll
            }
            Pane::Inspect => {
                let max = line_count(&self.inspect).saturating_sub(1);
                self.inspect_scroll = apply_delta(self.inspect_scroll, delta, max);
                self.inspect_scroll
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
    /// Settings & keybinding reference (`?`).
    Help(HelpModal),
    /// Container log tail (`l` on the Instances tab); whose is
    /// [`App::logs_container`].
    Logs(TextModal),
    /// An instance's build log (`b` on the Instances tab).
    BuildLog(BuildLogModal),
    Confirm(ConfirmModal),
}

impl App {
    /// Scroll the pane under `col` (left of `divider` = config, else inspect) by
    /// `delta` lines, clamped to that pane's line count.
    pub(super) fn wheel_scroll(view: &mut ConfigView, col: u16, divider: u16, delta: i16) {
        view.scroll_pane(if col < divider { Pane::Config } else { Pane::Inspect }, delta);
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
                let n = SETTINGS.len();
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') => self.modal = Modal::None,
                    KeyCode::Tab if n > 0 => view.cursor = (view.cursor + 1) % n,
                    KeyCode::BackTab if n > 0 => view.cursor = (view.cursor + n - 1) % n,
                    KeyCode::Char(' ') | KeyCode::Enter => {
                        let i = view.cursor;
                        self.toggle_setting(i);
                    }
                    _ => {
                        view.text.on_scroll_key(key);
                    }
                }
            }
            Modal::Logs(view) => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
                    self.modal = Modal::None;
                } else {
                    view.on_scroll_key(key);
                }
            }
            Modal::BuildLog(view) => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
                    self.modal = Modal::None;
                } else {
                    view.on_scroll_key(key);
                }
            }
            Modal::Confirm(view) => match key.code {
                KeyCode::Char('y') | KeyCode::Enter => {
                    let Confirmed::CancelBuild(instance) = &view.action;
                    self.status = Some(format!("cancelling {instance}…"));
                    self.pending_cancel = Some(instance.clone());
                    self.modal = Modal::None;
                }
                KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q') => self.modal = Modal::None,
                _ => {}
            },
        }
    }

    /// Ask before cancelling `instance`'s build (`d` on an in-flight row).
    pub(super) fn confirm_cancel_build(&mut self, instance: String) {
        self.modal = Modal::Confirm(ConfirmModal {
            question: format!("cancel {instance}'s build?"),
            action: Confirmed::CancelBuild(instance),
        });
    }

    /// Take the instance whose build the event loop should cancel.
    pub fn take_pending_cancel(&mut self) -> Option<String> {
        self.pending_cancel.take()
    }

    /// `b` (Instances tab): the selected instance's build log, read by the
    /// event loop (see [`BuildLogModal`]). No-op off an instance row; an
    /// unrecorded one (no id) has no log to read.
    pub(super) fn open_build_log(&mut self) {
        if self.tab != Tab::Instances {
            return;
        }
        let Some(i) = self.selected_instance_index() else {
            return;
        };
        let Some(row) = self.snapshot.as_ref().and_then(|s| s.instances.get(i)) else {
            return;
        };
        let title = build_log_title(&row.name, row.phase.as_ref());
        let (body, read) = if row.instance_id.is_empty() {
            (format!("(no build log: `{}` is not a recorded instance)", row.name), None)
        } else {
            ("(loading…)".to_string(), Some(row.instance_id.clone()))
        };
        self.pending_build_log = read;
        self.modal = Modal::BuildLog(BuildLogModal {
            instance: row.name.clone(),
            instance_id: row.instance_id.clone(),
            text: TextModal::new(title, body),
            stick: true,
            live: matches!(row.phase, Some(PhaseRow::InFlight { .. })),
            height: Cell::new(0),
        });
    }

    /// The build log the event loop should ask the daemon for now, by
    /// instance id: just opened, or a snapshot landed while it's being
    /// written. `None` while an earlier read is unanswered, so a snapshot
    /// every 2 s can't pile reads up: the queued one goes after the answer.
    pub fn take_build_log_read(&mut self) -> Option<String> {
        if self.build_log_in_flight {
            return None;
        }
        let read = self.pending_build_log.take();
        self.build_log_in_flight = read.is_some();
        read
    }

    /// Install what the daemon answered for `instance_id` (`Err`: why it
    /// couldn't), if its modal is still the open one. Keeps the user's
    /// scroll position unless following the bottom. Any answer frees the
    /// read slot (one is outstanding at a time, whichever modal asked).
    pub fn set_build_log(&mut self, instance_id: &str, read: Result<Option<Tail>, String>) {
        self.build_log_in_flight = false;
        let Modal::BuildLog(view) = &mut self.modal else {
            return;
        };
        if view.instance_id != instance_id {
            return;
        }
        view.text.body = match read {
            Ok(Some(tail)) if tail.cut => format!(
                "… (earlier lines: devsandbox logs --build {})\n{}",
                view.instance, tail.text
            ),
            Ok(Some(tail)) if tail.text.is_empty() => "(empty build log)".to_string(),
            Ok(Some(tail)) => tail.text,
            Ok(None) => format!(
                "(no build log for `{}` yet: run, rebuild and start write one)",
                view.instance
            ),
            Err(e) => e,
        };
        let max = line_count(&view.text.body).saturating_sub(1);
        view.text.scroll = view.text.scroll.min(max);
    }

    /// After a snapshot: retitle an open build log with its instance's
    /// phase, and re-read it while that instance is in flight (plus once
    /// after, for the lines the build ended with).
    pub(super) fn refresh_build_log(&mut self) {
        let Modal::BuildLog(view) = &mut self.modal else {
            return;
        };
        let row = self
            .snapshot
            .as_ref()
            .and_then(|s| s.instances.iter().find(|r| !view.instance_id.is_empty() && r.instance_id == view.instance_id));
        let Some(row) = row else {
            // Removed (a cancelled `run` takes its record and log along):
            // what was read stays up, nothing more to follow.
            view.live = false;
            return;
        };
        view.instance.clone_from(&row.name);
        view.text.title = build_log_title(&row.name, row.phase.as_ref());
        let was = std::mem::replace(&mut view.live, matches!(row.phase, Some(PhaseRow::InFlight { .. })));
        if was || view.live {
            self.pending_build_log = Some(view.instance_id.clone());
        }
    }

    /// Open the Settings & help overlay: the settings, then all keybindings
    /// grouped by context.
    pub(super) fn open_help(&mut self) {
        let text = TextModal::new(" settings & help ".to_string(), help_body(self.copy_key_intercepted));
        self.modal = Modal::Help(HelpModal { text, cursor: 0 });
    }

    /// Flip setting `i` of `SETTINGS` (a key or a click in the `?` modal),
    /// with its cursor following a click, and mark the settings for the
    /// event loop to save.
    pub(super) fn toggle_setting(&mut self, i: usize) {
        if i >= SETTINGS.len() {
            return;
        }
        self.settings.toggle(i);
        self.settings_dirty = true;
        if let Modal::Help(view) = &mut self.modal {
            view.cursor = i;
        }
    }

    /// The settings to save, once after each change; `None` when unchanged.
    pub fn take_settings_save(&mut self) -> Option<Settings> {
        std::mem::take(&mut self.settings_dirty).then_some(self.settings)
    }

    /// A left press on a settings row of the `?` modal toggles it. The
    /// hit-test is `ui`'s, shared with the drawing.
    pub(super) fn help_mouse(&mut self, ev: &MouseEvent, area: Rect) {
        if ev.kind != MouseEventKind::Down(MouseButton::Left) || !matches!(self.modal, Modal::Help(_)) {
            return;
        }
        if let Some(i) = crate::tui::ui::help_setting_hit(area, ev.column, ev.row) {
            self.toggle_setting(i);
        }
    }

    /// Open a full-screen log tail for the selected instance's container:
    /// the last [`LOG_LINES`] lines (stdout+stderr merged), asked of the
    /// daemon at open time; a missing container or runtime error renders as
    /// the body. Instances tab only; no-op with no selectable row.
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
        self.open_logs_for(&container);
    }

    /// The log tail modal for `container`, whatever is selected: the Inbox
    /// thread pane targets a thread's child, not a table row.
    pub(super) fn open_logs_for(&mut self, container: &str) {
        let title = format!(" logs — {container} (last {LOG_LINES}) ");
        self.modal = Modal::Logs(TextModal::new(title, LOADING.to_string()));
        self.logs_container = Some(container.to_string());
        self.pending_fetch.push(Fetch::Logs { container: container.to_string() });
    }

    /// The reads the event loop should ask the daemon for, in order.
    pub fn take_pending_fetch(&mut self) -> Vec<Fetch> {
        std::mem::take(&mut self.pending_fetch)
    }

    /// Install `container`'s log tail (or why there's none) if its logs
    /// modal is still the open one.
    pub fn set_logs(&mut self, container: &str, body: String) {
        let Modal::Logs(view) = &mut self.modal else {
            return;
        };
        if self.logs_container.as_deref() != Some(container) {
            return;
        }
        view.body = body;
        view.scroll = view.scroll.min(line_count(&view.body).saturating_sub(1));
    }

    /// Install a config answer (`Err`: its message, in both bodies) if the
    /// open config view is `kind` `name`'s.
    pub fn set_config(&mut self, kind: ConfigKind, name: &str, text: Result<SandboxText, String>) {
        let Modal::Config(view) = &mut self.modal else {
            return;
        };
        if view.kind != kind || view.title != name {
            return;
        }
        match text {
            Ok(text) => (view.original, view.resolved, view.hash) = (text.original, text.resolved, text.hash),
            Err(msg) => {
                view.original = msg.clone();
                view.resolved = msg;
            }
        }
        view.clamp_scroll();
    }

    /// Install `container`'s pretty-printed inspect (or why there's none) if
    /// the open config view's inspect pane shows it.
    pub fn set_inspect(&mut self, container: &str, body: String) {
        let Modal::Config(view) = &mut self.modal else {
            return;
        };
        if view.inspect_container.is_empty() || view.inspect_container != container {
            return;
        }
        view.inspect = body;
        view.inspect_scroll = view.inspect_scroll.min(line_count(&view.inspect).saturating_sub(1));
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
        let (kind, name, target) = match self.tab {
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
                (ConfigKind::Sandbox, name, target)
            }
            Tab::Services => {
                let Some(row) = snapshot.services.get(self.selected()) else {
                    return;
                };
                let target = service_inspect_target(&row.containers);
                (ConfigKind::Service, row.name.clone(), target)
            }
            // The Ports and Inbox tabs have no config to explore; `e` is a
            // no-op (`enter` on the Inbox focuses the thread instead).
            Tab::Ports | Tab::Inbox => return,
        };
        let placeholder = match self.tab {
            Tab::Instances => "(no running instance)",
            Tab::Services => "(no containers)",
            Tab::Ports | Tab::Inbox => "",
        };
        // Sandbox: original = raw table, resolved = merged; a service's two
        // are the same (services don't `extends`). The daemon fills both.
        self.pending_fetch.push(Fetch::Config { kind, name: name.clone() });
        let (inspect, inspect_container) = match target {
            Some(container) => {
                self.pending_fetch.push(Fetch::Inspect { container: container.clone() });
                (LOADING.to_string(), container)
            }
            None => (placeholder.to_string(), String::new()),
        };
        self.modal = Modal::Config(ConfigView {
            title: name,
            kind,
            original: LOADING.to_string(),
            resolved: LOADING.to_string(),
            showing: Side::Original,
            scroll: 0,
            hash: String::new(),
            inspect,
            inspect_scroll: 0,
            inspect_container,
            focus: Pane::Config,
            split_pct: 50,
        });
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

    fn help(app: &App) -> &HelpModal {
        match &app.modal {
            Modal::Help(v) => v,
            _ => panic!("help not open"),
        }
    }

    #[test]
    fn settings_toggle_in_the_help_modal_and_are_marked_for_saving() {
        let mut app = new_app();
        app.on_key(key(KeyCode::Char('?')));
        assert_eq!(app.take_settings_save(), None, "opening changes nothing");
        assert_eq!(help(&app).cursor, 0);
        app.on_key(key(KeyCode::Char(' ')));
        assert!(!app.settings.copy_on_select, "on by default");
        assert_eq!(app.take_settings_save(), Some(app.settings));
        assert_eq!(app.take_settings_save(), None, "once per change");
        // tab / S-tab move the cursor (wrapping); enter toggles too.
        app.on_key(key(KeyCode::Tab));
        assert_eq!(help(&app).cursor, 1);
        app.on_key(key(KeyCode::Enter));
        assert!(!app.settings.terminal_clipboard);
        app.on_key(key(KeyCode::Tab));
        assert_eq!(help(&app).cursor, 0);
        app.on_key(key(KeyCode::BackTab));
        assert_eq!(help(&app).cursor, 1);
        // ↑↓ still scroll the help, not the settings.
        app.on_key(key(KeyCode::Down));
        assert_eq!((help(&app).cursor, help(&app).text.scroll), (1, 1));
        assert_eq!(
            app.take_settings_save(),
            Some(Settings { copy_on_select: false, terminal_clipboard: false })
        );
    }

    #[test]
    fn the_help_names_ctrl_c_where_the_terminal_keeps_ctrl_shift_c() {
        let body = |intercepted| {
            let mut app = new_app();
            app.copy_key_intercepted = intercepted;
            app.on_key(key(KeyCode::Char('?')));
            help(&app).text.body.clone()
        };
        let plain = body(false);
        let line = |b: &str, start: &str| b.lines().find(|l| l.starts_with(start)).map(str::to_string);
        let copy = line(&plain, "  ctrl-shift-c / cmd-c").expect("copy line");
        assert!(!plain.contains("{copy}"));
        let moved = body(true);
        assert!(line(&moved, "  ctrl-shift-c").is_none(), "{moved}");
        let ctrl_c = line(&moved, "  ctrl-c / cmd-c ").expect("ctrl-c line");
        // The description stays in the same column.
        let col = |l: &str| l.find("copy the selection").unwrap();
        assert_eq!(col(&ctrl_c), col(&copy));
        assert_eq!(plain.lines().count(), moved.lines().count());
    }

    #[test]
    fn a_click_on_a_settings_row_toggles_it() {
        let mut app = new_app();
        app.on_key(key(KeyCode::Char('?')));
        let press = |col, row| mouse_at(MouseEventKind::Down(MouseButton::Left), col, row);
        // Inside the border: row 1 is the first setting, row 2 the second.
        app.on_mouse(&press(5, 2), FRAME);
        assert!(!app.settings.terminal_clipboard);
        assert_eq!(help(&app).cursor, 1, "the cursor follows");
        assert!(app.take_settings_save().is_some());
        // The border, the gap row and the help text toggle nothing.
        for (col, row) in [(5, 0), (0, 1), (5, 3), (5, 10)] {
            app.on_mouse(&press(col, row), FRAME);
            assert_eq!(app.take_settings_save(), None, "({col}, {row})");
        }
        // Not without the modal.
        app.on_key(key(KeyCode::Esc));
        app.on_mouse(&press(5, 1), FRAME);
        assert_eq!(app.settings, Settings { copy_on_select: true, terminal_clipboard: false });
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

    fn logs(app: &App) -> &TextModal {
        match &app.modal {
            Modal::Logs(v) => v,
            _ => panic!("logs modal not open"),
        }
    }

    #[test]
    fn logs_modal_opens_on_selected_instance() {
        // Instances tab with a row: the modal opens at once, loading, and
        // queues the daemon read.
        let mut app = new_app();
        app.set_snapshot(snapshot_with(1));
        // Cursor starts on the sandbox node; move down to the instance.
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Char('l')));
        assert!(logs(&app).title.contains("devsandbox-inst0"));
        assert_eq!(logs(&app).body, "(loading…)");
        assert_eq!(app.take_pending_fetch(), [Fetch::Logs { container: "devsandbox-inst0".into() }]);
        assert_eq!(app.take_pending_fetch(), [], "once");
        app.on_key(key(KeyCode::Char('q')));
        assert!(matches!(app.modal, Modal::None));
    }

    #[test]
    fn the_logs_answer_fills_its_own_modal_only() {
        let mut app = new_app();
        app.open_logs_for("devsandbox-a");
        // Another container's (a modal since retargeted): dropped.
        app.set_logs("devsandbox-b", "b's".into());
        assert_eq!(logs(&app).body, "(loading…)");
        app.set_logs("devsandbox-a", "l1\nl2".into());
        assert_eq!(logs(&app).body, "l1\nl2");
        // Reopened on b: a's late answer doesn't land there.
        app.on_key(key(KeyCode::Esc));
        app.open_logs_for("devsandbox-b");
        app.set_logs("devsandbox-a", "late".into());
        assert_eq!(logs(&app).body, "(loading…)");
        // Closed: dropped, nothing opens.
        app.on_key(key(KeyCode::Esc));
        app.set_logs("devsandbox-b", "late".into());
        assert!(matches!(app.modal, Modal::None));
        // Nor into another modal.
        app.open_help();
        app.set_logs("devsandbox-b", "late".into());
        assert!(!help(&app).text.body.contains("late"));
    }

    #[test]
    fn the_config_view_loads_then_fills_from_its_answers() {
        let mut app = new_app();
        let mut snap = snapshot_with(1);
        snap.instances[0].status = ContainerStatus::Running("Up".into());
        app.set_snapshot(snap);
        app.on_key(key(KeyCode::Down)); // inst0
        app.on_key(key(KeyCode::Char('e')));
        assert_eq!((view(&app).original.as_str(), view(&app).resolved.as_str()), ("(loading…)", "(loading…)"));
        assert_eq!((view(&app).inspect.as_str(), view(&app).inspect_container.as_str()), ("(loading…)", "devsandbox-inst0"));
        let sandbox = view(&app).title.clone();
        assert_eq!(
            app.take_pending_fetch(),
            [
                Fetch::Config { kind: ConfigKind::Sandbox, name: sandbox.clone() },
                Fetch::Inspect { container: "devsandbox-inst0".into() },
            ]
        );
        // A service of the same name, another sandbox, another container: dropped.
        let text = |o: &str| SandboxText { original: o.into(), resolved: format!("{o}+"), hash: "abc".into() };
        app.set_config(ConfigKind::Service, &sandbox, Ok(text("svc")));
        app.set_config(ConfigKind::Sandbox, "other", Ok(text("other")));
        app.set_inspect("devsandbox-x", "x's".into());
        assert_eq!((view(&app).original.as_str(), view(&app).inspect.as_str()), ("(loading…)", "(loading…)"));
        app.set_config(ConfigKind::Sandbox, &sandbox, Ok(text("mine")));
        app.set_inspect("devsandbox-inst0", "[\n  {}\n]".into());
        let v = view(&app);
        assert_eq!((v.original.as_str(), v.resolved.as_str(), v.hash.as_str()), ("mine", "mine+", "abc"));
        assert_eq!(v.inspect, "[\n  {}\n]");
        // An error fills both sides.
        app.set_config(ConfigKind::Sandbox, &sandbox, Err("config: needs devsandbox serve".into()));
        assert_eq!((view(&app).original.as_str(), view(&app).resolved.as_str()), ("config: needs devsandbox serve", "config: needs devsandbox serve"));
        // Closed: dropped.
        app.on_key(key(KeyCode::Esc));
        app.set_config(ConfigKind::Sandbox, &sandbox, Ok(text("late")));
        app.set_inspect("devsandbox-inst0", "late".into());
        assert!(matches!(app.modal, Modal::None));
    }

    #[test]
    fn the_config_view_keeps_its_placeholder_without_a_target() {
        // A sandbox with no running instance: no inspect read, the placeholder.
        let mut app = new_app();
        app.set_snapshot(snapshot_with(1)); // inst0 not running
        app.on_key(key(KeyCode::Char('e'))); // the sandbox row
        assert_eq!(view(&app).inspect, "(no running instance)");
        assert_eq!(view(&app).kind, ConfigKind::Sandbox);
        let fetches = app.take_pending_fetch();
        assert!(matches!(fetches.as_slice(), [Fetch::Config { kind: ConfigKind::Sandbox, .. }]), "{fetches:?}");
        // Nothing in the pane takes an inspect answer then.
        app.set_inspect("", "x".into());
        assert_eq!(view(&app).inspect, "(no running instance)");
        // A service with no containers: a service read, its placeholder.
        let mut app = new_app();
        app.set_snapshot(service_snapshot(Vec::new()));
        app.tab = Tab::Services;
        app.on_key(key(KeyCode::Char('e')));
        assert_eq!(view(&app).inspect, "(no containers)");
        assert_eq!(app.take_pending_fetch(), [Fetch::Config { kind: ConfigKind::Service, name: "svc".into() }]);
        // With one, an inspect read too, keyed by the service's kind.
        let mut app = new_app();
        app.set_snapshot(service_snapshot(vec![("devsandbox-svc-1".into(), ContainerStatus::Running("Up".into()))]));
        app.tab = Tab::Services;
        app.on_key(key(KeyCode::Char('e')));
        assert_eq!(view(&app).kind, ConfigKind::Service);
        assert_eq!(
            app.take_pending_fetch(),
            [
                Fetch::Config { kind: ConfigKind::Service, name: "svc".into() },
                Fetch::Inspect { container: "devsandbox-svc-1".into() },
            ]
        );
        app.set_config(ConfigKind::Service, "svc", Ok(SandboxText { original: "o".into(), resolved: "o".into(), hash: String::new() }));
        assert_eq!(view(&app).resolved, "o");
    }

    #[test]
    fn one_build_log_read_is_outstanding_at_a_time() {
        let mut app = new_app();
        app.set_snapshot(recorded(Some(in_flight("building"))));
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Char('b')));
        assert_eq!(app.take_build_log_read(), Some("id0".into()));
        // Snapshots land while it's unanswered: one read waits, none is sent.
        app.set_snapshot(recorded(Some(in_flight("building"))));
        app.set_snapshot(recorded(Some(in_flight("building"))));
        assert_eq!(app.take_build_log_read(), None);
        app.set_build_log("id0", tail(0..1));
        assert_eq!(app.take_build_log_read(), Some("id0".into()), "the waiting one, after the answer");
        assert_eq!(app.take_build_log_read(), None);
        // An answer for a closed modal still frees the slot.
        app.on_key(key(KeyCode::Esc));
        app.set_build_log("id0", tail(0..1));
        app.on_key(key(KeyCode::Char('b')));
        assert_eq!(app.take_build_log_read(), Some("id0".into()));
    }

    fn build_log(app: &App) -> &BuildLogModal {
        match &app.modal {
            Modal::BuildLog(v) => v,
            _ => panic!("build log not open"),
        }
    }

    fn tail(lines: std::ops::Range<usize>) -> Result<Option<Tail>, String> {
        let text = lines.map(|i| format!("l{i}")).collect::<Vec<_>>().join("\n");
        Ok(Some(Tail { text, cut: false }))
    }

    /// A snapshot with inst0 recorded (id `id0`) at `phase`.
    fn recorded(phase: Option<PhaseRow>) -> crate::tui::data::Snapshot {
        let mut snap = snapshot_with(1);
        snap.instances[0].instance_id = "id0".into();
        snap.instances[0].phase = phase;
        snap
    }

    #[test]
    fn b_opens_the_build_log_for_the_loop_to_read() {
        let mut app = new_app();
        app.set_snapshot(recorded(Some(in_flight("building"))));
        app.on_key(key(KeyCode::Char('b'))); // the sandbox row: nothing
        assert!(matches!(app.modal, Modal::None));
        app.on_key(key(KeyCode::Down)); // onto inst0
        app.on_key(key(KeyCode::Char('b')));
        assert_eq!(build_log(&app).text.title, " build log — inst0 (building) ");
        assert_eq!(app.take_build_log_read(), Some("id0".into()));
        assert_eq!(app.take_build_log_read(), None, "once");
        app.set_build_log("id0", tail(0..3));
        assert_eq!(build_log(&app).text.body, "l0\nl1\nl2");
        // Not for another instance's modal.
        app.set_build_log("other", tail(0..1));
        assert_eq!(build_log(&app).text.body, "l0\nl1\nl2");
        app.on_key(key(KeyCode::Char('q')));
        assert!(matches!(app.modal, Modal::None));
        app.set_build_log("id0", tail(0..1)); // closed: dropped
        assert!(matches!(app.modal, Modal::None));
    }

    #[test]
    fn the_build_log_says_when_there_is_none() {
        let mut app = new_app();
        app.set_snapshot(recorded(None));
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Char('b')));
        assert_eq!(build_log(&app).text.title, " build log — inst0 ");
        app.set_build_log("id0", Ok(None));
        assert!(build_log(&app).text.body.starts_with("(no build log for `inst0` yet"), "{}", build_log(&app).text.body);
        app.set_build_log("id0", Ok(Some(Tail { text: "x".into(), cut: true })));
        assert_eq!(build_log(&app).text.body, "… (earlier lines: devsandbox logs --build inst0)\nx");
        app.set_build_log("id0", Err("cannot read".into()));
        assert_eq!(build_log(&app).text.body, "cannot read");
        // An unrecorded row has nothing to read.
        let mut app = new_app();
        app.set_snapshot(snapshot_with(1)); // no instance id
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Char('b')));
        assert_eq!(app.take_build_log_read(), None);
        assert!(build_log(&app).text.body.contains("not a recorded instance"));
    }

    #[test]
    fn the_build_log_rereads_while_in_flight_and_once_after() {
        let mut app = new_app();
        app.set_snapshot(recorded(Some(in_flight("building"))));
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Char('b')));
        app.take_build_log_read();
        app.set_build_log("id0", tail(0..1));
        app.set_snapshot(recorded(Some(in_flight("postCreateCommand"))));
        assert_eq!(app.take_build_log_read(), Some("id0".into()));
        assert_eq!(build_log(&app).text.title, " build log — inst0 (running postCreateCommand) ");
        app.set_build_log("id0", tail(0..2));
        // It ended: one last read, then no more.
        app.set_snapshot(recorded(Some(failed("postCreateCommand"))));
        assert_eq!(app.take_build_log_read(), Some("id0".into()));
        app.set_build_log("id0", tail(0..3));
        assert_eq!(build_log(&app).text.title, " build log — inst0 (failed at postCreateCommand) ");
        app.set_snapshot(recorded(Some(failed("postCreateCommand"))));
        assert_eq!(app.take_build_log_read(), None);
        // A finished build's log is read once, on open.
        let mut app = new_app();
        app.set_snapshot(recorded(None));
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Char('b')));
        assert_eq!(app.take_build_log_read(), Some("id0".into()));
        app.set_snapshot(recorded(None));
        assert_eq!(app.take_build_log_read(), None);
    }

    #[test]
    fn the_build_log_sticks_to_the_bottom_until_scrolled_up() {
        let mut app = new_app();
        app.set_snapshot(recorded(Some(in_flight("building"))));
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Char('b')));
        build_log(&app).height.set(10); // as `ui` would
        app.set_build_log("id0", tail(0..30));
        assert!(build_log(&app).stick);
        assert_eq!(build_log(&app).top(), 20, "the last line at the bottom");
        app.set_build_log("id0", tail(0..40));
        assert_eq!(build_log(&app).top(), 30, "follows new lines");
        // Up unsticks, from where it was shown; new lines keep the spot.
        app.on_key(key(KeyCode::Up));
        assert!(!build_log(&app).stick);
        assert_eq!(build_log(&app).top(), 29);
        app.set_build_log("id0", tail(0..50));
        assert_eq!(build_log(&app).top(), 29);
        // Back down to the bottom sticks again; so does `G`.
        app.on_key(key(KeyCode::PageDown));
        assert!(build_log(&app).stick);
        assert_eq!(build_log(&app).top(), 40);
        app.on_key(key(KeyCode::Char('g')));
        assert_eq!(build_log(&app).top(), 0);
        app.on_key(key(KeyCode::Char('G')));
        assert!(build_log(&app).stick);
        app.set_build_log("id0", tail(0..60));
        assert_eq!(build_log(&app).top(), 50);
        // A shorter body (rotated by a new build) clamps a kept spot.
        app.on_key(key(KeyCode::Char('g')));
        app.on_key(key(KeyCode::PageDown));
        assert_eq!(build_log(&app).top(), 20);
        app.set_build_log("id0", tail(0..5));
        assert_eq!(build_log(&app).top(), 4);
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
            done: false,
            phase: None,
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
