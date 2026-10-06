//! Inbox tab: this dashboard's view of the shared store (`crate::inbox`,
//! `inbox.json`). The content comes from the store — loaded at startup,
//! reloaded whenever the file changes, since any dashboard or CLI may write
//! it — and everything here is view state: the current view, the selection,
//! the focused zone, all unit-testable without touching the file.
//!
//! Mutations (`d`, `D`, mark-read, and the pane's actions, replies, done and
//! reopen) are only *requested* here, as [`crate::inbox::Op`]s the event loop
//! applies through `inbox::ops::apply` before reloading; the local copy is
//! updated at once so the next frame already shows the result, except for
//! ops that enqueue an event: their ids and times are minted under the store
//! lock, and the reload in the same loop pass shows them. Opening a link is
//! requested the same way (`pending_open`).
//!
//! The list is one row per thread, newest change first, filtered by a
//! [`View`] (docs/inbox-threads.md, *Inbox UI* and *Inbox layout v2*). The
//! thread pane beside it always shows the selected thread: a pinned header
//! ([`pane_header`]), the thread's open forms pinned under it (`forms.rs`),
//! then its feed, newest first ([`pane_feed`]). Keys go to one of four
//! [`InboxFocus`] zones; all but the list shadow the dashboard keys.
//!
//! Read semantics: a thread is read when it becomes the selected one (it's on
//! screen in the pane), and again whenever it changes while selected. Reading
//! a notify record drops it from Needs you, so the selected thread stays
//! listed until the cursor leaves it ([`InboxView::shown`]); otherwise the
//! next row would slide under the cursor, get read, and so on down the list.
//! Notify records are also read when the user leaves the Inbox after it
//! showed them: a notify record has no state to resolve it, so without that
//! every unkeyed `notify` would sit in Needs you until selected one by one.

use std::cell::Cell;
use std::cmp::Reverse;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use crate::devsbd::notify::Level;
use crate::inbox::{feed, is_url, Inbox, ItemKind, Kind, Marker, Op, State, Thread};
use crate::tui::textarea::TextArea;

use super::forms::{self, FormEdit};
use super::view::{divider_pct, point_in};
use super::{App, Modal, Tab};
use crate::tui::ui;

/// Which threads the list shows, stepped with `←`/`→`. The membership rule
/// ([`View::shows`]) lives in `crate::inbox::view`, shared with the daemon
/// API; this adds the dashboard's display bits.
pub use crate::inbox::View;

impl View {
    pub fn title(self) -> &'static str {
        match self {
            View::NeedsYou => "Needs you",
            View::Active => "Active",
            View::Done => "Done",
            View::All => "All",
        }
    }

    /// The view `step` places over (negative: left), clamped at the ends:
    /// the views read as a strip, and wrapping would jump from All back to
    /// Needs you on a held arrow key.
    fn step(self, step: isize) -> View {
        let i = View::ALL.iter().position(|v| *v == self).unwrap_or(0) as isize;
        View::ALL[(i + step).clamp(0, View::ALL.len() as isize - 1) as usize]
    }

    /// Whether this view lists unread notify records, i.e. leaving the Inbox
    /// from it means the user has seen them.
    fn shows_notify(self) -> bool {
        matches!(self, View::NeedsYou | View::All)
    }
}

/// Where Inbox keys go. The list is the dashboard's own zone (its keys are
/// the normal Inbox arms); the thread and the input shadow the dashboard, so
/// `1`-`9` and the letter keys mean the thread's actions and text there.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InboxFocus {
    #[default]
    List,
    Thread,
    /// The thread's pinned open forms (`forms.rs`); always paired with
    /// [`InboxView::form`].
    Form,
    /// The reply input at the bottom of the thread pane; always paired with
    /// [`InboxView::reply`].
    Input,
}

/// The list's default share of the Inbox width, in percent.
const DEFAULT_SPLIT_PCT: u16 = 40;

/// The loaded Inbox plus this dashboard's view state, which is never
/// persisted: it keys off thread ids, which outlive a reload.
pub struct InboxView {
    content: Inbox,
    pub view: View,
    pub focus: InboxFocus,
    /// The list's share of the width, in percent (the thread pane gets the
    /// rest). Not saved.
    pub split_pct: u16,
    /// True while the list/thread divider is being dragged with the mouse.
    /// Separate from the config modal's drag flag so a drag in flight on one
    /// can't carry over to the other when the modal opens or closes.
    pub(super) dragging: bool,
    /// Id of the thread the pane shows, i.e. the selected one, as of the last
    /// [`App::sync_inbox_selection`]. The pane's scroll and reply box belong
    /// to it, and it stays listed while shown even once it leaves the view
    /// (see the module doc). `None` off the Inbox tab.
    shown: Option<u64>,
    /// First pane row shown.
    pub scroll: u16,
    /// Largest useful `scroll`, written by the renderer, which alone knows the
    /// pane's size and how its lines wrap. A `Cell` because drawing borrows
    /// the app immutably; it only bounds the scroll keys, so a stale value
    /// costs at most a frame of overscroll.
    pane_max: Cell<u16>,
    /// First list card shown, written by the renderer (which alone knows the
    /// list's height) and read back next frame, so the list only scrolls when
    /// the selection would leave it instead of re-centering on every move.
    list_offset: Cell<usize>,
    /// The reply input's line, while the input has focus.
    pub reply: Option<ReplyBox>,
    /// The form cursor, while the form zone has focus.
    pub form: Option<FormEdit>,
    /// First row of the pinned forms box shown, and its largest useful
    /// value: renderer hooks like [`Self::pane_max`].
    forms_scroll: Cell<usize>,
    forms_max: Cell<usize>,
    /// The form cursor moved: the next frame scrolls the forms box to it
    /// (`forms::forms_offset`); the wheel clears it.
    pub(super) forms_follow: Cell<bool>,
    /// `m`: show every thread's text as its markdown source instead of
    /// rendered. For this session only: it's for the odd message the
    /// renderer gets wrong, not a preference.
    pub raw: bool,
}

impl Default for InboxView {
    fn default() -> Self {
        Self {
            content: Inbox::default(),
            view: View::default(),
            focus: InboxFocus::default(),
            split_pct: DEFAULT_SPLIT_PCT,
            dragging: false,
            shown: None,
            scroll: 0,
            pane_max: Cell::new(0),
            list_offset: Cell::new(0),
            reply: None,
            form: None,
            forms_scroll: Cell::new(0),
            forms_max: Cell::new(0),
            forms_follow: Cell::new(false),
            raw: false,
        }
    }
}

/// The reply input at the bottom of the thread pane: a wrapping
/// [`TextArea`] (the `:` prompt's editing over text that may hold newlines)
/// that grows with what's typed.
pub struct ReplyBox {
    /// Id of the thread being replied to.
    pub thread: u64,
    pub line: TextArea,
}

impl InboxView {
    pub fn threads(&self) -> &[Thread] {
        &self.content.threads
    }

    /// The tab-title badge: threads waiting on the user.
    pub fn needs_you(&self) -> usize {
        self.content.needs_you()
    }

    /// The Instances-row badge for owner id `owner`.
    pub fn needs_you_for(&self, owner: &str) -> usize {
        self.content.needs_you_for(owner)
    }

    /// Threads in `view`, for the view header.
    pub fn count(&self, view: View) -> usize {
        self.threads().iter().filter(|t| view.shows(t)).count()
    }

    /// The visible rows: indices into [`Self::threads`] in the current view
    /// (plus the shown thread, see [`Self::shown`]), last change first. The
    /// sort is stable, so ties keep the store's order (newest arrival first).
    /// Cheap, recomputed on demand.
    pub fn rows(&self) -> Vec<usize> {
        let threads = self.threads();
        let listed = |t: &Thread| self.view.shows(t) || Some(t.id) == self.shown;
        let mut rows: Vec<usize> = (0..threads.len()).filter(|&i| listed(&threads[i])).collect();
        rows.sort_by_key(|&i| Reverse(threads[i].changed_at()));
        rows
    }

    /// Renderer hook: the pane's largest scroll offset at its current size.
    pub fn set_pane_max(&self, max: u16) {
        self.pane_max.set(max);
    }

    /// Scroll the pane by `delta` rows within the rendered bound; the new
    /// scroll.
    pub(super) fn scroll_pane(&mut self, delta: i16) -> u16 {
        let max = self.pane_max.get();
        self.scroll = (self.scroll.min(max) as i32 + delta as i32).clamp(0, max as i32) as u16;
        self.scroll
    }

    /// Renderer hook: the list's first visible card, as of the last frame.
    pub fn list_offset(&self) -> usize {
        self.list_offset.get()
    }

    pub fn set_list_offset(&self, offset: usize) {
        self.list_offset.set(offset);
    }

    /// Renderer hook: the forms box's first row as last drawn.
    pub fn forms_scroll(&self) -> usize {
        self.forms_scroll.get()
    }

    /// Renderer hook: whether to scroll the forms box to the cursor, once.
    pub fn take_forms_follow(&self) -> bool {
        self.forms_follow.replace(false)
    }

    /// Renderer hook: the forms box's scroll and its bound, as drawn.
    pub fn set_forms_scroll(&self, scroll: usize, max: usize) {
        self.forms_scroll.set(scroll);
        self.forms_max.set(max);
    }

    #[cfg(test)]
    pub(super) fn content_clone(&self) -> Inbox {
        self.content.clone()
    }
}

/// A thread's one-line title: a dispatcher thread's `title`, or a notify
/// thread's newest message, first line only.
pub fn title_of(t: &Thread) -> String {
    match t.kind {
        Kind::Thread => t.title.clone(),
        Kind::Notify => t.head().map_or("", |r| r.msg.lines().next().unwrap_or("")).to_string(),
    }
}

/// The resolved `child` of a thread, for the pane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChildInfo {
    pub key: String,
    /// Instance name, `None` when the key resolves to none of the owner's
    /// children (removed, renamed away from `<sandbox>-<key>`, or ambiguous).
    pub name: Option<String>,
    /// The container's run state from the latest snapshot.
    pub status: Option<String>,
    /// The container exists in the snapshot but isn't up (exited or gone),
    /// so the header says `(stopped)`; false while its state is unknown.
    pub stopped: bool,
}

/// How a pane segment is drawn; the renderer maps these to styles, so the
/// content stays plain data (the source text) that tests can read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Plain,
    Dim,
    Bold,
    Link,
    State(State),
    Level(Level),
    /// Inline markdown (code, emphasis, links as text) on a plain base:
    /// statuses and timeline rows.
    Text,
    /// Inline markdown on a bold base: the title.
    Title,
    /// A whole markdown document (the message, a notify body), rendered as
    /// blocks; always alone on its line.
    Markdown,
    /// The form option under the cursor: highlighted, plain text.
    Cursor,
}

/// One pane line: styled segments, unwrapped.
pub type PaneLine = Vec<(Tone, String)>;

fn line(tone: Tone, text: impl Into<String>) -> PaneLine {
    vec![(tone, text.into())]
}

/// A `fields` block as `label  value` rows, labels padded to the widest one
/// (display columns, so wide glyphs line up), dim labels over plain values.
fn field_rows(items: &[feed::Field]) -> Vec<PaneLine> {
    let cols = |s: &str| ratatui::text::Span::raw(s).width();
    let width = items.iter().map(|f| cols(&f.label)).max().unwrap_or(0);
    items
        .iter()
        .map(|f| {
            let pad = " ".repeat(width - cols(&f.label) + 2);
            vec![(Tone::Dim, format!("{}{pad}", f.label)), (Tone::Plain, f.value.clone())]
        })
        .collect()
}

/// A thread's state chip, the one source of its text for the card and the
/// pane head: a dispatcher thread's state marker with its status (inline
/// markdown) or, without one, the state's name; a notify thread's level.
pub fn chip(t: &Thread) -> PaneLine {
    match t.kind {
        Kind::Thread => {
            let state = t.state.unwrap_or(State::Active);
            let (marker, name) = match state {
                State::NeedsYou => ("●", "needs you"),
                State::Active => ("○", "active"),
                State::Done => ("✓", "done"),
            };
            match t.status.as_deref().filter(|s| !s.trim().is_empty()) {
                Some(status) => vec![(Tone::State(state), format!("{marker} ")), (Tone::Text, status.to_string())],
                None => line(Tone::State(state), format!("{marker} {name}")),
            }
        }
        Kind::Notify => {
            let level = t.head().map_or(Level::Info, |r| r.level);
            let marker = match level {
                Level::Error => "✖",
                Level::Warn => "▲",
                Level::Info => "·",
            };
            line(Tone::Level(level), format!("{marker} {}", level.as_str()))
        }
    }
}

/// A multi-line value folded onto one timeline row.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A state as the chip names it.
fn state_name(state: State) -> &'static str {
    match state {
        State::NeedsYou => "needs you",
        State::Active => "active",
        State::Done => "done",
    }
}

/// A marker's one line, after its `·`.
fn marker_text(m: &Marker) -> String {
    match m {
        Marker::Done { .. } => "done".into(),
        Marker::Reopen { .. } => "reopened".into(),
        Marker::State { from, to } => format!("{} → {}", state_name(*from), state_name(*to)),
        Marker::Status { from: None, to: Some(to) } => format!("status: {}", one_line(to)),
        Marker::Status { from, to } => format!(
            "status: {} → {}",
            from.as_deref().map_or("–".into(), one_line),
            to.as_deref().map_or("–".into(), one_line)
        ),
    }
}

/// Blank lines between feed entries: around a block (a message, a reply, a
/// notify record), not between one-row entries (actions, markers).
fn push_entry(out: &mut Vec<PaneLine>, after_block: &mut bool, block: bool, lines: Vec<PaneLine>) {
    if !out.is_empty() && (block || *after_block) {
        out.push(Vec::new());
    }
    *after_block = block;
    out.extend(lines);
}

/// An owner thread's feed as pane lines, **newest first** (scroll 0 is the
/// latest thing). A message or a reply is a dim stamp row (author, and a
/// message's `(edited)`/`(withdrawn)` tag) over its full markdown, set off by
/// blank lines; a withdrawn message keeps only its row. Actions and markers
/// are one row each, run together.
fn feed_lines(t: &Thread, utc_offset: i64) -> Vec<PaneLine> {
    let mut out = Vec::new();
    let mut after_block = false;
    for item in t.feed.iter().rev() {
        let at = stamp(item.at, utc_offset);
        let block = matches!(item.kind, ItemKind::Message { .. } | ItemKind::Reply { .. });
        let mut lines = Vec::new();
        match &item.kind {
            ItemKind::Message { blocks, edited, withdrawn, form, .. } => {
                let tag = match (edited, withdrawn) {
                    (_, true) => "  (withdrawn)",
                    (true, false) => "  (edited)",
                    _ => "",
                };
                lines.push(line(Tone::Dim, format!("{at}  {}{tag}", t.owner_name)));
                if !withdrawn {
                    for block in blocks {
                        match block {
                            feed::Block::Markdown { text } => lines.push(line(Tone::Markdown, text.clone())),
                            feed::Block::Fields { items } => lines.extend(field_rows(items)),
                            feed::Block::Form(f) => lines.extend(forms::feed_form_lines(f, form.as_ref(), utc_offset)),
                        }
                    }
                    // A re-send that dropped the form withdrew it.
                    let has_form = blocks.iter().any(|b| matches!(b, feed::Block::Form(_)));
                    if !has_form && form.as_ref().is_some_and(|r| r.state == crate::inbox::FormState::Withdrawn) {
                        lines.push(line(Tone::Dim, "form withdrawn"));
                    }
                }
            }
            ItemKind::Reply { text, .. } => {
                lines.push(line(Tone::Dim, format!("{at}  you")));
                lines.push(line(Tone::Markdown, text.clone()));
            }
            ItemKind::Action { label, .. } => {
                lines.push(vec![(Tone::Dim, format!("{at}  ")), (Tone::Text, format!("you: {label}"))]);
            }
            // Folded: the answers show under the message's form.
            ItemKind::Submission { answers, .. } => {
                let n = forms::answer_count(answers);
                let s = if n == 1 { "" } else { "s" };
                lines.push(line(Tone::Dim, format!("{at}  you answered {n} question{s}")));
            }
            ItemKind::Marker(m) => lines.push(line(Tone::Dim, format!("{at}  · {}", marker_text(m)))),
        }
        push_entry(&mut out, &mut after_block, block, lines);
    }
    out
}

/// A notify thread's records as pane lines, newest (the head) first: each a
/// dim stamp row with its level over its full markdown.
fn note_lines(t: &Thread, utc_offset: i64) -> Vec<PaneLine> {
    let mut out = Vec::new();
    let mut after_block = false;
    for n in &t.notes {
        let r = &n.record;
        let lines = vec![
            vec![(Tone::Dim, format!("{}  ", stamp(r.at, utc_offset))), (Tone::Level(r.level), r.level.as_str().to_string())],
            line(Tone::Markdown, r.msg.as_str()),
        ];
        push_entry(&mut out, &mut after_block, true, lines);
    }
    out
}

/// "2 events waiting for web": what the owner hasn't pulled yet.
fn pending_line(t: &Thread) -> Option<String> {
    match t.events.len() {
        0 => None,
        1 => Some(format!("1 event waiting for {}", t.owner_name)),
        n => Some(format!("{n} events waiting for {}", t.owner_name)),
    }
}

/// One row of the thread pane's pinned header ([`pane_header`]). Width-free:
/// the renderer cuts and packs it to the pane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeaderRow {
    /// One screen row: `left` cut with `…` to the width, `right` (dim)
    /// right-aligned while there's room for both.
    Line { left: PaneLine, right: Option<String> },
    /// Buttons (`[1] Retry`, `[o] VS Code`), packed into as many rows as the
    /// width needs, wrapped only between buttons.
    Buttons(Vec<PaneLine>),
}

/// The thread-target keys ([`App::on_inbox_thread_key`]) the action row
/// offers next to the owner's actions, with their button labels.
const TARGET_KEYS: [(char, &str); 4] = [('o', "VS Code"), ('t', "Terminal"), ('l', "Logs"), ('p', "Port")];

/// The thread pane's pinned header for `t`: the title (`↗` with a link), the
/// chip and stamp with `owner · key` on the right, the child (with
/// `(stopped)`), `(archived: instance removed)`, then the action row: the
/// numbered owner actions (`⌂` runs on the host, `✓` marks done; the rest is
/// an event for the owner, which the status line says on press) and the
/// target keys. Those are offered on a live dispatcher thread whose target
/// resolves (not on an unresolved child); a notify thread's header is its
/// title and level chip. Pure, so what the header says is unit-testable.
pub fn pane_header(t: &Thread, child: Option<&ChildInfo>, utc_offset: i64) -> Vec<HeaderRow> {
    let mut title = line(Tone::Title, title_of(t));
    if t.link.is_some() || t.head().is_some_and(|r| r.link.is_some()) {
        title.push((Tone::Link, " ↗".into()));
    }
    let mut rows = vec![HeaderRow::Line { left: title, right: None }];
    let mut head = chip(t);
    head.push((Tone::Dim, format!("  ·  {}", stamp(t.changed_at(), utc_offset))));
    let from = match &t.key {
        Some(key) => format!("{} · {key}", t.owner_name),
        None => t.owner_name.clone(),
    };
    rows.push(HeaderRow::Line { left: head, right: Some(from) });
    if let Some(c) = child {
        let mut row = vec![(Tone::Dim, "child ".to_string())];
        match &c.name {
            Some(name) => {
                row.push((Tone::Plain, name.clone()));
                let status = match c.status.as_deref() {
                    _ if c.stopped => "(stopped)",
                    Some(status) => status,
                    None => "not in snapshot",
                };
                row.push((Tone::Dim, format!("  {status}")));
            }
            None => row.push((Tone::Dim, format!("{} (no such child)", c.key))),
        }
        rows.push(HeaderRow::Line { left: row, right: None });
    }
    if t.archived {
        rows.push(HeaderRow::Line { left: line(Tone::Dim, "(archived: instance removed)"), right: None });
    }
    let mut buttons: Vec<PaneLine> = t
        .actions
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let mut b = vec![(Tone::Bold, format!("[{}] ", i + 1)), (Tone::Plain, a.label.clone())];
            if a.host.is_some() {
                b.push((Tone::Dim, " ⌂".into()));
            }
            if a.done {
                b.push((Tone::Dim, " ✓".into()));
            }
            b
        })
        .collect();
    let target = t.kind == Kind::Thread && !t.archived && child.is_none_or(|c| c.name.is_some());
    if target {
        buttons.extend(TARGET_KEYS.iter().map(|(k, label)| vec![(Tone::Bold, format!("[{k}] ")), (Tone::Plain, label.to_string())]));
    }
    if !buttons.is_empty() {
        rows.push(HeaderRow::Buttons(buttons));
    }
    rows
}

/// The thread pane's scrolling feed for `t`, under the header: the pending
/// events line first (state, not history), then an owner thread's feed
/// ([`feed_lines`]) or a notify thread's records ([`note_lines`]), newest
/// first. Pure, so what the feed says is unit-testable.
pub fn pane_feed(t: &Thread, utc_offset: i64) -> Vec<PaneLine> {
    let mut out = Vec::new();
    if let Some(pending) = pending_line(t) {
        out.push(line(Tone::Dim, pending));
        out.push(Vec::new());
    }
    out.extend(match t.kind {
        Kind::Thread => feed_lines(t, utc_offset),
        Kind::Notify => note_lines(t, utc_offset),
    });
    // No reply hint here: the pane's composer (or its no-replies line) says it.
    out
}



impl App {
    /// Install Inbox content loaded from the store (startup and every reload),
    /// keeping the cursor on the thread it was on. In the list, a cursor on
    /// the top row stays on top (the newest); one moved down stays on its
    /// thread, wherever that moved to. With the thread or its input focused
    /// the cursor always stays on its thread: a new arrival must not swap the
    /// pane (and drop a half-typed reply) under the user.
    pub fn set_inbox(&mut self, inbox: Inbox) {
        let selected = match (self.selected[Tab::Inbox.index()], self.inbox.focus) {
            (0, InboxFocus::List) => None,
            _ => self.selected_inbox_id(),
        };
        self.inbox.content = inbox;
        self.reselect_inbox(selected);
        // A change to the selected thread reads it again; one that removed it
        // (another dashboard, `thread rm`) moves the pane on.
        self.sync_inbox_selection();
        self.check_form_edit();
    }

    /// Ask the event loop to apply `op` to the store, and apply it here at
    /// once so the next frame shows it (the reload that follows confirms it).
    /// The cursor stays on its thread, or on its row when the thread left
    /// the view (e.g. a dismissed notify record).
    pub(super) fn request_inbox(&mut self, op: Op) {
        let selected = self.selected_inbox_id();
        // An event-enqueuing op is the store's to stamp (see the module doc).
        if !op.enqueues_event() {
            self.inbox.content.apply(&op, 0, "tui");
        }
        self.pending_inbox.push(op);
        self.reselect_inbox(selected);
    }

    /// Take the mutations the event loop owes the store.
    pub fn take_pending_inbox(&mut self) -> Vec<Op> {
        std::mem::take(&mut self.pending_inbox)
    }

    /// Switch tabs. Leaving the Inbox resets it to the list and reads the
    /// notify records it showed (see the module doc); entering it reads the
    /// selected thread, now in the pane. Entering Instances refreshes the
    /// process rows (they aren't fetched elsewhere). A selection goes with
    /// the screen it was on.
    pub(super) fn set_tab(&mut self, tab: Tab) {
        self.clear_selection();
        if self.tab == Tab::Inbox && tab != Tab::Inbox {
            self.close_form_edit();
            self.inbox.focus = InboxFocus::List;
            self.inbox.reply = None;
            let unread_notify = self.inbox.threads().iter().any(|t| t.kind == Kind::Notify && t.unread);
            if self.inbox.view.shows_notify() && unread_notify {
                self.request_inbox(Op::MarkNotifyRead);
            }
            // Unpinned only now: the cursor follows its thread through the
            // read above, then the read records may leave the list.
            let selected = self.selected_inbox_id();
            self.inbox.shown = None;
            self.reselect_inbox(selected);
        }
        if tab == Tab::Instances && self.tab != tab {
            self.needs_proc_fetch = true;
        }
        self.tab = tab;
        self.sync_inbox_selection();
    }

    /// Tab-bar title, carrying the needs-you count on the Inbox.
    pub fn tab_title(&self, tab: Tab) -> String {
        match (tab, self.inbox.needs_you()) {
            (Tab::Inbox, n) if n > 0 => format!("{} ({n})", tab.title()),
            _ => tab.title().to_string(),
        }
    }

    /// The thread under the Inbox cursor, if any: the one the pane shows.
    pub fn selected_inbox_thread(&self) -> Option<&Thread> {
        let i = *self.inbox.rows().get(self.selected[Tab::Inbox.index()])?;
        self.inbox.threads().get(i)
    }

    fn selected_inbox_id(&self) -> Option<u64> {
        self.selected_inbox_thread().map(|t| t.id)
    }

    /// Put the cursor back on thread `id` if it is still visible, else leave
    /// the index and re-clamp.
    fn reselect_inbox(&mut self, id: Option<u64>) {
        if let Some(id) = id {
            let threads = self.inbox.threads();
            if let Some(pos) = self.inbox.rows().iter().position(|&i| threads[i].id == id) {
                self.selected[Tab::Inbox.index()] = pos;
            }
        }
        self.clamp_selection();
    }

    /// Bring the pane in line with the cursor, after anything that may have
    /// moved it (a key, a reload, entering the tab). A newly selected thread
    /// gets a fresh scroll, drops the previous thread's reply box and stays
    /// listed while shown; the selected thread is read if it isn't (newly
    /// selected, or changed while selected). Off the Inbox nothing is on
    /// screen, so nothing is read.
    pub(super) fn sync_inbox_selection(&mut self) {
        if self.tab != Tab::Inbox {
            return;
        }
        let selected = self.selected_inbox_thread().map(|t| (t.id, t.unread));
        let id = selected.map(|(id, _)| id);
        if id != self.inbox.shown {
            // A selection's rows are the old thread's: copying it now would
            // read another thread's text.
            self.clear_selection();
            // Before `shown` moves: a text edit is saved on its own thread.
            self.close_form_edit();
            self.inbox.shown = id;
            self.inbox.scroll = 0;
            self.inbox.set_forms_scroll(0, 0);
            self.inbox.reply = None;
            self.inbox.focus = match (id, self.inbox.focus) {
                (None, _) => InboxFocus::List,
                (Some(_), InboxFocus::Input | InboxFocus::Form) => InboxFocus::Thread,
                (Some(_), focus) => focus,
            };
            // Pinning it may bring back rows the old pin hid; stay on it.
            self.reselect_inbox(id);
        }
        if let Some((id, true)) = selected {
            self.request_inbox(Op::MarkRead(id));
        }
    }

    /// `←`/`→` (Inbox list): the view `step` over, clamped at the ends, the
    /// cursor following its thread when the new view lists it, else starting
    /// at the top: the old row number means nothing in another view.
    pub(super) fn step_inbox_view(&mut self, step: isize) {
        let selected = self.selected_inbox_id();
        // Unpinned, so a thread the new view doesn't list isn't kept in it.
        self.inbox.shown = None;
        self.inbox.view = self.inbox.view.step(step);
        let threads = self.inbox.threads();
        let pos = selected.and_then(|id| self.inbox.rows().iter().position(|&i| threads[i].id == id));
        if pos.is_none() {
            self.inbox.set_list_offset(0);
        }
        self.selected[Tab::Inbox.index()] = pos.unwrap_or(0);
        self.clamp_selection();
    }

    /// `enter` (Inbox list): focus the selected thread's pane.
    pub(super) fn focus_inbox_thread(&mut self) {
        if self.selected_inbox_thread().is_some() {
            self.inbox.focus = InboxFocus::Thread;
        }
    }

    /// `r`/`i` (list or thread): focus the selected thread's input, when it
    /// takes replies.
    pub(super) fn focus_inbox_input(&mut self) {
        if let Some(t) = self.selected_inbox_thread().cloned() {
            self.open_reply(&t);
        }
    }

    /// The selected thread, owned: the action keys borrow `self` mutably.
    pub(super) fn selected_inbox_owned(&self) -> Option<Thread> {
        self.selected_inbox_thread().cloned()
    }

    /// The thread keys both zones share, on the selected thread: `o`/`t`/
    /// `l`/`p` aimed at the thread's target rather than a table row
    /// (docs/inbox-threads.md, *Decisions*), `d`/`u` done and reopen.
    /// Whether `key` was one of them.
    pub(super) fn on_inbox_thread_key(&mut self, key: KeyCode) -> bool {
        if !matches!(key, KeyCode::Char('o' | 't' | 'l' | 'p' | 'd' | 'u')) {
            return false;
        }
        let Some(t) = self.selected_inbox_owned() else { return true };
        match key {
            KeyCode::Char('o') => self.thread_code(&t),
            KeyCode::Char('t') => self.thread_terminal(&t),
            KeyCode::Char('l') => self.thread_logs(&t),
            KeyCode::Char('p') => self.thread_port_prompt(&t),
            KeyCode::Char('d') => self.dismiss_selected_notification(),
            KeyCode::Char('u') if t.kind == Kind::Thread => self.reopen_thread(&t),
            _ => {}
        }
        true
    }

    /// The child of `t`, resolved through the map the event loop refreshes
    /// with each snapshot, plus that instance's run state.
    pub fn thread_child(&self, t: &Thread) -> Option<ChildInfo> {
        let key = t.child.clone()?;
        let name = self.thread_children.get(&t.owner).and_then(|keys| keys.get(&key)).cloned();
        let row = name.as_ref().and_then(|name| {
            let snapshot = self.snapshot.as_ref()?;
            snapshot.instances.iter().find(|r| &r.name == name)
        });
        let status = row.map(|r| r.status.label().to_string());
        let stopped = row.is_some_and(|r| {
            matches!(r.status, crate::snapshot::ContainerStatus::Exited(_) | crate::snapshot::ContainerStatus::Missing)
        });
        Some(ChildInfo { key, name, status, stopped })
    }

    /// Keys while the thread pane or its input has focus. They shadow the
    /// dashboard like a modal does: tab keys, `d` and the rest mean the
    /// thread's actions, not the list's. Quit, the prompt and help stay
    /// reachable.
    pub(super) fn on_key_inbox_pane(&mut self, key: KeyEvent) {
        // The input, when focused, takes every key (like the `:` prompt).
        if self.inbox.focus == InboxFocus::Input {
            self.on_key_reply(key);
            return;
        }
        if self.inbox.focus == InboxFocus::Form {
            self.on_key_form(key);
            return;
        }
        self.status = None;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(t) = self.selected_inbox_owned() else {
            self.inbox.focus = InboxFocus::List;
            return;
        };
        if !ctrl && self.on_inbox_thread_key(key.code) {
            return;
        }
        let link = t.link.clone().or_else(|| t.head().and_then(|r| r.link.clone()));
        match key.code {
            KeyCode::Esc => self.inbox.focus = InboxFocus::List,
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('c') if ctrl => self.should_quit = true,
            KeyCode::Char(':') => self.open_prompt(),
            KeyCode::Char('?') => self.open_help(),
            KeyCode::Enter => match link {
                Some(link) => self.request_open_link(link),
                None => self.status = Some("no link on this thread".into()),
            },
            KeyCode::Char(c @ '1'..='9') => self.run_thread_action(&t, c as usize - '1' as usize),
            KeyCode::Char('r' | 'i') => self.open_reply(&t),
            // The next zone down: the pinned forms, else the composer.
            KeyCode::Tab => {
                if !self.focus_inbox_form(None) {
                    self.open_reply(&t);
                }
            }
            KeyCode::Char('m') if !ctrl => {
                // Raw rows aren't the rendered ones a selection points into.
                self.clear_selection();
                self.inbox.raw = !self.inbox.raw;
            }
            _ => {
                let lines = self.inbox.pane_max.get().saturating_add(1);
                let mut scroll = self.inbox.scroll;
                super::view::scroll_key(&mut scroll, lines, key);
                self.inbox.scroll = scroll;
            }
        }
    }

    /// `d` (Inbox list or thread): dismiss a notify thread (its history with
    /// it). A dispatcher thread is the dispatcher's to drop, so `d` marks it
    /// done.
    pub(super) fn dismiss_selected_notification(&mut self) {
        let Some(t) = self.selected_inbox_owned() else {
            return;
        };
        match t.kind {
            Kind::Notify => self.request_inbox(Op::RemoveThread(t.id)),
            Kind::Thread => self.mark_thread_done(&t),
        }
    }

    /// Why `t` takes no user op, if it doesn't: an archived thread's owner is
    /// gone, so nobody would ever pull the event.
    pub(super) fn refuse_user_op(&mut self, t: &Thread) -> bool {
        if t.archived {
            self.status = Some(format!("archived: `{}` was removed", t.owner_name));
        }
        t.archived
    }

    /// `d` on a dispatcher thread: done, with an event.
    pub(super) fn mark_thread_done(&mut self, t: &Thread) {
        if self.refuse_user_op(t) {
            return;
        }
        if t.state == Some(State::Done) {
            self.status = Some("already done".into());
            return;
        }
        self.request_inbox(Op::MarkDone(t.id));
        let child = self.mark_thread_child(t, true).map(|c| format!(" · {c}")).unwrap_or_default();
        self.status = Some(format!("marked done{child} · event queued for {}", t.owner_name));
    }

    /// `u`: reopen a done thread, with an event; its child's done flag is
    /// cleared too.
    fn reopen_thread(&mut self, t: &Thread) {
        if self.refuse_user_op(t) {
            return;
        }
        if t.state != Some(State::Done) {
            self.status = Some("not done: nothing to reopen".into());
            return;
        }
        self.request_inbox(Op::Reopen(t.id));
        let child = self.mark_thread_child(t, false).map(|c| format!(" · {c}")).unwrap_or_default();
        self.status = Some(format!("reopened{child} · event queued for {}", t.owner_name));
    }

    /// `r`/`i`: focus the input with an empty line, when the thread takes
    /// replies; otherwise focus stays where it is, with a hint.
    pub(super) fn open_reply(&mut self, t: &Thread) {
        if self.refuse_user_op(t) {
            return;
        }
        if t.compose.is_none() {
            self.status = Some("this thread takes no replies".into());
            return;
        }
        self.inbox.reply = Some(ReplyBox { thread: t.id, line: TextArea::new() });
        self.inbox.focus = InboxFocus::Input;
    }

    /// Keys while the input has focus: `esc` goes back to the thread (the
    /// line with it), `enter` sends a non-empty reply and keeps the input,
    /// empty, for the next one (an empty `enter` does nothing); `alt-enter`
    /// (and `shift-enter`, where the terminal reports it) inserts a newline;
    /// the rest edits the text as the `:` prompt does, with `↑`/`↓` and
    /// `home`/`end` on its visual rows.
    fn on_key_reply(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let newline = key.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::SHIFT);
        let Some(rb) = &mut self.inbox.reply else {
            self.inbox.focus = InboxFocus::Thread;
            return;
        };
        let line = &mut rb.line;
        match key.code {
            KeyCode::Esc => self.leave_reply(),
            KeyCode::Char('c') if ctrl => self.leave_reply(),
            KeyCode::Enter if newline => line.newline(),
            KeyCode::Enter => {
                let text = line.input().trim().to_string();
                if text.is_empty() {
                    return;
                }
                line.clear();
                let thread = rb.thread;
                let owner = self.inbox.threads().iter().find(|t| t.id == thread).map(|t| t.owner_name.clone());
                self.request_inbox(Op::Reply { thread, text });
                self.status = Some(format!("reply queued for {}", owner.unwrap_or_default()));
            }
            _ => edit_text(line, key),
        }
    }

    fn leave_reply(&mut self) {
        self.inbox.reply = None;
        self.inbox.focus = InboxFocus::Thread;
    }

    /// `D` (Inbox tab): clear every notify thread; dispatcher threads stay.
    pub(super) fn clear_notifications(&mut self) {
        self.request_inbox(Op::ClearNotify);
    }

    /// Ask the event loop to open `link`. A non-URL link (it comes from inside
    /// the container) only gets a status line.
    pub(super) fn request_open_link(&mut self, link: String) {
        if is_url(&link) {
            self.status = Some(format!("opening {link}"));
            self.pending_open = Some(link);
        } else {
            self.status = Some(format!("not a URL, not opening: {link}"));
        }
    }

    /// Take the pending link for the event loop to open, if any.
    pub fn take_pending_open(&mut self) -> Option<String> {
        self.pending_open.take()
    }

    /// Drag the list/thread divider on a frame of size `area`: a left press
    /// on (or a cell off) the divider, within the split's rows, starts a drag
    /// and every drag event moves it, clamped like the config modal's split;
    /// release ends it. Only on the Inbox tab with no modal or prompt over it,
    /// since then the divider is what's under the mouse. A press off the
    /// divider is a click ([`Self::inbox_click`]); the wheel goes to
    /// [`Self::inbox_wheel`]. The hit-tests reuse `ui`'s layout functions so
    /// they land where things are drawn. Never consumes the event: the
    /// terminal panel still sees it (a click here unfocuses a terminal).
    /// I/O-free.
    pub(super) fn inbox_mouse(&mut self, ev: &MouseEvent, area: Rect) {
        if self.tab != Tab::Inbox || self.prompt.is_some() || !matches!(self.modal, Modal::None) {
            self.inbox.dragging = false;
            return;
        }
        let rect = ui::inbox_split_rect(area, false, !self.terms.is_empty());
        let divider = ui::inbox_divider_col(rect, self.inbox.split_pct);
        let pct = divider_pct(ev.column.saturating_sub(rect.x), rect.width);
        match ev.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                // The two border columns only (list's right, pane's left): a
                // press on the first text column starts a text selection.
                if point_in(rect, ev.column, ev.row) && (divider.saturating_sub(1)..=divider).contains(&ev.column) {
                    self.inbox.dragging = true;
                    self.inbox.split_pct = pct;
                } else {
                    self.inbox_click(ui::inbox_hit(self, area, ev.column, ev.row));
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.inbox.dragging => {
                self.inbox.split_pct = pct;
            }
            MouseEventKind::Up(MouseButton::Left) => self.inbox.dragging = false,
            MouseEventKind::ScrollUp => self.inbox_wheel(ui::inbox_hit(self, area, ev.column, ev.row), true),
            MouseEventKind::ScrollDown => self.inbox_wheel(ui::inbox_hit(self, area, ev.column, ev.row), false),
            _ => {}
        }
    }

    /// A left click on the Inbox: a card selects its thread and focuses the
    /// list (the caller's [`Self::sync_inbox_selection`] reads it), the strip
    /// switches views, the pane focuses the thread, the input box the input.
    /// The keys' own paths, so a click does what the matching key would.
    fn inbox_click(&mut self, hit: Option<ui::InboxHit>) {
        use ui::{InboxHit, StripHit};
        match hit {
            Some(InboxHit::Card(pos)) => {
                self.selected[Tab::Inbox.index()] = pos;
                self.focus_inbox_list();
            }
            Some(InboxHit::Strip(strip)) => {
                let at = |v: View| View::ALL.iter().position(|w| *w == v).unwrap_or(0) as isize;
                let step = match strip {
                    StripHit::Prev => -1,
                    StripHit::Next => 1,
                    StripHit::View(v) => at(v) - at(self.inbox.view),
                };
                if step != 0 {
                    self.step_inbox_view(step);
                }
                self.focus_inbox_list();
            }
            // Out of the input like `esc`, its line with it: the box is only
            // drawn live while focused.
            Some(InboxHit::Thread | InboxHit::Hint) if self.inbox.focus == InboxFocus::Input => self.leave_reply(),
            Some(InboxHit::Thread | InboxHit::Hint) => {
                self.close_form_edit();
                self.focus_inbox_thread();
            }
            // Already in it: keep the half-typed line `open_reply` would clear.
            Some(InboxHit::Input) if self.inbox.focus == InboxFocus::Input => {}
            Some(InboxHit::Input) => {
                self.close_form_edit();
                self.focus_inbox_input();
            }
            // The composer box while a text question is edited in it.
            Some(InboxHit::Form(None)) if self.inbox.focus == InboxFocus::Form => {}
            Some(InboxHit::Form(spot)) => {
                if self.inbox.focus == InboxFocus::Input {
                    self.leave_reply();
                }
                self.focus_inbox_form(spot);
            }
            Some(InboxHit::ListBlank) | None => {}
        }
    }

    /// One wheel notch on the Inbox: over the list it moves the selection a
    /// card, over the thread pane it scrolls the pane like its scroll keys.
    fn inbox_wheel(&mut self, hit: Option<ui::InboxHit>, up: bool) {
        use ui::InboxHit;
        match hit {
            Some(InboxHit::Card(_) | InboxHit::ListBlank) => {
                if up {
                    self.select_up();
                } else {
                    self.select_down();
                }
            }
            Some(InboxHit::Thread) => {
                let lines = INBOX_WHEEL_LINES as i16;
                self.inbox.scroll_pane(if up { -lines } else { lines });
            }
            Some(InboxHit::Form(_)) => {
                let (scroll, max) = (self.inbox.forms_scroll.get(), self.inbox.forms_max.get());
                let lines = INBOX_WHEEL_LINES as usize;
                let scroll = if up { scroll.saturating_sub(lines) } else { (scroll + lines).min(max) };
                self.inbox.set_forms_scroll(scroll, max);
                self.inbox.forms_follow.set(false);
            }
            _ => {}
        }
    }

    /// Focus the list, dropping any reply box with the input's focus (and
    /// the form cursor, a text edit saved).
    fn focus_inbox_list(&mut self) {
        self.close_form_edit();
        self.inbox.focus = InboxFocus::List;
        self.inbox.reply = None;
    }
}

/// The editing keys a [`TextArea`] takes in the reply input and a form's
/// text edit: the `:` prompt's, with `↑`/`↓` and `home`/`end` on its visual
/// rows.
pub(super) fn edit_text(line: &mut TextArea, key: KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Left => line.left(),
        KeyCode::Right => line.right(),
        KeyCode::Up => line.up(),
        KeyCode::Down => line.down(),
        KeyCode::Home => line.home(),
        KeyCode::End => line.end(),
        KeyCode::Backspace => line.backspace(),
        KeyCode::Delete => line.delete(),
        KeyCode::Char('u') if ctrl => line.clear(),
        KeyCode::Char('w') if ctrl => line.delete_word(),
        KeyCode::Char(c) if !ctrl => line.insert_char(c),
        _ => {}
    }
}

/// Thread-pane lines one wheel notch scrolls.
const INBOX_WHEEL_LINES: u16 = 3;

/// Past this age a card shows a date instead of a relative age.
const RELATIVE_FOR: u64 = 7 * 86_400;

/// A card's age, as short as it gets (`now`, `5m`, `3h`, `2d`), then the
/// date alone (`Oct 12`) once it's a week old: a card's corner has no room
/// for words, and the pane shows the full [`stamp`]. A timestamp
/// ahead of `now` (clock skew) reads `now`.
pub fn short_age(at: u64, now: u64, utc_offset: i64) -> String {
    match now.saturating_sub(at) {
        a if a < 60 => "now".into(),
        a if a < 3600 => format!("{}m", a / 60),
        a if a < 86_400 => format!("{}h", a / 3600),
        a if a < RELATIVE_FOR => format!("{}d", a / 86_400),
        _ => {
            let (month, day) = month_day((at as i64 + utc_offset).div_euclid(86_400));
            format!("{} {day}", MONTHS[month as usize - 1])
        }
    }
}

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// `Oct 2 14:32` of unix time `at`, shifted by `utc_offset` seconds (no year:
/// the Inbox only holds recent notifications).
pub fn stamp(at: u64, utc_offset: i64) -> String {
    let local = at as i64 + utc_offset;
    let (days, secs) = (local.div_euclid(86_400), local.rem_euclid(86_400));
    let (month, day) = month_day(days);
    format!("{} {day} {:02}:{:02}", MONTHS[month as usize - 1], secs / 3600, secs % 3600 / 60)
}

/// (month 1-12, day 1-31) of `days` since 1970-01-01 in the proleptic
/// Gregorian calendar (Howard Hinnant's `civil_from_days`; std has no dates).
fn month_day(days: i64) -> (i64, i64) {
    let z = days + 719_468;
    let doe = z - z.div_euclid(146_097) * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    (if mp < 10 { mp + 3 } else { mp - 9 }, day)
}

/// Parse `date +%z` output (`+0200`, `-0530`) into seconds east of UTC.
pub fn parse_utc_offset(s: &str) -> Option<i64> {
    let s = s.trim();
    let (sign, digits) = match s.as_bytes().first()? {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => return None,
    };
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let hours: i64 = digits[..2].parse().ok()?;
    let minutes: i64 = digits[2..].parse().ok()?;
    Some(sign * (hours * 3600 + minutes * 60))
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use crate::devsbd::notify::Record;
    use crate::inbox::{Action, Compose, ThreadPut};
    use crate::snapshot::ContainerStatus;

    #[test]
    fn fields_render_as_aligned_rows() {
        let f = |label: &str, value: &str| feed::Field { label: label.into(), value: value.into() };
        let rows = field_rows(&[f("Head", "36b13d41"), f("CI", "green"), f("界面", "wide")]);
        let text: Vec<String> = rows.iter().map(|r| r.iter().map(|(_, s)| s.as_str()).collect()).collect();
        // Values start in one column; the CJK label counts as 4 cells.
        assert_eq!(text, ["Head  36b13d41", "CI    green", "界面  wide"]);
        assert_eq!(rows[0], vec![(Tone::Dim, "Head  ".to_string()), (Tone::Plain, "36b13d41".to_string())]);
    }

    fn rec(msg: &str, key: Option<&str>, link: Option<&str>) -> Record {
        Record {
            level: Level::Info,
            key: key.map(str::to_string),
            link: link.map(str::to_string),
            msg: msg.into(),
            at: 0,
        }
    }

    /// What the event loop does on a record from instance `name` (owner id
    /// `<name>-id`): the store gets the push, the dashboard reloads.
    fn push(app: &mut App, name: &str, record: Record) {
        let mut inbox = app.inbox.content.clone();
        inbox.push(format!("{name}-id"), name.into(), record, true);
        app.set_inbox(inbox);
    }

    fn body(key: &str, state: State) -> ThreadPut {
        ThreadPut { key: key.into(), title: key.into(), state, ..ThreadPut::default() }
    }

    /// The same for a `thread put` from `name` at time `at`.
    fn put(app: &mut App, name: &str, at: u64, put: ThreadPut) {
        let mut inbox = app.inbox.content.clone();
        inbox.put(&format!("{name}-id"), name, at, put);
        app.set_inbox(inbox);
    }

    /// Visible rows by title, in order.
    fn rows(app: &App) -> Vec<String> {
        app.inbox.rows().iter().map(|&i| title_of(&app.inbox.threads()[i])).collect()
    }

    fn selected(app: &App) -> Option<String> {
        app.selected_inbox_thread().map(title_of)
    }

    fn thread<'a>(app: &'a App, title: &str) -> &'a Thread {
        app.inbox.threads().iter().find(|t| title_of(t) == title).unwrap()
    }

    fn inbox_tab(app: &mut App) {
        app.on_key(key(KeyCode::Char('4')));
    }

    /// One of each kind of thread, at increasing times (so `old` is oldest).
    fn mixed(app: &mut App) {
        put(app, "d", 10, body("asks", State::NeedsYou));
        put(app, "d", 20, body("busy", State::Active));
        put(app, "d", 30, body("over", State::Done));
        push(app, "a", Record { at: 40, ..rec("unread note", None, None) });
        push(app, "a", Record { at: 50, ..rec("read note", None, None) });
        let id = thread(app, "read note").id;
        let mut inbox = app.inbox.content.clone();
        inbox.apply(&Op::MarkRead(id), 0, "tui");
        app.set_inbox(inbox);
    }

    #[test]
    fn view_membership() {
        let mut app = new_app();
        mixed(&mut app);
        put(&mut app, "gone", 60, body("orphan", State::NeedsYou));
        let mut inbox = app.inbox.content.clone();
        inbox.archive_owner("gone-id");
        app.set_inbox(inbox);

        let titles = |view: View| -> Vec<String> {
            let mut t: Vec<String> =
                app.inbox.threads().iter().filter(|t| view.shows(t)).map(title_of).collect();
            t.sort();
            t
        };
        // Unread notify joins needs-you; the read note and the archived
        // needs-you thread only show in All.
        assert_eq!(titles(View::NeedsYou), ["asks", "unread note"]);
        assert_eq!(titles(View::Active), ["busy"]);
        assert_eq!(titles(View::Done), ["over"]);
        assert_eq!(titles(View::All).len(), 6);
        assert_eq!(app.inbox.count(View::NeedsYou), 2);
        assert_eq!(app.inbox.count(View::All), 6);
    }

    #[test]
    fn rows_sort_by_last_change() {
        let mut app = new_app();
        mixed(&mut app);
        app.inbox.view = View::All;
        assert_eq!(rows(&app), ["read note", "unread note", "over", "busy", "asks"]);
        // A thread that changes moves to the top, wherever it was.
        put(&mut app, "d", 70, ThreadPut { status: Some("ci".into()), ..body("asks", State::NeedsYou) });
        assert_eq!(rows(&app)[0], "asks");
    }

    #[test]
    fn badges_count_needs_you_and_unread_notify() {
        let mut app = new_app();
        mixed(&mut app);
        assert_eq!(app.tab_title(Tab::Inbox), "Inbox (2)");
        assert_eq!(app.tab_title(Tab::Ports), "Ports");
        assert_eq!(app.inbox.needs_you_for("d-id"), 1);
        assert_eq!(app.inbox.needs_you_for("a-id"), 1);
        assert_eq!(app.inbox.needs_you_for("x-id"), 0);
        // Noise that's merely unread doesn't count: an active thread changing.
        put(&mut app, "d", 80, ThreadPut { status: Some("new".into()), ..body("busy", State::Active) });
        assert_eq!(app.inbox.needs_you(), 2);
    }

    #[test]
    fn arrows_step_views_clamped_and_selection_follows_the_thread() {
        let mut app = new_app();
        mixed(&mut app);
        inbox_tab(&mut app);
        assert_eq!(app.inbox.view, View::NeedsYou);
        assert_eq!(rows(&app), ["unread note", "asks"]);
        app.on_key(key(KeyCode::Left));
        assert_eq!(app.inbox.view, View::NeedsYou, "clamped at the left end");
        for want in [View::Active, View::Done, View::All] {
            app.on_key(key(KeyCode::Right));
            assert_eq!(app.inbox.view, want);
        }
        app.on_key(key(KeyCode::Right));
        assert_eq!(app.inbox.view, View::All, "clamped at the right end");
        for _ in 0..4 {
            app.on_key(key(KeyCode::Down));
        }
        assert_eq!(selected(&app).as_deref(), Some("asks"));
        assert_eq!(app.selected(), 4);
        app.on_key(key(KeyCode::Char('v')));
        assert_eq!(app.inbox.view, View::All, "`v` no longer switches views");
        // Into a view without it: the top row.
        app.on_key(key(KeyCode::Left)); // Done: one row
        assert_eq!(app.selected(), 0);
        assert_eq!(selected(&app).as_deref(), Some("over"));
        // Still visible in the next view: the cursor follows it.
        for _ in 0..2 {
            app.on_key(key(KeyCode::Right));
        }
        assert_eq!(app.inbox.view, View::All);
        assert_eq!(selected(&app).as_deref(), Some("over"));
        assert_eq!(app.selected(), 2);
    }

    #[test]
    fn reload_keeps_selection_by_id_and_clamps() {
        let mut app = new_app();
        mixed(&mut app);
        inbox_tab(&mut app);
        app.on_key(key(KeyCode::Right));
        app.on_key(key(KeyCode::Right));
        // Done's only row, "over", is followed into All.
        app.on_key(key(KeyCode::Right)); // All
        assert_eq!(selected(&app).as_deref(), Some("over"));
        assert_eq!(app.selected(), 2);

        // Another writer adds a newer thread above it: the cursor follows.
        put(&mut app, "d", 90, body("newer", State::Active));
        assert_eq!(selected(&app).as_deref(), Some("over"));
        assert_eq!(app.selected(), 3);

        // ... and removes it: the index stays, clamped into the list.
        let mut inbox = app.inbox.content.clone();
        let ids: Vec<u64> = inbox.threads.iter().map(|t| t.id).collect();
        for id in &ids[1..] {
            inbox.apply(&Op::RemoveThread(*id), 0, "tui");
        }
        app.set_inbox(inbox);
        assert_eq!(app.selected(), 0);
        assert_eq!(rows(&app).len(), 1);
    }

    /// A new arrival on top moves a top-row cursor in the list, but never
    /// one whose thread has focus.
    #[test]
    fn a_focused_thread_keeps_the_cursor_through_new_arrivals() {
        let mut app = new_app();
        put(&mut app, "d", 10, ThreadPut { compose: Some(Compose::default()), ..body("asks", State::NeedsYou) });
        inbox_tab(&mut app);
        app.on_key(key(KeyCode::Char('r')));
        typed(&mut app, "half");
        put(&mut app, "d", 20, body("newer", State::NeedsYou));
        assert_eq!(selected(&app).as_deref(), Some("asks"));
        assert_eq!(app.inbox.focus, InboxFocus::Input);
        assert_eq!(app.inbox.reply.as_ref().unwrap().line.input(), "half");

        app.on_key(key(KeyCode::Esc));
        app.on_key(key(KeyCode::Esc));
        put(&mut app, "d", 30, body("newest", State::NeedsYou));
        assert_eq!(selected(&app).as_deref(), Some("asks"), "moved down, so it stays");
        app.on_key(key(KeyCode::Up));
        app.on_key(key(KeyCode::Up));
        put(&mut app, "d", 40, body("latest", State::NeedsYou));
        assert_eq!(selected(&app).as_deref(), Some("latest"), "the top row stays on top");
    }

    #[test]
    fn enter_and_esc_step_between_the_zones() {
        let mut app = new_app();
        mixed(&mut app);
        inbox_tab(&mut app);
        assert_eq!(app.inbox.focus, InboxFocus::List);
        app.on_key(key(KeyCode::Down)); // "asks"
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.inbox.focus, InboxFocus::Thread);
        assert_eq!(selected(&app).as_deref(), Some("asks"));
        assert_eq!(app.take_pending_open(), None, "enter on the list doesn't open the link");
        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.inbox.focus, InboxFocus::List);
        assert_eq!(app.tab, Tab::Inbox);

        // Nothing selected: enter stays in the list.
        app.on_key(key(KeyCode::Right)); // Active
        app.on_key(key(KeyCode::Right)); // Done
        let mut inbox = app.inbox.content.clone();
        inbox.apply(&Op::RemoveThread(thread(&app, "over").id), 0, "tui");
        app.set_inbox(inbox);
        assert_eq!(selected(&app), None);
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.inbox.focus, InboxFocus::List);
    }

    #[test]
    fn number_keys_switch_tabs_in_the_list_and_run_actions_in_the_thread() {
        let mut app = new_app();
        let actions = vec![Action { id: "post".into(), label: "Post replies".into(), ..Action::default() }];
        put(&mut app, "d", 10, ThreadPut { actions, ..body("asks", State::NeedsYou) });
        let id = thread(&app, "asks").id;
        inbox_tab(&mut app);
        app.take_pending_inbox();
        app.on_key(key(KeyCode::Char('1')));
        assert_eq!(app.tab, Tab::Instances);
        assert_eq!(app.take_pending_inbox(), []);

        inbox_tab(&mut app);
        app.on_key(key(KeyCode::Enter));
        app.on_key(key(KeyCode::Char('1')));
        assert_eq!(app.tab, Tab::Inbox);
        assert_eq!(app.take_pending_inbox(), [Op::Act { thread: id, action: "post".into() }]);
    }

    /// While the thread has focus, dashboard keys don't reach the rows or
    /// tabs underneath.
    #[test]
    fn the_thread_shadows_dashboard_keys() {
        let mut app = new_app();
        push(&mut app, "a", rec("note", None, Some("https://x/1")));
        put(
            &mut app,
            "d",
            10,
            ThreadPut {
                link: Some("https://x/pr/1".into()),
                actions: vec![Action { id: "post".into(), label: "Post replies".into(), ..Action::default() }],
                ..body("asks", State::NeedsYou)
            },
        );
        inbox_tab(&mut app);
        app.on_key(key(KeyCode::Enter)); // "asks", the newest
        assert_eq!(app.inbox.focus, InboxFocus::Thread);
        app.take_pending_inbox();

        for c in ['2', '3', '4', 'D', 'v', 'T', 'x'] {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Tab));
        app.on_key(key(KeyCode::Right));
        assert_eq!(app.tab, Tab::Inbox, "tab keys are shadowed");
        assert_eq!(app.inbox.focus, InboxFocus::Thread);
        assert_eq!(app.inbox.view, View::NeedsYou, "`→` didn't reach the list");
        assert!(app.terms.is_empty(), "`T` opened no terminal");
        assert_eq!(app.take_pending_inbox(), [], "`D` dismissed nothing");
        assert_eq!(app.inbox.threads().len(), 2);

        app.on_key(key(KeyCode::Char('1')));
        assert!(app.status.as_deref().unwrap().contains("Post replies"), "{:?}", app.status);
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.take_pending_open().as_deref(), Some("https://x/pr/1"));
        // Quit still works.
        app.on_key(key(KeyCode::Char('q')));
        assert!(app.should_quit);
    }

    #[test]
    fn pane_enter_refuses_non_url_links() {
        let mut app = new_app();
        push(&mut app, "a", rec("x", None, Some("--help")));
        inbox_tab(&mut app);
        app.on_key(key(KeyCode::Enter));
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.take_pending_open(), None);
        assert!(app.status.as_deref().unwrap().starts_with("not a URL"));
    }

    #[test]
    fn pane_scrolls_within_the_rendered_bound_and_resets_on_selection() {
        let mut app = new_app();
        put(&mut app, "d", 10, body("asks", State::NeedsYou));
        put(&mut app, "d", 20, body("other", State::NeedsYou));
        inbox_tab(&mut app);
        app.on_key(key(KeyCode::Enter));
        app.inbox.set_pane_max(3);
        for _ in 0..5 {
            app.on_key(key(KeyCode::Char('j')));
        }
        assert_eq!(app.inbox.scroll, 3);
        app.on_key(key(KeyCode::PageUp));
        assert_eq!(app.inbox.scroll, 0);
        app.on_key(key(KeyCode::Char('G')));
        assert_eq!(app.inbox.scroll, 3);
        // Another thread starts at the top.
        app.on_key(key(KeyCode::Esc));
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.inbox.scroll, 0);
    }

    #[test]
    fn selecting_reads_and_a_change_while_selected_reads_again() {
        let mut app = new_app();
        put(&mut app, "d", 10, body("asks", State::NeedsYou));
        put(&mut app, "d", 20, body("other", State::NeedsYou));
        assert_eq!(app.take_pending_inbox(), [], "off the Inbox nothing is read");
        let (asks, other) = (thread(&app, "asks").id, thread(&app, "other").id);

        // Entering the tab reads the first row: it's in the pane.
        inbox_tab(&mut app);
        assert_eq!(app.take_pending_inbox(), [Op::MarkRead(other)]);
        assert!(thread(&app, "asks").unread);
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.take_pending_inbox(), [Op::MarkRead(asks)]);
        assert!(!thread(&app, "asks").unread);

        // The dispatcher changes it while it's selected: read again on reload.
        // (It moves to the top; a cursor moved down stays on it.)
        put(&mut app, "d", 30, ThreadPut { status: Some("ci".into()), ..body("asks", State::NeedsYou) });
        assert_eq!(selected(&app).as_deref(), Some("asks"));
        assert_eq!(app.take_pending_inbox(), [Op::MarkRead(asks)]);
        assert!(!thread(&app, "asks").unread);
        // A change to another thread isn't read for it.
        put(&mut app, "d", 5, ThreadPut { status: Some("x".into()), ..body("other", State::NeedsYou) });
        assert_eq!(app.take_pending_inbox(), []);
        assert!(thread(&app, "other").unread);

        // The selected thread removed elsewhere: the next one is read.
        app.on_key(key(KeyCode::Enter));
        let mut inbox = app.inbox.content.clone();
        inbox.apply(&Op::RemoveThread(asks), 0, "tui");
        app.set_inbox(inbox);
        assert_eq!(selected(&app).as_deref(), Some("other"));
        assert_eq!(app.take_pending_inbox(), [Op::MarkRead(other)]);
        assert_eq!(app.inbox.focus, InboxFocus::Thread);
    }

    /// Reading a notify record drops it from Needs you; it stays listed while
    /// selected, so the cursor doesn't slide onto (and read) the next one.
    #[test]
    fn a_read_notify_record_stays_listed_while_selected() {
        let mut app = new_app();
        push(&mut app, "a", Record { at: 10, ..rec("n1", None, None) });
        push(&mut app, "a", Record { at: 20, ..rec("n2", None, None) });
        push(&mut app, "a", Record { at: 30, ..rec("n3", None, None) });
        inbox_tab(&mut app);
        assert_eq!(app.take_pending_inbox(), [Op::MarkRead(thread(&app, "n3").id)]);
        assert_eq!(rows(&app), ["n3", "n2", "n1"]);
        assert_eq!(app.inbox.count(View::NeedsYou), 2);
        // Moving on drops the read one, and reads only the next.
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.take_pending_inbox(), [Op::MarkRead(thread(&app, "n2").id)]);
        assert_eq!(rows(&app), ["n2", "n1"]);
        assert_eq!(selected(&app).as_deref(), Some("n2"));
        assert!(thread(&app, "n1").unread);
    }

    /// Notify records have no state: leaving the Inbox after seeing them in
    /// Needs you reads them, so they don't pile up there.
    #[test]
    fn leaving_the_inbox_reads_the_notify_records_it_showed() {
        let mut app = new_app();
        push(&mut app, "a", rec("one", None, None));
        put(&mut app, "d", 10, body("asks", State::NeedsYou));
        // Not shown yet: nothing read.
        app.on_key(key(KeyCode::Char('2')));
        assert_eq!(app.take_pending_inbox(), []);
        inbox_tab(&mut app);
        assert_eq!(rows(&app), ["asks", "one"]);
        assert_eq!(app.take_pending_inbox(), [Op::MarkRead(thread(&app, "asks").id)]);
        app.on_key(key(KeyCode::Char('1')));
        assert_eq!(app.take_pending_inbox(), [Op::MarkNotifyRead]);
        assert_eq!(app.inbox.needs_you(), 1, "the dispatcher's thread still asks");

        // From a view that doesn't list notify records, nothing is read.
        push(&mut app, "a", rec("two", None, None));
        inbox_tab(&mut app);
        app.on_key(key(KeyCode::Right)); // Active: empty
        app.take_pending_inbox();
        app.on_key(key(KeyCode::Char('1')));
        assert_eq!(app.take_pending_inbox(), []);
        assert!(thread(&app, "two").unread);
    }

    #[test]
    fn leaving_the_tab_resets_focus_and_drops_the_reply_box() {
        let mut app = new_app();
        open_asks(&mut app, Some(Compose::default()));
        app.on_key(key(KeyCode::Char('r')));
        typed(&mut app, "half");
        app.set_tab(Tab::Instances);
        assert_eq!(app.inbox.focus, InboxFocus::List);
        assert!(app.inbox.reply.is_none());
        inbox_tab(&mut app);
        assert_eq!(app.inbox.focus, InboxFocus::List);

        // From the thread zone too (`tab` is shadowed there, `:` isn't).
        app.on_key(key(KeyCode::Enter));
        app.next_tab();
        assert_eq!(app.inbox.focus, InboxFocus::List);
    }

    #[test]
    fn d_dismisses_notify_only_and_capital_d_clears_notify_only() {
        let mut app = new_app();
        put(&mut app, "d", 10, body("asks", State::NeedsYou));
        push(&mut app, "a", Record { at: 20, ..rec("n1", None, None) });
        push(&mut app, "a", Record { at: 30, ..rec("n2", None, None) });
        app.inbox.view = View::All;
        inbox_tab(&mut app);
        assert_eq!(rows(&app), ["n2", "n1", "asks"]);
        let (n1, n2) = (thread(&app, "n1").id, thread(&app, "n2").id);
        app.take_pending_inbox();
        app.on_key(key(KeyCode::Char('d')));
        assert_eq!(rows(&app), ["n1", "asks"]);
        // The row under the cursor is gone: the next one is selected, so read.
        assert_eq!(app.take_pending_inbox(), [Op::RemoveThread(n2), Op::MarkRead(n1)]);

        // On a dispatcher thread, `d` marks it done (an event for the store).
        app.on_key(key(KeyCode::Down));
        let asks = thread(&app, "asks").id;
        assert_eq!(app.take_pending_inbox(), [Op::MarkRead(asks)]);
        app.on_key(key(KeyCode::Char('d')));
        assert_eq!(app.take_pending_inbox(), [Op::MarkDone(asks)]);
        assert_eq!(app.status.as_deref(), Some("marked done · event queued for d"));
        assert_eq!(rows(&app), ["n1", "asks"], "applied by the store, not here");

        app.on_key(key(KeyCode::Char('D')));
        assert_eq!(app.take_pending_inbox(), [Op::ClearNotify]);
        assert_eq!(rows(&app), ["asks"]);
        assert_eq!(app.selected(), 0, "re-clamped");
    }

    #[test]
    fn list_keys_act_on_the_selected_thread() {
        let mut app = new_app();
        put(&mut app, "d", 10, body("asks", State::NeedsYou));
        put(&mut app, "d", 20, body("other", State::NeedsYou));
        inbox_tab(&mut app);
        app.on_key(key(KeyCode::Down)); // "asks"
        app.take_pending_inbox();
        // `o` aims at the thread's owner, not an Instances row: none here.
        app.on_key(key(KeyCode::Char('o')));
        assert_eq!(app.pending_action, None);
        assert_eq!(app.status.as_deref(), Some("`d` not found"));
        assert_eq!(app.inbox.focus, InboxFocus::List);
        app.on_key(key(KeyCode::Char('d')));
        assert_eq!(app.take_pending_inbox(), [Op::MarkDone(thread(&app, "asks").id)]);
        app.on_key(key(KeyCode::Char('u')));
        assert_eq!(app.status.as_deref(), Some("not done: nothing to reopen"));
    }

    #[test]
    fn d_off_the_inbox_does_not_dismiss() {
        let mut app = new_app();
        push(&mut app, "a", rec("1", None, None));
        app.on_key(key(KeyCode::Char('d')));
        app.on_key(key(KeyCode::Char('D')));
        assert_eq!(app.inbox.threads().len(), 1);
        assert_eq!(app.take_pending_inbox(), []);
    }

    /// Pane lines as their text, one string per line.
    fn texts(lines: &[PaneLine]) -> Vec<String> {
        lines.iter().map(|l| l.iter().map(|(_, s)| s.as_str()).collect()).collect()
    }

    /// Header rows as text: a line's left, then ` | right`; buttons joined
    /// by two spaces, prefixed `buttons: `.
    fn header_texts(rows: &[HeaderRow]) -> Vec<String> {
        rows.iter()
            .map(|r| match r {
                HeaderRow::Line { left, right: None } => texts(std::slice::from_ref(left)).remove(0),
                HeaderRow::Line { left, right: Some(right) } => {
                    format!("{} | {right}", texts(std::slice::from_ref(left)).remove(0))
                }
                HeaderRow::Buttons(b) => format!("buttons: {}", texts(b).join("  ")),
            })
            .collect()
    }

    fn header_actions() -> Vec<Action> {
        vec![
            Action {
                id: "open".into(),
                label: "Open draft".into(),
                host: Some(crate::inbox::thread::HostVerb::Terminal(Default::default())),
                ..Action::default()
            },
            Action { id: "post".into(), label: "Post replies".into(), ..Action::default() },
            Action { id: "done".into(), label: "Done".into(), done: true, ..Action::default() },
        ]
    }

    #[test]
    fn pane_header_with_a_child_link_and_actions() {
        let mut app = new_app();
        let full = ThreadPut {
            link: Some("https://x/pr/1".into()),
            status: Some("review".into()),
            child: Some("pr-1".into()),
            // v2 put compat, removed in step 13b: the header message.
            message: Some("drafts ready".into()),
            actions: header_actions(),
            compose: Some(Compose { placeholder: Some("next run".into()), hint: Some("starts a run".into()) }),
            ..body("asks", State::NeedsYou)
        };
        put(&mut app, "d", 10, full);
        app.thread_children.insert("d-id".into(), [("pr-1".to_string(), "inst0".to_string())].into());
        app.set_snapshot(snapshot_with_status(1, running()));

        let t = thread(&app, "asks").clone();
        let child = app.thread_child(&t).unwrap();
        assert_eq!((child.name.as_deref(), child.status.as_deref(), child.stopped), (Some("inst0"), Some("running"), false));
        let rows = pane_header(&t, Some(&child), 0);
        assert_eq!(header_texts(&rows), [
            "asks ↗".to_string(),
            format!("● review  ·  {} | d · asks", stamp(10, 0)),
            "child inst0  running".into(),
            "buttons: [1] Open draft ⌂  [2] Post replies  [3] Done ✓  [o] VS Code  [t] Terminal  [l] Logs  [p] Port"
                .into(),
        ]);
        let HeaderRow::Line { left: title, .. } = &rows[0] else { panic!() };
        assert_eq!(title[..], [(Tone::Title, "asks".into()), (Tone::Link, " ↗".into())]);
        // Bold key, plain label.
        let HeaderRow::Buttons(buttons) = &rows[3] else { panic!() };
        for b in [&buttons[0], &buttons[4]] {
            assert_eq!(b[..2].iter().map(|(t, _)| *t).collect::<Vec<_>>(), [Tone::Bold, Tone::Plain]);
        }
        // The message, compose text and events are the feed's and composer's.
        let all = header_texts(&rows).join("\n");
        for gone in ["drafts ready", "next run", "starts a run", "waiting for", "https://x"] {
            assert!(!all.contains(gone), "{gone} in {all}");
        }

        // Stopped: the container is in the snapshot but not up.
        app.set_snapshot(snapshot_with_status(1, ContainerStatus::Exited("Exited (0)".into())));
        let child = app.thread_child(&t).unwrap();
        assert!(child.stopped);
        assert_eq!(header_texts(&pane_header(&t, Some(&child), 0))[2], "child inst0  (stopped)");

        // A child key the owner doesn't have stays visible, unresolved, and
        // the target keys go: they'd only say "not found".
        app.thread_children.clear();
        let child = app.thread_child(&t).unwrap();
        assert_eq!(child.name, None);
        let text = header_texts(&pane_header(&t, Some(&child), 0));
        assert_eq!(text[2], "child pr-1 (no such child)");
        assert_eq!(text[3], "buttons: [1] Open draft ⌂  [2] Post replies  [3] Done ✓");
    }

    #[test]
    fn pane_header_without_a_child_archived_and_bare() {
        let mut app = new_app();
        put(&mut app, "d", 10, body("asks", State::Active));
        let t = thread(&app, "asks").clone();
        // No child row; the target keys aim at the owner.
        assert_eq!(header_texts(&pane_header(&t, None, 0)), [
            "asks".to_string(),
            format!("○ active  ·  {} | d · asks", stamp(10, 0)),
            "buttons: [o] VS Code  [t] Terminal  [l] Logs  [p] Port".into(),
        ]);
        // Archived: said in the header, and no keys (every one is refused).
        let mut inbox = app.inbox.content.clone();
        inbox.archive_owner("d-id");
        app.set_inbox(inbox);
        let t = thread(&app, "asks").clone();
        assert_eq!(header_texts(&pane_header(&t, None, 0)), [
            "asks".to_string(),
            format!("○ active  ·  {} | d · asks", stamp(10, 0)),
            "(archived: instance removed)".into(),
        ]);
    }

    #[test]
    fn pane_header_of_a_notify_thread_is_title_and_level() {
        let mut app = new_app();
        push(&mut app, "a", Record { level: Level::Warn, at: 5, ..rec("disk low\nmore", Some("k"), Some("https://x")) });
        let t = thread(&app, "disk low").clone();
        assert_eq!(header_texts(&pane_header(&t, None, 0)), [
            "disk low ↗".to_string(),
            format!("▲ warn  ·  {} | a · k", stamp(5, 0)),
        ]);
        push(&mut app, "a", rec("unkeyed", None, None));
        let t = thread(&app, "unkeyed").clone();
        assert_eq!(header_texts(&pane_header(&t, None, 0))[1], format!("· info  ·  {} | a", stamp(0, 0)));
    }

    /// The feed newest first: messages under their author's stamp row with
    /// an `(edited)`/`(withdrawn)` tag, fields as aligned rows, the user's
    /// items as `you`, markers as one dim `·` row each.
    #[test]
    fn pane_feed_is_newest_first() {
        let mut app = new_app();
        let v2 = |message: Option<&str>, state: State, status: &str| ThreadPut {
            status: Some(status.into()),
            message: message.map(str::to_string),
            compose: Some(Compose::default()),
            actions: vec![Action { id: "post".into(), label: "Post replies".into(), ..Action::default() }],
            ..body("asks", state)
        };
        put(&mut app, "d", 10, v2(Some("one"), State::Active, "running"));
        let id = thread(&app, "asks").id;
        let mut inbox = app.inbox.content.clone();
        inbox.apply(&Op::Act { thread: id, action: "post".into() }, 20, "tui");
        inbox.apply(&Op::MarkDone(id), 20, "tui");
        inbox.apply(&Op::Reopen(id), 20, "tui");
        inbox.put("d-id", "d", 30, v2(Some("two"), State::NeedsYou, "review"));
        inbox.apply(&Op::Reply { thread: id, text: "ok".into() }, 40, "tui");
        // A fields message, straight into the feed (no put field makes one yet).
        let fields = vec![feed::Block::Fields { items: vec![feed::Field { label: "CI".into(), value: "green".into() }] }];
        let t = inbox.threads.iter_mut().find(|t| t.id == id).unwrap();
        let seq = t.feed.iter().map(|i| i.seq).max().unwrap_or(0) + 1;
        t.feed.push(feed::FeedItem {
            seq,
            at: 50,
            kind: ItemKind::Message { id: "f".into(), blocks: fields, edited: false, withdrawn: false, form: None },
        });
        app.set_inbox(inbox);
        let (s10, s20, s30, s40, s50) = (stamp(10, 0), stamp(20, 0), stamp(30, 0), stamp(40, 0), stamp(50, 0));
        let lines = pane_feed(thread(&app, "asks"), 0);
        let text = texts(&lines);
        // The reply's event first (state, not history), then the feed.
        assert_eq!(text, [
            "4 events waiting for d".to_string(),
            String::new(),
            format!("{s50}  d"),
            "CI  green".into(),
            String::new(),
            format!("{s40}  you"),
            "ok".into(),
            String::new(),
            format!("{s30}  · status: running → review"),
            format!("{s30}  · active → needs you"),
            format!("{s20}  · reopened"),
            format!("{s20}  · done"),
            format!("{s20}  you: Post replies"),
            String::new(),
            format!("{s10}  d  (edited)"),
            "two".into(),
        ], "{text:#?}");
        // Markers are dim; the message is a markdown document.
        assert_eq!(lines[15], [(Tone::Markdown, "two".to_string())]);
        assert_eq!(lines[11], [(Tone::Dim, format!("{s20}  · done"))]);

        // Withdrawn: its row stays, tagged, without the text.
        let mut inbox = app.inbox.content.clone();
        inbox.put("d-id", "d", 60, v2(None, State::NeedsYou, "review"));
        app.set_inbox(inbox);
        let text = texts(&pane_feed(thread(&app, "asks"), 0));
        assert_eq!(text.last(), Some(&format!("{s10}  d  (withdrawn)")), "{text:#?}");
        assert!(!text.iter().any(|l| l == "two"), "{text:#?}");
    }

    #[test]
    fn notify_feed_is_the_records_newest_first() {
        let mut app = new_app();
        push(&mut app, "a", Record { at: 1, ..rec("v1", Some("k"), None) });
        push(&mut app, "a", Record { at: 2, level: Level::Error, ..rec("v2", Some("k"), None) });
        push(&mut app, "a", Record { at: 3, ..rec("v3\n\n- body", Some("k"), None) });
        let lines = pane_feed(thread(&app, "v3"), 0);
        assert_eq!(texts(&lines), [
            format!("{}  info", stamp(3, 0)),
            "v3\n\n- body".into(),
            String::new(),
            format!("{}  error", stamp(2, 0)),
            "v2".into(),
            String::new(),
            format!("{}  info", stamp(1, 0)),
            "v1".into(),
        ]);
        assert_eq!(lines[3][1], (Tone::Level(Level::Error), "error".into()));
        assert_eq!(lines[1], [(Tone::Markdown, "v3\n\n- body".to_string())]);
    }

    /// What the event loop does with the queued ops: apply them to the store
    /// at time `now`, then reload.
    fn flush(app: &mut App, now: u64) -> Vec<Op> {
        let ops = app.take_pending_inbox();
        let mut inbox = app.inbox.content.clone();
        ops.iter().for_each(|op| inbox.apply(op, now, "tui"));
        app.set_inbox(inbox);
        ops
    }

    fn typed(app: &mut App, text: &str) {
        for c in text.chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
    }

    /// `asks` from `d`, taking replies, its thread focused, its read mark
    /// flushed.
    fn open_asks(app: &mut App, compose: Option<Compose>) -> u64 {
        put(app, "d", 10, ThreadPut { compose, ..body("asks", State::NeedsYou) });
        inbox_tab(app);
        app.on_key(key(KeyCode::Enter));
        flush(app, 10);
        thread(app, "asks").id
    }

    #[test]
    fn r_focuses_the_input_which_sends_replies_and_stays() {
        let mut app = new_app();
        let id = open_asks(&mut app, Some(Compose { placeholder: Some("next run".into()), hint: None }));
        app.on_key(key(KeyCode::Char('r')));
        assert_eq!(app.inbox.focus, InboxFocus::Input);
        let rb = app.inbox.reply.as_ref().expect("reply box open");
        assert_eq!((rb.thread, rb.line.input()), (id, ""));

        // Keys edit the line, not the pane: `d`, `q`, `1` are just text.
        typed(&mut app, "do q1x");
        app.on_key(key(KeyCode::Backspace));
        app.on_key(key(KeyCode::Home));
        app.on_key(key(KeyCode::Char('>')));
        assert!(!app.should_quit);
        assert_eq!(app.inbox.reply.as_ref().unwrap().line.input(), ">do q1");
        assert_eq!(app.take_pending_inbox(), []);

        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.inbox.focus, InboxFocus::Input, "still in the input");
        assert_eq!(app.inbox.reply.as_ref().unwrap().line.input(), "", "cleared for the next one");
        assert_eq!(app.status.as_deref(), Some("reply queued for d"));
        assert_eq!(flush(&mut app, 20), [Op::Reply { thread: id, text: ">do q1".into() }]);
        // And again, without leaving the input.
        typed(&mut app, "more");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.take_pending_inbox(), [Op::Reply { thread: id, text: "more".into() }]);
        assert_eq!(app.inbox.focus, InboxFocus::Input);
        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.inbox.focus, InboxFocus::Thread);
        app.on_key(key(KeyCode::Char('i')));
        assert_eq!(app.inbox.focus, InboxFocus::Input, "`i` focuses it too");
        let text = texts(&pane_feed(thread(&app, "asks"), 0));
        assert!(text.windows(2).any(|w| w[0].ends_with("  you") && w[1] == ">do q1"), "{text:#?}");
        assert!(text.contains(&"1 event waiting for d".to_string()), "{text:#?}");
    }

    #[test]
    fn pane_head_is_the_chip_then_the_stamp() {
        let mut app = new_app();
        put(&mut app, "d", 10, ThreadPut { status: Some("review `drafts`".into()), ..body("asks", State::NeedsYou) });
        put(&mut app, "d", 10, body("idle", State::Active));
        put(&mut app, "d", 10, ThreadPut { status: Some("merged".into()), ..body("fin", State::Done) });
        push(&mut app, "a", Record { level: Level::Error, ..rec("boom", None, None) });
        let head = |title: &str| match pane_header(thread(&app, title), None, 0).swap_remove(1) {
            HeaderRow::Line { left, .. } => left,
            row => panic!("{row:?}"),
        };
        let at10 = (Tone::Dim, format!("  ·  {}", stamp(10, 0)));
        assert_eq!(head("asks"), [
            (Tone::State(State::NeedsYou), "● ".into()),
            (Tone::Text, "review `drafts`".into()),
            at10.clone()
        ]);
        assert_eq!(head("idle"), [(Tone::State(State::Active), "○ active".into()), at10.clone()]);
        assert_eq!(head("fin"), [(Tone::State(State::Done), "✓ ".into()), (Tone::Text, "merged".into()), at10]);
        assert_eq!(head("boom")[..2], [
            (Tone::Level(Level::Error), "✖ error".into()),
            (Tone::Dim, format!("  ·  {}", stamp(thread(&app, "boom").changed_at(), 0)))
        ]);
        for title in ["asks", "idle", "fin", "boom"] {
            assert!(head(title).starts_with(&chip(thread(&app, title))), "pane head starts with the chip: {title}");
        }
    }

    #[test]
    fn a_multi_line_reply_keeps_its_lines_as_markdown() {
        let mut app = new_app();
        let id = open_asks(&mut app, Some(Compose::default()));
        let mut inbox = app.inbox.content.clone();
        inbox.apply(&Op::Reply { thread: id, text: "first line\n\n- a\n- b".into() }, 20, "tui");
        app.set_inbox(inbox);
        let lines = pane_feed(thread(&app, "asks"), 0);
        let at = lines.iter().position(|l| l.len() == 1 && l[0].0 == Tone::Dim && l[0].1.ends_with("  you")).unwrap();
        assert_eq!(lines[at][0].1, format!("{}  you", stamp(20, 0)));
        assert_eq!(lines[at + 1], [(Tone::Markdown, "first line\n\n- a\n- b".to_string())]);
    }

    #[test]
    fn the_reply_box_cancels_and_ignores_empty() {
        let mut app = new_app();
        open_asks(&mut app, Some(Compose::default()));
        app.on_key(key(KeyCode::Char('r')));
        // Empty (or blank) enter: nothing sent, the box stays.
        typed(&mut app, "  ");
        app.on_key(key(KeyCode::Enter));
        assert!(app.inbox.reply.is_some());
        typed(&mut app, "half");
        app.on_key(key(KeyCode::Esc));
        assert!(app.inbox.reply.is_none());
        assert_eq!(app.inbox.focus, InboxFocus::Thread, "esc steps back to the thread");
        assert_eq!(app.take_pending_inbox(), []);
        // From the list, `r` goes straight to the input.
        app.on_key(key(KeyCode::Esc));
        app.on_key(key(KeyCode::Char('r')));
        assert_eq!(app.inbox.focus, InboxFocus::Input);
        assert!(app.inbox.reply.is_some());
    }

    #[test]
    fn a_selection_change_drops_the_reply_box() {
        let mut app = new_app();
        open_asks(&mut app, Some(Compose::default()));
        put(&mut app, "d", 20, body("other", State::NeedsYou));
        app.on_key(key(KeyCode::Char('r')));
        typed(&mut app, "half");
        // The thread is removed under the input: the pane moves on.
        let mut inbox = app.inbox.content.clone();
        inbox.apply(&Op::RemoveThread(thread(&app, "asks").id), 0, "tui");
        app.set_inbox(inbox);
        assert_eq!(selected(&app).as_deref(), Some("other"));
        assert!(app.inbox.reply.is_none());
        assert_eq!(app.inbox.focus, InboxFocus::Thread);
    }

    #[test]
    fn m_in_the_thread_toggles_raw_for_every_thread() {
        let mut app = new_app();
        open_asks(&mut app, Some(Compose::default()));
        put(&mut app, "d", 20, body("other", State::NeedsYou));
        app.on_key(key(KeyCode::Char('m')));
        assert!(app.inbox.raw);
        // Not per thread: it holds on the next one.
        app.on_key(key(KeyCode::Esc));
        app.on_key(key(KeyCode::Up));
        assert_eq!(selected(&app).as_deref(), Some("other"));
        assert!(app.inbox.raw);
        // In the input, `m` is text.
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Char('r')));
        app.on_key(key(KeyCode::Char('m')));
        assert_eq!(app.inbox.reply.as_ref().unwrap().line.input(), "m");
        assert!(app.inbox.raw);
        app.on_key(key(KeyCode::Esc));
        app.on_key(key(KeyCode::Char('m')));
        assert!(!app.inbox.raw);
    }

    #[test]
    fn r_without_a_reply_box_is_a_hint() {
        let mut app = new_app();
        open_asks(&mut app, None);
        app.on_key(key(KeyCode::Char('r')));
        assert!(app.inbox.reply.is_none());
        assert_eq!(app.status.as_deref(), Some("this thread takes no replies"));
        assert_eq!(app.inbox.focus, InboxFocus::Thread, "focus stays");
        app.on_key(key(KeyCode::Esc));
        app.on_key(key(KeyCode::Char('i')));
        assert_eq!(app.status.as_deref(), Some("this thread takes no replies"));
        assert_eq!(app.inbox.focus, InboxFocus::List, "focus stays");
        assert_eq!(app.take_pending_inbox(), []);
    }

    #[test]
    fn pane_d_marks_done_and_u_reopens() {
        let mut app = new_app();
        let id = open_asks(&mut app, None);
        app.on_key(key(KeyCode::Char('u')));
        assert_eq!(app.status.as_deref(), Some("not done: nothing to reopen"));
        app.on_key(key(KeyCode::Char('d')));
        assert_eq!(flush(&mut app, 20), [Op::MarkDone(id)]);
        assert_eq!(thread(&app, "asks").state, Some(State::Done));
        assert_eq!(app.inbox.focus, InboxFocus::Thread, "the pane stays on the thread");
        app.on_key(key(KeyCode::Char('d')));
        assert_eq!(app.status.as_deref(), Some("already done"));
        assert_eq!(app.take_pending_inbox(), []);
        app.on_key(key(KeyCode::Char('u')));
        assert_eq!(app.status.as_deref(), Some("reopened · event queued for d"));
        assert_eq!(flush(&mut app, 30), [Op::Reopen(id)]);
        let t = thread(&app, "asks");
        assert_eq!(t.state, Some(State::Active));
        assert_eq!(t.events.len(), 2);
        let text = texts(&pane_feed(t, 0));
        assert!(text.contains(&format!("{}  · done", stamp(20, 0))), "{text:#?}");
        assert!(text.contains(&format!("{}  · reopened", stamp(30, 0))), "{text:#?}");
        assert!(text.contains(&"2 events waiting for d".to_string()), "{text:#?}");
    }

    #[test]
    fn archived_threads_take_no_user_ops() {
        let mut app = new_app();
        put(&mut app, "d", 10, ThreadPut { compose: Some(Compose::default()), ..body("asks", State::NeedsYou) });
        let mut inbox = app.inbox.content.clone();
        inbox.archive_owner("d-id");
        app.set_inbox(inbox);
        inbox_tab(&mut app);
        app.inbox.view = View::All;
        app.on_key(key(KeyCode::Char('d')));
        app.on_key(key(KeyCode::Enter));
        flush(&mut app, 10);
        for c in ['r', 'd', 'u'] {
            app.on_key(key(KeyCode::Char(c)));
            assert!(app.status.as_deref().unwrap().starts_with("archived"), "{c}: {:?}", app.status);
        }
        assert!(app.inbox.reply.is_none());
        assert_eq!(app.take_pending_inbox(), []);
    }

    #[test]
    fn url_check() {
        assert!(is_url("https://github.com/o/r/pull/1"));
        assert!(is_url("HTTP://example.com"));
        assert!(!is_url("vscode://file/x"));
        assert!(!is_url("file:///etc/passwd"));
        assert!(!is_url("https://"));
        assert!(!is_url("-x"));
        assert!(!is_url("/etc/passwd"));
        assert!(!is_url("1http://x"));
        assert!(!is_url("http:"));
        assert!(!is_url("-a:b"));
    }

    #[test]
    fn short_age_then_date() {
        let now = 1_000_000_000;
        assert_eq!(short_age(now, now, 0), "now");
        assert_eq!(short_age(now + 5, now, 0), "now", "skewed clock");
        assert_eq!(short_age(now - 59, now, 0), "now");
        assert_eq!(short_age(now - 60, now, 0), "1m");
        assert_eq!(short_age(now - 5 * 60 - 30, now, 0), "5m");
        assert_eq!(short_age(now - 3599, now, 0), "59m");
        assert_eq!(short_age(now - 3 * 3600, now, 0), "3h");
        assert_eq!(short_age(now - 86_399, now, 0), "23h");
        assert_eq!(short_age(now - 2 * 86_400, now, 0), "2d");
        assert_eq!(short_age(now - (7 * 86_400 - 1), now, 0), "6d");
        // 1_000_000_000 is 2001-09-09 01:46:40 UTC: a week back is Sep 2,
        // and the date follows the local offset across midnight.
        assert_eq!(short_age(now - 7 * 86_400, now, 0), "Sep 2");
        assert_eq!(short_age(now - 7 * 86_400, now, -2 * 3600), "Sep 1");
    }

    #[test]
    fn view_change_keeps_the_thread_or_goes_to_the_top() {
        let mut app = new_app();
        mixed(&mut app);
        inbox_tab(&mut app);
        for _ in 0..3 {
            app.on_key(key(KeyCode::Right)); // All
        }
        // In All, select "over"; Done lists it too, at another row.
        let pos = rows(&app).iter().position(|r| r == "over").unwrap();
        assert_ne!(pos, 0);
        app.selected[Tab::Inbox.index()] = pos;
        app.sync_inbox_selection();
        app.inbox.set_list_offset(3);
        app.on_key(key(KeyCode::Left)); // Done
        assert_eq!(app.inbox.view, View::Done);
        assert_eq!((app.selected(), selected(&app).as_deref()), (0, Some("over")), "kept by id");
        assert_eq!(app.inbox.list_offset(), 3, "offset left to the renderer");
        app.on_key(key(KeyCode::Left)); // Active
        app.on_key(key(KeyCode::Left)); // Needs you

        // A second Active thread, so "top" differs from the old row number.
        put(&mut app, "d", 15, body("busy2", State::Active));
        put(&mut app, "d", 12, body("asks2", State::NeedsYou));
        assert_eq!(rows(&app), ["asks2", "asks"]);
        app.selected[Tab::Inbox.index()] = 1;
        app.sync_inbox_selection();
        assert_eq!(selected(&app).as_deref(), Some("asks"));
        app.inbox.set_list_offset(2);
        app.on_key(key(KeyCode::Right)); // Active: busy, busy2
        assert_eq!(app.inbox.view, View::Active);
        assert_eq!(rows(&app), ["busy", "busy2"]);
        assert_eq!(app.selected(), 0, "top of the new view, not row 1");
        assert_eq!(selected(&app).as_deref(), Some("busy"));
        assert_eq!(app.inbox.list_offset(), 0, "offset reset");
    }

    // FRAME (100x40, no prompt): content rows 1..=38; without terminals the
    // split fills them all, the 40% divider (thread pane's left border) at 40.
    const ROW: u16 = 10;

    #[test]
    fn inbox_mouse_drag_resizes_divider() {
        let mut app = new_app();
        inbox_tab(&mut app);
        assert_eq!(app.inbox.split_pct, 40);
        // A cell off the divider still grabs it.
        app.on_mouse(&mouse_at(MouseEventKind::Down(MouseButton::Left), 39, ROW), FRAME);
        assert!(app.inbox.dragging);
        app.on_mouse(&mouse_at(MouseEventKind::Drag(MouseButton::Left), 30, ROW), FRAME);
        assert_eq!(app.inbox.split_pct, 30);
        // Clamped like the config modal's split, both ways.
        app.on_mouse(&mouse_at(MouseEventKind::Drag(MouseButton::Left), 2, ROW), FRAME);
        assert_eq!(app.inbox.split_pct, 20);
        app.on_mouse(&mouse_at(MouseEventKind::Drag(MouseButton::Left), 99, ROW), FRAME);
        assert_eq!(app.inbox.split_pct, 80);
        // Release; a later drag with no press does nothing.
        app.on_mouse(&mouse_at(MouseEventKind::Up(MouseButton::Left), 99, ROW), FRAME);
        assert!(!app.inbox.dragging);
        app.on_mouse(&mouse_at(MouseEventKind::Drag(MouseButton::Left), 50, ROW), FRAME);
        assert_eq!(app.inbox.split_pct, 80);
    }

    #[test]
    fn inbox_mouse_press_off_the_divider_ignored() {
        let mut app = new_app();
        inbox_tab(&mut app);
        // Too far left, and on the divider's column but in the tab line or
        // the help bar (outside the split's rows).
        for (col, row) in [(37, ROW), (40, 0), (40, 39)] {
            app.on_mouse(&mouse_at(MouseEventKind::Down(MouseButton::Left), col, row), FRAME);
            app.on_mouse(&mouse_at(MouseEventKind::Drag(MouseButton::Left), 60, row), FRAME);
            assert_eq!(app.inbox.split_pct, 40, "press at ({col}, {row})");
        }
    }

    #[test]
    fn inbox_mouse_only_on_the_inbox_tab_unobstructed() {
        let mut app = new_app();
        // Another tab: the same press does nothing.
        app.on_mouse(&mouse_at(MouseEventKind::Down(MouseButton::Left), 40, ROW), FRAME);
        app.on_mouse(&mouse_at(MouseEventKind::Drag(MouseButton::Left), 60, ROW), FRAME);
        assert_eq!(app.inbox.split_pct, 40);
        // Nor with the prompt over the Inbox.
        inbox_tab(&mut app);
        app.on_key(key(KeyCode::Char(':')));
        assert!(app.prompt.is_some());
        app.on_mouse(&mouse_at(MouseEventKind::Down(MouseButton::Left), 40, ROW), FRAME);
        app.on_mouse(&mouse_at(MouseEventKind::Drag(MouseButton::Left), 60, ROW), FRAME);
        assert_eq!(app.inbox.split_pct, 40);
    }

    #[test]
    fn inbox_mouse_with_terminals_stays_above_the_panel() {
        let mut app = new_app();
        inbox_tab(&mut app);
        open_test_term(&mut app, "web-1", "devsandbox-web-1");
        let panel = crate::tui::ui::terminal_panel_rect(FRAME, false);
        // In the terminal panel's rows: not the divider; the click focuses
        // the terminal as before.
        app.focus = super::super::Focus::Dashboard;
        app.on_mouse(&mouse_at(MouseEventKind::Down(MouseButton::Left), 40, panel.y + 1), FRAME);
        app.on_mouse(&mouse_at(MouseEventKind::Drag(MouseButton::Left), 60, panel.y + 1), FRAME);
        assert_eq!(app.inbox.split_pct, 40);
        assert_eq!(app.focus, super::super::Focus::Terminal);
        // Above it: drags.
        app.on_mouse(&mouse_at(MouseEventKind::Down(MouseButton::Left), 40, panel.y - 1), FRAME);
        app.on_mouse(&mouse_at(MouseEventKind::Drag(MouseButton::Left), 60, panel.y - 1), FRAME);
        assert_eq!(app.inbox.split_pct, 60);
    }

    // FRAME without terminals: view strip on row 1 (cols 0..40), list block
    // rows 2..=38 with cards from row 3 (card 0: rows 3-4, spacer 5, card 1:
    // rows 6-7); thread pane cols 40..100, rows 1..=38, inner rows 2..=37: an
    // input in rows 35..=37, or the hint on row 37.
    fn click(app: &mut App, col: u16, row: u16) {
        app.on_mouse(&mouse_at(MouseEventKind::Down(MouseButton::Left), col, row), FRAME);
        app.on_mouse(&mouse_at(MouseEventKind::Up(MouseButton::Left), col, row), FRAME);
    }

    #[test]
    fn click_card_selects_reads_and_focuses_the_list() {
        let mut app = new_app();
        put(&mut app, "d", 10, body("asks", State::NeedsYou));
        put(&mut app, "d", 20, body("other", State::NeedsYou));
        inbox_tab(&mut app);
        let asks = thread(&app, "asks").id;
        app.take_pending_inbox();
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.inbox.focus, InboxFocus::Thread);

        click(&mut app, 10, 6);
        assert_eq!(selected(&app).as_deref(), Some("asks"));
        assert_eq!(app.inbox.focus, InboxFocus::List);
        assert_eq!(app.take_pending_inbox(), [Op::MarkRead(asks)]);
        // The spacer and rows past the last card select nothing.
        click(&mut app, 10, 5);
        click(&mut app, 10, 9);
        assert_eq!(selected(&app).as_deref(), Some("asks"));
        // The offset the renderer last drew with shifts the cards.
        app.inbox.set_list_offset(1);
        click(&mut app, 10, 3);
        assert_eq!(selected(&app).as_deref(), Some("asks"));
        app.inbox.set_list_offset(0);
        click(&mut app, 10, 3);
        assert_eq!(selected(&app).as_deref(), Some("other"));
        // A press on the divider still drags, and isn't a click.
        app.on_key(key(KeyCode::Enter));
        app.on_mouse(&mouse_at(MouseEventKind::Down(MouseButton::Left), 40, 6), FRAME);
        assert!(app.inbox.dragging);
        assert_eq!(app.inbox.focus, InboxFocus::Thread);
    }

    #[test]
    fn click_pane_and_input_focus_them() {
        let mut app = new_app();
        put(&mut app, "d", 10, ThreadPut { compose: Some(Compose::default()), ..body("asks", State::NeedsYou) });
        inbox_tab(&mut app);
        click(&mut app, 60, 10);
        assert_eq!(app.inbox.focus, InboxFocus::Thread);
        click(&mut app, 60, 36);
        assert_eq!(app.inbox.focus, InboxFocus::Input);
        typed(&mut app, "half");
        // Clicking the input again keeps the line.
        click(&mut app, 60, 35);
        assert_eq!(app.inbox.reply.as_ref().unwrap().line.input(), "half");
        // Back to the thread, like `esc`.
        click(&mut app, 60, 10);
        assert_eq!(app.inbox.focus, InboxFocus::Thread);
        assert!(app.inbox.reply.is_none());
        // The list, on a card.
        click(&mut app, 10, 3);
        assert_eq!(app.inbox.focus, InboxFocus::List);
    }

    #[test]
    fn click_where_a_no_reply_thread_has_its_hint_focuses_the_thread() {
        let mut app = new_app();
        put(&mut app, "d", 10, body("asks", State::NeedsYou));
        inbox_tab(&mut app);
        for row in [36, 37] {
            app.inbox.focus = InboxFocus::List;
            click(&mut app, 60, row);
            assert_eq!(app.inbox.focus, InboxFocus::Thread, "row {row}");
            assert!(app.inbox.reply.is_none());
        }
    }

    #[test]
    fn click_strip_switches_views() {
        let mut app = new_app();
        put(&mut app, "d", 10, body("asks", State::NeedsYou));
        put(&mut app, "d", 20, body("busy", State::Active));
        inbox_tab(&mut app);
        // 40 columns drop the counts: ` ‹ Needs you │ Active │ Done │ All ›`.
        click(&mut app, 16, 1);
        assert_eq!(app.inbox.view, View::Active);
        assert_eq!(selected(&app).as_deref(), Some("busy"));
        click(&mut app, 35, 1); // ›
        assert_eq!(app.inbox.view, View::Done);
        click(&mut app, 2, 1); // ‹
        assert_eq!(app.inbox.view, View::Active);
        click(&mut app, 32, 1);
        assert_eq!(app.inbox.view, View::All);
        click(&mut app, 14, 1); // the separator before Active
        assert_eq!(app.inbox.view, View::All);
    }

    #[test]
    fn inbox_wheel_moves_the_selection_and_scrolls_the_pane() {
        let mut app = new_app();
        put(&mut app, "d", 10, body("asks", State::NeedsYou));
        put(&mut app, "d", 20, body("other", State::NeedsYou));
        inbox_tab(&mut app);
        app.on_mouse(&mouse_at(MouseEventKind::ScrollDown, 10, 10), FRAME);
        assert_eq!(selected(&app).as_deref(), Some("asks"));
        app.on_mouse(&mouse_at(MouseEventKind::ScrollUp, 10, 10), FRAME);
        assert_eq!(selected(&app).as_deref(), Some("other"));
        app.inbox.set_pane_max(4);
        app.on_mouse(&mouse_at(MouseEventKind::ScrollDown, 60, 10), FRAME);
        assert_eq!(app.inbox.scroll, 3);
        app.on_mouse(&mouse_at(MouseEventKind::ScrollDown, 60, 10), FRAME);
        assert_eq!(app.inbox.scroll, 4, "clamped to the drawn bound");
        app.on_mouse(&mouse_at(MouseEventKind::ScrollUp, 60, 10), FRAME);
        assert_eq!(app.inbox.scroll, 1);
        assert_eq!(selected(&app).as_deref(), Some("other"));
    }

    #[test]
    fn inbox_clicks_ignored_under_a_modal_or_the_prompt() {
        let mut app = new_app();
        put(&mut app, "d", 10, body("asks", State::NeedsYou));
        put(&mut app, "d", 20, body("other", State::NeedsYou));
        inbox_tab(&mut app);
        app.on_key(key(KeyCode::Char(':')));
        click(&mut app, 10, 6);
        click(&mut app, 60, 10);
        assert_eq!((selected(&app).as_deref(), app.inbox.focus), (Some("other"), InboxFocus::List));
        app.on_key(key(KeyCode::Esc));
        assert!(app.prompt.is_none());
        app.on_key(key(KeyCode::Char('?')));
        assert!(!matches!(app.modal, Modal::None));
        click(&mut app, 10, 6);
        click(&mut app, 60, 10);
        assert_eq!((selected(&app).as_deref(), app.inbox.focus), (Some("other"), InboxFocus::List));
    }

    #[test]
    fn click_thread_while_a_terminal_has_focus_moves_focus_there() {
        let mut app = new_app();
        put(&mut app, "d", 10, body("asks", State::NeedsYou));
        inbox_tab(&mut app);
        open_test_term(&mut app, "web-1", "devsandbox-web-1");
        assert_eq!(app.focus, super::super::Focus::Terminal);
        // With terminals the split ends above the panel; row 5 is in the pane.
        click(&mut app, 60, 5);
        assert_eq!(app.focus, super::super::Focus::Dashboard);
        assert_eq!(app.inbox.focus, InboxFocus::Thread);
    }

    #[test]
    fn click_tab_leaving_the_inbox_reads_its_notify_records() {
        let mut app = new_app();
        push(&mut app, "a", rec("note", None, None));
        push(&mut app, "b", rec("other note", None, None));
        inbox_tab(&mut app); // reads the selected one only
        app.take_pending_inbox();
        let tabs = crate::tui::ui::tab_spans(&app);
        let services = tabs.iter().find(|(t, _, _)| *t == Tab::Services).unwrap().2.start;
        click(&mut app, services, 0);
        assert_eq!(app.tab, Tab::Services);
        assert_eq!(app.take_pending_inbox(), [Op::MarkNotifyRead]);
    }

    #[test]
    fn stamp_and_offset() {
        assert_eq!(stamp(0, 0), "Jan 1 00:00");
        assert_eq!(stamp(13 * 3600 + 7 * 60 + 59, 0), "Jan 1 13:07");
        assert_eq!(stamp(23 * 3600, 2 * 3600), "Jan 2 01:00");
        assert_eq!(stamp(3600, -2 * 3600), "Dec 31 23:00");
        // Leap day, and the day after (2024-02-29 / 03-01, noon UTC).
        assert_eq!(stamp(1_709_208_000, 0), "Feb 29 12:00");
        assert_eq!(stamp(1_709_294_400, 0), "Mar 1 12:00");
        assert_eq!(stamp(1_000_000_000, 0), "Sep 9 01:46");
        assert_eq!(parse_utc_offset("+0200\n"), Some(7200));
        assert_eq!(parse_utc_offset("-0530"), Some(-(5 * 3600 + 30 * 60)));
        assert_eq!(parse_utc_offset("CEST"), None);
        assert_eq!(parse_utc_offset("+02"), None);
        assert_eq!(parse_utc_offset(""), None);
    }
}
