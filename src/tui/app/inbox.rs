//! Inbox tab: this dashboard's view of the shared store (`crate::inbox`,
//! `inbox.toml`). The content comes from the store — loaded at startup,
//! reloaded whenever the file changes, since any dashboard or CLI may write
//! it — and everything here is view state: the current view, the selection,
//! the focused zone, all unit-testable without touching the file.
//!
//! Mutations (`d`, `D`, mark-read, and the pane's actions, replies, done and
//! reopen) are only *requested* here, as [`crate::inbox::Op`]s the event loop
//! applies through `store::update` before reloading; the local copy is
//! updated at once so the next frame already shows the result, except for
//! ops that enqueue an event: their ids and times are minted under the store
//! lock, and the reload in the same loop pass shows them. Opening a link is
//! requested the same way (`pending_open`).
//!
//! The list is one row per thread, newest change first, filtered by a
//! [`View`] (docs/inbox-threads.md, *Inbox UI* and *Inbox layout v2*). The
//! thread pane beside it always shows the selected thread: its timeline, or a
//! notify thread's earlier records. Keys go to one of three [`InboxFocus`]
//! zones; the thread and its input shadow the dashboard keys.
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
use crate::inbox::{is_url, EntryKind, Inbox, Kind, Op, State, Thread};
use crate::tui::prompt::Prompt;

use super::view::{col_near, divider_pct, point_in};
use super::{App, Modal, Tab};
use crate::tui::ui;

/// Which threads the list shows, stepped with `←`/`→`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum View {
    /// Waiting on the user ([`Thread::needs_you`]): the default, and what the
    /// badges count.
    #[default]
    NeedsYou,
    Active,
    Done,
    /// Everything, archived threads and read notify records included.
    All,
}

impl View {
    pub const ALL: [View; 4] = [View::NeedsYou, View::Active, View::Done, View::All];

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

    /// Whether `t` belongs in this view. Archived threads (their owner is
    /// gone) are history and only show in All; so do read notify records,
    /// which have no state to file them under.
    pub fn shows(self, t: &Thread) -> bool {
        match self {
            View::NeedsYou => t.needs_you(),
            View::Active => !t.archived && t.state == Some(State::Active),
            View::Done => !t.archived && t.state == Some(State::Done),
            View::All => true,
        }
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
    dragging: bool,
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
        }
    }
}

/// The one-line reply input at the bottom of the thread pane. It reuses the `:`
/// prompt's editing ([`Prompt`], with no history and no completion): only
/// what `enter` does differs.
pub struct ReplyBox {
    /// Id of the thread being replied to.
    pub thread: u64,
    pub line: Prompt,
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

    /// Renderer hook: the list's first visible card, as of the last frame.
    pub fn list_offset(&self) -> usize {
        self.list_offset.get()
    }

    pub fn set_list_offset(&self, offset: usize) {
        self.list_offset.set(offset);
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
}

/// How a pane segment is drawn; the renderer maps these to styles, so the
/// content stays plain data that tests can read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Plain,
    Dim,
    Bold,
    Link,
    State(State),
    Level(Level),
}

/// One pane line: styled segments, unwrapped.
pub type PaneLine = Vec<(Tone, String)>;

fn line(tone: Tone, text: impl Into<String>) -> PaneLine {
    vec![(tone, text.into())]
}

/// A multi-line value folded onto one timeline row.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn entry_label(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::Message => "message",
        EntryKind::State => "state",
        EntryKind::Status => "status",
        EntryKind::Action => "action",
        EntryKind::Reply => "reply",
        EntryKind::Done => "done",
        EntryKind::Reopen => "reopen",
    }
}

/// "2 events waiting for web": what the owner hasn't pulled yet.
fn pending_line(t: &Thread) -> Option<String> {
    match t.events.len() {
        0 => None,
        1 => Some(format!("1 event waiting for {}", t.owner_name)),
        n => Some(format!("{n} events waiting for {}", t.owner_name)),
    }
}

/// Everything the thread pane shows for `t` above its input: the header
/// fields, the message, the timeline (a notify thread's records, oldest first
/// like a dispatcher's entries), then the numbered actions. Pure, so what the
/// pane says is unit-testable.
pub fn pane_lines(t: &Thread, child: Option<&ChildInfo>, utc_offset: i64) -> Vec<PaneLine> {
    let mut out = vec![line(Tone::Bold, title_of(t))];
    let mut head: PaneLine = Vec::new();
    match (t.kind, t.state, t.head()) {
        (Kind::Thread, Some(state), _) => head.push((Tone::State(state), state.as_str().to_string())),
        (Kind::Notify, _, Some(r)) => head.push((Tone::Level(r.level), r.level.as_str().to_string())),
        _ => {}
    }
    if let Some(status) = &t.status {
        head.push((Tone::Dim, "  ·  ".into()));
        head.push((Tone::Plain, status.clone()));
    }
    head.push((Tone::Dim, format!("  ·  {}", stamp(t.changed_at(), utc_offset))));
    out.push(head);
    let mut from = vec![(Tone::Dim, "from   ".to_string()), (Tone::Plain, t.owner_name.clone())];
    if t.archived {
        from.push((Tone::Dim, "  (archived: instance removed)".into()));
    }
    out.push(from);
    let link = t.link.as_ref().or_else(|| t.head().and_then(|r| r.link.as_ref()));
    if let Some(link) = link {
        out.push(vec![(Tone::Dim, "link   ".into()), (Tone::Link, link.clone()), (Tone::Dim, "  (enter)".into())]);
    }
    if let Some(c) = child {
        let mut row = vec![(Tone::Dim, "child  ".to_string())];
        match &c.name {
            Some(name) => {
                row.push((Tone::Plain, name.clone()));
                row.push((Tone::Dim, format!("  {}", c.status.as_deref().unwrap_or("not in snapshot"))));
            }
            None => row.push((Tone::Dim, format!("{} (no such child)", c.key))),
        }
        out.push(row);
    }
    let message = match t.kind {
        Kind::Thread => t.message.as_deref(),
        Kind::Notify => t.head().map(|r| r.msg.as_str()),
    };
    if let Some(message) = message {
        out.push(Vec::new());
        out.extend(message.lines().map(|l| line(Tone::Plain, l)));
    }

    let timeline: Vec<PaneLine> = match t.kind {
        Kind::Thread => t
            .entries
            .iter()
            .map(|e| {
                vec![
                    (Tone::Dim, format!("{}  {:7} ", stamp(e.at, utc_offset), entry_label(e.kind))),
                    (Tone::Plain, one_line(&e.text)),
                ]
            })
            .collect(),
        // The head is the message above; the timeline is what came before.
        Kind::Notify => t
            .notes
            .iter()
            .skip(1)
            .rev()
            .map(|n| {
                let r = &n.record;
                vec![
                    (Tone::Dim, format!("{}  ", stamp(r.at, utc_offset))),
                    (Tone::Level(r.level), format!("{:7} ", r.level.as_str())),
                    (Tone::Plain, one_line(&r.msg)),
                ]
            })
            .collect(),
    };
    if !timeline.is_empty() {
        out.push(Vec::new());
        out.push(line(Tone::Dim, match t.kind {
            Kind::Thread => "timeline",
            Kind::Notify => "earlier",
        }));
        out.extend(timeline);
    }

    if let Some(pending) = pending_line(t) {
        out.push(Vec::new());
        out.push(line(Tone::Dim, pending));
    }

    if !t.actions.is_empty() {
        out.push(Vec::new());
        for (i, a) in t.actions.iter().enumerate() {
            // Host actions run in the dashboard; the rest (and a host
            // action's `notify`/`done` half) go to the owner as an event.
            let mut row = vec![(Tone::Bold, format!("[{}] ", i + 1)), (Tone::Plain, a.label.clone())];
            if a.host.is_some() {
                row.push((Tone::Dim, "  ⌂ host".into()));
            }
            if a.enqueues_event() {
                row.push((Tone::Dim, format!("  → {}", t.owner_name)));
            }
            if a.done {
                row.push((Tone::Dim, "  ✓ done".into()));
            }
            out.push(row);
        }
    }
    // No reply hint here: the pane's input box (or its no-replies line) says it.
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
    }

    /// Ask the event loop to apply `op` to the store, and apply it here at
    /// once so the next frame shows it (the reload that follows confirms it).
    /// The cursor stays on its thread, or on its row when the thread left
    /// the view (e.g. a dismissed notify record).
    pub(super) fn request_inbox(&mut self, op: Op) {
        let selected = self.selected_inbox_id();
        // An event-enqueuing op is the store's to stamp (see the module doc).
        if !op.enqueues_event() {
            self.inbox.content.apply(&op, 0);
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
    /// process rows (they aren't fetched elsewhere).
    pub(super) fn set_tab(&mut self, tab: Tab) {
        if self.tab == Tab::Inbox && tab != Tab::Inbox {
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
            self.inbox.shown = id;
            self.inbox.scroll = 0;
            self.inbox.reply = None;
            self.inbox.focus = match (id, self.inbox.focus) {
                (None, _) => InboxFocus::List,
                (Some(_), InboxFocus::Input) => InboxFocus::Thread,
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
    fn selected_inbox_owned(&self) -> Option<Thread> {
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
        let status = name.as_ref().and_then(|name| {
            let snapshot = self.snapshot.as_ref()?;
            let row = snapshot.instances.iter().find(|r| &r.name == name)?;
            Some(row.status.label().to_string())
        });
        Some(ChildInfo { key, name, status })
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
    fn refuse_user_op(&mut self, t: &Thread) -> bool {
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
    fn open_reply(&mut self, t: &Thread) {
        if self.refuse_user_op(t) {
            return;
        }
        if t.reply.is_none() {
            self.status = Some("this thread takes no replies".into());
            return;
        }
        self.inbox.reply = Some(ReplyBox { thread: t.id, line: Prompt::new(Vec::new()) });
        self.inbox.focus = InboxFocus::Input;
    }

    /// Keys while the input has focus: `esc` goes back to the thread (the
    /// line with it), `enter` sends a non-empty reply and keeps the input,
    /// empty, for the next one (an empty `enter` does nothing), the rest
    /// edits the line as the `:` prompt does.
    fn on_key_reply(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(rb) = &mut self.inbox.reply else {
            self.inbox.focus = InboxFocus::Thread;
            return;
        };
        let line = &mut rb.line;
        match key.code {
            KeyCode::Esc => self.leave_reply(),
            KeyCode::Char('c') if ctrl => self.leave_reply(),
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
            KeyCode::Left => line.left(),
            KeyCode::Right => line.right(),
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
    /// since then the divider is what's under the mouse. The hit-test reuses
    /// `ui`'s layout functions so it lands where the divider is drawn. Never
    /// consumes the event: a press elsewhere is the caller's to route.
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
                if point_in(rect, ev.column, ev.row) && col_near(ev.column, divider) {
                    self.inbox.dragging = true;
                    self.inbox.split_pct = pct;
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.inbox.dragging => {
                self.inbox.split_pct = pct;
            }
            MouseEventKind::Up(MouseButton::Left) => self.inbox.dragging = false,
            _ => {}
        }
    }
}

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
    use crate::inbox::{Action, Reply, ThreadPut};

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
        inbox.apply(&Op::MarkRead(id), 0);
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
            inbox.apply(&Op::RemoveThread(*id), 0);
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
        put(&mut app, "d", 10, ThreadPut { reply: Some(Reply::default()), ..body("asks", State::NeedsYou) });
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
        inbox.apply(&Op::RemoveThread(thread(&app, "over").id), 0);
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
        inbox.apply(&Op::RemoveThread(asks), 0);
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
        open_asks(&mut app, Some(Reply::default()));
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

    #[test]
    fn pane_lines_show_child_timeline_actions_and_reply() {
        let mut app = new_app();
        let actions = vec![
            Action {
                id: "open".into(),
                label: "Open draft".into(),
                host: Some(crate::inbox::thread::HostVerb::Terminal(Default::default())),
                ..Action::default()
            },
            Action { id: "post".into(), label: "Post replies".into(), ..Action::default() },
            Action { id: "done".into(), label: "Done".into(), done: true, ..Action::default() },
        ];
        let full = ThreadPut {
            link: Some("https://x/pr/1".into()),
            status: Some("review".into()),
            child: Some("pr-1".into()),
            message: Some("drafts ready".into()),
            actions,
            reply: Some(Reply { placeholder: Some("next run".into()) }),
            ..body("asks", State::NeedsYou)
        };
        put(&mut app, "d", 10, full);
        app.thread_children.insert("d-id".into(), [("pr-1".to_string(), "inst0".to_string())].into());
        app.set_snapshot(snapshot_with_status(1, running()));

        let t = thread(&app, "asks").clone();
        let child = app.thread_child(&t).unwrap();
        assert_eq!(child.name.as_deref(), Some("inst0"));
        assert_eq!(child.status.as_deref(), Some("running"));
        let text: Vec<String> = pane_lines(&t, Some(&child), 0)
            .iter()
            .map(|l| l.iter().map(|(_, s)| s.as_str()).collect())
            .collect();
        let has = |want: &str| text.iter().any(|l| l.contains(want));
        assert!(has("child  inst0  running"), "{text:#?}");
        assert!(has("https://x/pr/1"));
        assert!(has("drafts ready"));
        assert!(has("timeline"));
        assert!(has("[1] Open draft  ⌂ host"), "{text:#?}");
        assert!(has("[2] Post replies  → d") && !has("[2] Post replies  ⌂"));
        // Every action is runnable now: bold number, plain label.
        let lines = pane_lines(&t, Some(&child), 0);
        let row = |n: &str| lines.iter().find(|l| l.first().is_some_and(|(_, s)| s == n)).unwrap();
        for n in ["[1] ", "[2] "] {
            assert_eq!(row(n)[..2].iter().map(|(t, _)| *t).collect::<Vec<_>>(), [Tone::Bold, Tone::Plain]);
        }
        assert!(has("[3] Done  → d  ✓ done"), "{text:#?}");
        assert!(!has("next run"), "the input shows the placeholder, not the content");
        assert!(!has("waiting for"), "no events yet");

        // A child key the owner doesn't have stays visible, unresolved.
        app.thread_children.clear();
        let child = app.thread_child(&t).unwrap();
        assert_eq!(child.name, None);
        assert!(pane_lines(&t, Some(&child), 0).iter().any(|l| l.iter().any(|(_, s)| s.contains("no such child"))));
    }

    #[test]
    fn notify_pane_shows_earlier_records() {
        let mut app = new_app();
        push(&mut app, "a", rec("v1", Some("k"), None));
        push(&mut app, "a", rec("v2", Some("k"), None));
        push(&mut app, "a", rec("v3", Some("k"), None));
        let t = thread(&app, "v3").clone();
        let text: Vec<String> =
            pane_lines(&t, None, 0).iter().map(|l| l.iter().map(|(_, s)| s.as_str()).collect()).collect();
        let at = |want: &str| text.iter().position(|l| l.ends_with(want)).unwrap();
        // The head is the message; history is oldest first under "earlier".
        assert!(at("earlier") < at("v1") && at("v1") < at("v2"), "{text:#?}");
        assert_eq!(text.iter().filter(|l| l.ends_with("v3")).count(), 2, "title and message");
    }

    /// What the event loop does with the queued ops: apply them to the store
    /// at time `now`, then reload.
    fn flush(app: &mut App, now: u64) -> Vec<Op> {
        let ops = app.take_pending_inbox();
        let mut inbox = app.inbox.content.clone();
        ops.iter().for_each(|op| inbox.apply(op, now));
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
    fn open_asks(app: &mut App, reply: Option<Reply>) -> u64 {
        put(app, "d", 10, ThreadPut { reply, ..body("asks", State::NeedsYou) });
        inbox_tab(app);
        app.on_key(key(KeyCode::Enter));
        flush(app, 10);
        thread(app, "asks").id
    }

    #[test]
    fn r_focuses_the_input_which_sends_replies_and_stays() {
        let mut app = new_app();
        let id = open_asks(&mut app, Some(Reply { placeholder: Some("next run".into()) }));
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
        let text: Vec<String> = pane_lines(thread(&app, "asks"), None, 0)
            .iter()
            .map(|l| l.iter().map(|(_, s)| s.as_str()).collect())
            .collect();
        assert!(text.iter().any(|l| l.contains("reply") && l.ends_with(">do q1")), "{text:#?}");
        assert!(text.contains(&"1 event waiting for d".to_string()), "{text:#?}");
    }

    #[test]
    fn the_reply_box_cancels_and_ignores_empty() {
        let mut app = new_app();
        open_asks(&mut app, Some(Reply::default()));
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
        open_asks(&mut app, Some(Reply::default()));
        put(&mut app, "d", 20, body("other", State::NeedsYou));
        app.on_key(key(KeyCode::Char('r')));
        typed(&mut app, "half");
        // The thread is removed under the input: the pane moves on.
        let mut inbox = app.inbox.content.clone();
        inbox.apply(&Op::RemoveThread(thread(&app, "asks").id), 0);
        app.set_inbox(inbox);
        assert_eq!(selected(&app).as_deref(), Some("other"));
        assert!(app.inbox.reply.is_none());
        assert_eq!(app.inbox.focus, InboxFocus::Thread);
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
        let text: Vec<String> =
            pane_lines(t, None, 0).iter().map(|l| l.iter().map(|(_, s)| s.as_str()).collect()).collect();
        assert!(text.iter().any(|l| l.contains("done") && l.ends_with("marked done")), "{text:#?}");
        assert!(text.iter().any(|l| l.contains("reopen") && l.ends_with("reopened")), "{text:#?}");
        assert!(text.contains(&"2 events waiting for d".to_string()), "{text:#?}");
    }

    #[test]
    fn archived_threads_take_no_user_ops() {
        let mut app = new_app();
        put(&mut app, "d", 10, ThreadPut { reply: Some(Reply::default()), ..body("asks", State::NeedsYou) });
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
