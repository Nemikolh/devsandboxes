//! Inbox tab: this dashboard's view of the shared store (`crate::inbox`,
//! `inbox.toml`). The content comes from the store — loaded at startup,
//! reloaded whenever the file changes, since any dashboard or CLI may write
//! it — and everything here is view state: selection, folds and the rows they
//! produce, all unit-testable without touching the file.
//!
//! Mutations (`d`, `D`, mark-read) are only *requested* here, as
//! [`crate::inbox::Op`]s the event loop applies through `store::update`
//! before reloading; the local copy is updated at once so the next frame
//! already shows the result. Opening a link is requested the same way
//! (`pending_open`).
//!
//! Records sharing an `(owner, key)` form a thread: the newest is the head
//! row, older ones are its foldable history. With more than one owner in the
//! inbox, threads are grouped under a foldable header per owner.

use std::collections::BTreeSet;

use crate::devsbd::notify::Record;
use crate::inbox::{is_url, Inbox, Op, Thread};

use super::{App, Tab};

/// One visible Inbox row, indexing into [`InboxView::threads`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InboxRow {
    /// Owner header (only when more than one owner has entries), carrying the
    /// owner's `instance_id`.
    Group(String),
    /// A thread's head.
    Thread(usize),
    /// `notes[note]` (≥ 1) of thread `thread`, shown while it is expanded.
    History { thread: usize, note: usize },
}

/// Row identity that survives inserts, removals and store reloads, for keeping
/// the cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
enum RowId {
    Group(String),
    Thread(u64),
    Note(u64),
}

/// The loaded Inbox plus this dashboard's fold state. Folds are per dashboard
/// and never persisted, so they key off ids that outlive a reload.
#[derive(Default)]
pub struct InboxView {
    content: Inbox,
    /// Folded owner groups, by owner id.
    collapsed: BTreeSet<String>,
    /// Threads (by id) whose history is shown.
    expanded: BTreeSet<u64>,
}

impl InboxView {
    pub fn threads(&self) -> &[Thread] {
        &self.content.threads
    }

    pub fn unread(&self) -> usize {
        self.content.unread()
    }

    /// Unread threads from owner id `owner` (group header).
    pub fn unread_for(&self, owner: &str) -> usize {
        self.content.unread_for(owner)
    }

    /// Unread threads from the instance named `name` (Instances-row badge).
    pub fn unread_for_name(&self, name: &str) -> usize {
        self.content.unread_for_name(name)
    }

    /// Records (history included) from owner id `owner`.
    pub fn count_for(&self, owner: &str) -> usize {
        self.content.count_for(owner)
    }

    /// Display name of owner id `owner`.
    pub fn owner_name<'a>(&'a self, owner: &'a str) -> &'a str {
        self.content.owner_name(owner)
    }

    /// Whether rows are grouped per owner: only once a second owner shows up,
    /// so a single sender keeps a flat list.
    pub fn grouped(&self) -> bool {
        let threads = self.threads();
        threads.iter().any(|t| t.owner != threads[0].owner)
    }

    pub fn is_collapsed(&self, owner: &str) -> bool {
        self.collapsed.contains(owner)
    }

    pub fn is_expanded(&self, thread: &Thread) -> bool {
        self.expanded.contains(&thread.id)
    }

    /// The visible rows for the current threads and fold state. Groups are
    /// ordered by their newest thread; cheap, recomputed on demand.
    pub fn rows(&self) -> Vec<InboxRow> {
        let threads = self.threads();
        let mut rows = Vec::new();
        let push_thread = |rows: &mut Vec<InboxRow>, i: usize| {
            rows.push(InboxRow::Thread(i));
            if self.expanded.contains(&threads[i].id) {
                rows.extend((1..threads[i].notes.len()).map(|note| InboxRow::History { thread: i, note }));
            }
        };
        if !self.grouped() {
            for i in 0..threads.len() {
                push_thread(&mut rows, i);
            }
            return rows;
        }
        let mut seen: Vec<&str> = Vec::new();
        for t in threads {
            if !seen.contains(&t.owner.as_str()) {
                seen.push(&t.owner);
            }
        }
        for owner in seen {
            rows.push(InboxRow::Group(owner.to_string()));
            if self.collapsed.contains(owner) {
                continue;
            }
            for (i, _) in threads.iter().enumerate().filter(|(_, t)| t.owner == owner) {
                push_thread(&mut rows, i);
            }
        }
        rows
    }

    fn row_id(&self, row: &InboxRow) -> RowId {
        let threads = self.threads();
        match row {
            InboxRow::Group(owner) => RowId::Group(owner.clone()),
            InboxRow::Thread(i) => RowId::Thread(threads[*i].id),
            InboxRow::History { thread, note } => RowId::Note(threads[*thread].notes[*note].id),
        }
    }
}

impl App {
    /// Install Inbox content loaded from the store (startup and every reload),
    /// keeping the cursor on the row it was on. A cursor on the top row stays
    /// on top (the newest); one moved down stays on its row, wherever that
    /// moved to.
    pub fn set_inbox(&mut self, mut inbox: Inbox) {
        let selected = match self.selected[Tab::Inbox.index()] {
            0 => None,
            _ => self.selected_inbox_row_id(),
        };
        // Archived threads (their instance is gone) stay in the store as
        // history but are out of the live list; step 4 adds the All view that
        // shows them. Dropping them from the view copy keeps every row index
        // and every `Op` id consistent with what is on screen.
        inbox.threads.retain(|t| !t.archived);
        self.inbox.content = inbox;
        self.reselect_inbox(selected);
        // A record arriving while the Inbox is shown counts as seen.
        self.mark_inbox_read_if_shown();
    }

    /// Ask the event loop to apply `op` to the store, and apply it here at
    /// once so the next frame shows it (the reload that follows confirms it).
    fn request_inbox(&mut self, op: Op) {
        self.inbox.content.apply(&op);
        self.pending_inbox.push(op);
    }

    /// Take the mutations the event loop owes the store.
    pub fn take_pending_inbox(&mut self) -> Vec<Op> {
        std::mem::take(&mut self.pending_inbox)
    }

    fn mark_inbox_read_if_shown(&mut self) {
        if self.tab == Tab::Inbox && self.inbox.unread() > 0 {
            self.request_inbox(Op::MarkAllRead);
        }
    }

    /// Switch tabs; entering the Inbox marks everything read, entering
    /// Instances refreshes the process rows (they aren't fetched elsewhere).
    pub(super) fn set_tab(&mut self, tab: Tab) {
        if tab == Tab::Instances && self.tab != tab {
            self.needs_proc_fetch = true;
        }
        self.tab = tab;
        self.mark_inbox_read_if_shown();
    }

    /// Tab-bar title, carrying the unread count on the Inbox.
    pub fn tab_title(&self, tab: Tab) -> String {
        match (tab, self.inbox.unread()) {
            (Tab::Inbox, n) if n > 0 => format!("{} ({n})", tab.title()),
            _ => tab.title().to_string(),
        }
    }

    /// The row under the Inbox cursor, if any.
    pub fn selected_inbox_row(&self) -> Option<InboxRow> {
        self.inbox.rows().into_iter().nth(self.selected[Tab::Inbox.index()])
    }

    /// The record under the Inbox cursor (a head or a history row) with its
    /// thread; `None` on a group header.
    pub fn selected_inbox_record(&self) -> Option<(&Thread, &Record)> {
        match self.selected_inbox_row()? {
            InboxRow::Group(_) => None,
            // A dispatcher thread has no records at all; its fields and
            // timeline are read off the `Thread` itself.
            InboxRow::Thread(i) => self.inbox.threads().get(i).and_then(|t| Some((t, t.head()?))),
            InboxRow::History { thread, note } => {
                let t = self.inbox.threads().get(thread)?;
                Some((t, &t.notes.get(note)?.record))
            }
        }
    }

    /// The thread under the Inbox cursor (a head row), whatever its kind.
    pub fn selected_inbox_thread(&self) -> Option<&Thread> {
        match self.selected_inbox_row()? {
            InboxRow::Thread(i) => self.inbox.threads().get(i),
            _ => None,
        }
    }

    fn selected_inbox_row_id(&self) -> Option<RowId> {
        self.selected_inbox_row().map(|r| self.inbox.row_id(&r))
    }

    /// Put the cursor back on row `id` if it is still visible, else re-clamp.
    fn reselect_inbox(&mut self, id: Option<RowId>) {
        if let Some(id) = id {
            if let Some(pos) = self.inbox.rows().iter().position(|r| self.inbox.row_id(r) == id) {
                self.selected[Tab::Inbox.index()] = pos;
            }
        }
        self.clamp_selection();
    }

    /// Index of the visible row with identity `id`.
    fn inbox_row_position(&self, id: &RowId) -> Option<usize> {
        self.inbox.rows().iter().position(|r| self.inbox.row_id(r) == *id)
    }

    /// `→` (Inbox tab): unfold the group or thread under the cursor.
    pub(super) fn inbox_expand(&mut self) {
        match self.selected_inbox_row() {
            Some(InboxRow::Group(owner)) => {
                self.inbox.collapsed.remove(&owner);
            }
            Some(InboxRow::Thread(i)) if self.inbox.threads()[i].history() > 0 => {
                self.inbox.expanded.insert(self.inbox.threads()[i].id);
            }
            _ => {}
        }
        self.clamp_selection();
    }

    /// `space` (Inbox tab): toggle the group or thread under the cursor.
    pub(super) fn inbox_toggle(&mut self) {
        match self.selected_inbox_row() {
            Some(InboxRow::Group(owner)) => {
                if !self.inbox.collapsed.remove(&owner) {
                    self.inbox.collapsed.insert(owner);
                }
            }
            Some(InboxRow::Thread(i)) if self.inbox.threads()[i].history() > 0 => {
                let id = self.inbox.threads()[i].id;
                if !self.inbox.expanded.remove(&id) {
                    self.inbox.expanded.insert(id);
                }
            }
            _ => {}
        }
        self.clamp_selection();
    }

    /// `←` (Inbox tab): fold one level. A group folds; an expanded thread
    /// folds; otherwise jump to the parent (thread for history, group for a
    /// thread when grouped).
    pub(super) fn inbox_collapse(&mut self) {
        let slot = Tab::Inbox.index();
        match self.selected_inbox_row() {
            Some(InboxRow::Group(owner)) => {
                self.inbox.collapsed.insert(owner);
            }
            Some(InboxRow::Thread(i)) => {
                let t = &self.inbox.threads()[i];
                let (id, owner) = (t.id, t.owner.clone());
                if !self.inbox.expanded.remove(&id) && self.inbox.grouped() {
                    if let Some(pos) = self.inbox_row_position(&RowId::Group(owner)) {
                        self.selected[slot] = pos;
                    }
                }
            }
            Some(InboxRow::History { thread, .. }) => {
                let id = RowId::Thread(self.inbox.threads()[thread].id);
                if let Some(pos) = self.inbox_row_position(&id) {
                    self.selected[slot] = pos;
                }
            }
            None => {}
        }
        self.clamp_selection();
    }

    /// `d` (Inbox tab): dismiss what's under the cursor: a history record, a
    /// whole thread (on its head), or an owner's entries (on its header).
    pub(super) fn dismiss_selected_notification(&mut self) {
        let op = match self.selected_inbox_row() {
            Some(InboxRow::Group(owner)) => {
                self.inbox.collapsed.remove(&owner);
                Op::RemoveOwner(owner)
            }
            Some(InboxRow::Thread(i)) => {
                let id = self.inbox.threads()[i].id;
                self.inbox.expanded.remove(&id);
                Op::RemoveThread(id)
            }
            Some(InboxRow::History { thread, note }) => {
                Op::RemoveNote(self.inbox.threads()[thread].notes[note].id)
            }
            None => return,
        };
        self.request_inbox(op);
        self.clamp_selection();
    }

    /// `D` (Inbox tab): clear the inbox.
    pub(super) fn clear_notifications(&mut self) {
        self.request_inbox(Op::Clear);
        self.inbox.collapsed.clear();
        self.inbox.expanded.clear();
        self.clamp_selection();
    }

    /// `enter` (Inbox tab): ask the event loop to open the selected record's
    /// link. No link → no-op; a non-URL link → status line, nothing opened.
    pub(super) fn open_selected_link(&mut self) {
        let Some(link) = self.selected_inbox_record().and_then(|(_, r)| r.link.clone()) else {
            return;
        };
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
}

/// Past this age the Inbox switches from "N days ago" to a date.
const RELATIVE_FOR: u64 = 7 * 86_400;

/// The Inbox TIME column: how long before `now` unix time `at` was ("just
/// now", "12s ago", "5 min ago", "3h ago", "6 days ago"), then [`stamp`] once it's a week
/// old. A timestamp ahead of `now` (container clock skew) reads "just now".
pub fn when(at: u64, now: u64, utc_offset: i64) -> String {
    let age = now.saturating_sub(at);
    match age {
        0 => "just now".into(),
        a if a < 60 => format!("{a}s ago"),
        a if a < 3600 => format!("{} min ago", a / 60),
        a if a < 86_400 => format!("{}h ago", a / 3600),
        a if a < 2 * 86_400 => "1 day ago".into(),
        a if a < RELATIVE_FOR => format!("{} days ago", a / 86_400),
        _ => stamp(at, utc_offset),
    }
}

/// `Oct 2 14:32` of unix time `at`, shifted by `utc_offset` seconds (no year:
/// the Inbox only holds recent notifications).
pub fn stamp(at: u64, utc_offset: i64) -> String {
    const MONTHS: [&str; 12] =
        ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
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
    use crate::devsbd::notify::Level;
    use crossterm::event::KeyCode;

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

    /// Thread heads, newest first.
    fn msgs(app: &App) -> Vec<(&str, &str)> {
        app.inbox
            .threads()
            .iter()
            .map(|t| (t.owner_name.as_str(), t.head().unwrap().msg.as_str()))
            .collect()
    }

    /// Visible rows rendered as short strings: `[inst]`, `msg`, `  msg`.
    fn rows(app: &App) -> Vec<String> {
        app.inbox
            .rows()
            .iter()
            .map(|r| match r {
                InboxRow::Group(owner) => format!("[{}]", app.inbox.owner_name(owner)),
                InboxRow::Thread(i) => app.inbox.threads()[*i].head().unwrap().msg.clone(),
                InboxRow::History { thread, note } => {
                    format!("  {}", app.inbox.threads()[*thread].notes[*note].record.msg)
                }
            })
            .collect()
    }

    fn selected_msg(app: &App) -> Option<String> {
        app.selected_inbox_record().map(|(_, r)| r.msg.clone())
    }

    #[test]
    fn flat_with_one_instance_grouped_with_two() {
        let mut app = new_app();
        push(&mut app, "a", rec("a1", None, None));
        push(&mut app, "a", rec("a2", None, None));
        assert!(!app.inbox.grouped());
        assert_eq!(rows(&app), ["a2", "a1"]);
        push(&mut app, "b", rec("b1", None, None));
        push(&mut app, "a", rec("a3", None, None));
        // Groups ordered by their newest thread.
        assert_eq!(rows(&app), ["[a]", "a3", "a2", "a1", "[b]", "b1"]);
    }

    #[test]
    fn fold_groups_and_threads() {
        let mut app = new_app();
        push(&mut app, "b", rec("b1", None, None));
        push(&mut app, "a", rec("v1", Some("k"), None));
        push(&mut app, "a", rec("v2", Some("k"), None));
        push(&mut app, "a", rec("v3", Some("k"), None));
        app.on_key(key(KeyCode::Char('4')));
        assert_eq!(rows(&app), ["[a]", "v3", "[b]", "b1"]);

        app.on_key(key(KeyCode::Down)); // v3
        app.on_key(key(KeyCode::Right));
        assert_eq!(rows(&app), ["[a]", "v3", "  v2", "  v1", "[b]", "b1"]);
        app.on_key(key(KeyCode::Down)); // v2 (history)
        assert_eq!(selected_msg(&app).as_deref(), Some("v2"));
        app.on_key(key(KeyCode::Left)); // history → its head
        assert_eq!(app.selected(), 1);
        app.on_key(key(KeyCode::Left)); // expanded thread folds
        assert_eq!(rows(&app), ["[a]", "v3", "[b]", "b1"]);
        app.on_key(key(KeyCode::Left)); // folded thread → its group
        assert_eq!(app.selected(), 0);
        app.on_key(key(KeyCode::Char(' '))); // fold the group
        assert_eq!(rows(&app), ["[a]", "[b]", "b1"]);
        app.on_key(key(KeyCode::Right));
        assert_eq!(rows(&app), ["[a]", "v3", "[b]", "b1"]);
        // A thread without history doesn't expand.
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Down)); // b1
        app.on_key(key(KeyCode::Char(' ')));
        assert_eq!(rows(&app), ["[a]", "v3", "[b]", "b1"]);
    }

    #[test]
    fn unread_count_and_read_on_entering_tab() {
        let mut app = new_app();
        push(&mut app, "a", rec("one", None, None));
        push(&mut app, "a", rec("two", None, None));
        assert_eq!(app.inbox.unread(), 2);
        assert_eq!(app.tab_title(Tab::Inbox), "Inbox (2)");
        assert_eq!(app.tab_title(Tab::Ports), "Ports");

        app.on_key(key(KeyCode::Char('4')));
        assert_eq!(app.tab, Tab::Inbox);
        assert_eq!(app.inbox.unread(), 0);
        assert_eq!(app.tab_title(Tab::Inbox), "Inbox");
        assert_eq!(app.take_pending_inbox(), [Op::MarkAllRead]);

        // Arriving while the Inbox is shown counts as seen.
        push(&mut app, "a", rec("three", None, None));
        assert_eq!(app.inbox.unread(), 0);
        assert_eq!(app.take_pending_inbox(), [Op::MarkAllRead]);

        // Tab-cycling into the Inbox marks read too.
        app.on_key(key(KeyCode::Char('1')));
        push(&mut app, "a", rec("four", None, None));
        app.on_key(key(KeyCode::BackTab)); // Instances → Inbox (wraps)
        assert_eq!(app.tab, Tab::Inbox);
        assert_eq!(app.inbox.unread(), 0);
    }

    #[test]
    fn badge_count_per_instance() {
        let mut app = new_app();
        push(&mut app, "a", rec("1", None, None));
        push(&mut app, "a", rec("2", None, None));
        push(&mut app, "a", rec("3", Some("k"), None));
        push(&mut app, "a", rec("4", Some("k"), None));
        push(&mut app, "b", rec("5", None, None));
        // A thread counts once, however long its history.
        assert_eq!(app.inbox.unread_for_name("a"), 3);
        assert_eq!(app.inbox.unread_for_name("b"), 1);
        assert_eq!(app.inbox.unread_for_name("c"), 0);
        assert_eq!(app.inbox.unread_for("a-id"), 3);
        app.set_tab(Tab::Inbox);
        assert_eq!(app.inbox.unread_for_name("a"), 0);
    }

    #[test]
    fn dismiss_and_clear() {
        let mut app = new_app();
        for m in ["1", "2", "3"] {
            push(&mut app, "a", rec(m, None, None));
        }
        app.on_key(key(KeyCode::Char('4')));
        app.on_key(key(KeyCode::Down)); // "2"
        app.on_key(key(KeyCode::Char('d')));
        assert_eq!(msgs(&app), [("a", "3"), ("a", "1")]);
        assert_eq!(app.selected(), 1);
        app.on_key(key(KeyCode::Char('d'))); // last row: selection re-clamps
        assert_eq!(msgs(&app), [("a", "3")]);
        assert_eq!(app.selected(), 0);

        push(&mut app, "a", rec("4", None, None));
        app.on_key(key(KeyCode::Char('D')));
        assert!(app.inbox.threads().is_empty());
        assert_eq!(app.selected(), 0);
        // Empty inbox: `d` is a no-op, not a panic.
        app.on_key(key(KeyCode::Char('d')));
    }

    #[test]
    fn dismiss_history_thread_and_group() {
        let mut app = new_app();
        push(&mut app, "b", rec("b1", None, None));
        push(&mut app, "b", rec("b2", None, None));
        for v in ["v1", "v2", "v3"] {
            push(&mut app, "a", rec(v, Some("k"), None));
        }
        app.on_key(key(KeyCode::Char('4')));
        app.on_key(key(KeyCode::Down)); // v3
        app.on_key(key(KeyCode::Right));
        app.on_key(key(KeyCode::Down)); // v2
        app.on_key(key(KeyCode::Char('d')));
        assert_eq!(rows(&app), ["[a]", "v3", "  v1", "[b]", "b2", "b1"]);
        app.on_key(key(KeyCode::Up)); // v3: the whole thread goes
        app.on_key(key(KeyCode::Char('d')));
        // `a` is gone, so only `b` is left: flat again.
        assert_eq!(rows(&app), ["b2", "b1"]);

        push(&mut app, "a", rec("a1", None, None));
        assert_eq!(rows(&app), ["[a]", "a1", "[b]", "b2", "b1"]);
        assert_eq!(app.selected(), 4); // the cursor followed "b1"
        app.on_key(key(KeyCode::Up));
        app.on_key(key(KeyCode::Up));
        assert_eq!(app.selected_inbox_row(), Some(InboxRow::Group("b-id".into())));
        app.on_key(key(KeyCode::Char('d'))); // header: all of `b`
        assert_eq!(rows(&app), ["a1"]);
    }

    /// Every key mutation is requested by id for the store to apply; the
    /// local copy is updated at once so the view doesn't lag a tick.
    #[test]
    fn key_mutations_are_requested_not_saved() {
        let mut app = new_app();
        push(&mut app, "a", rec("v1", Some("k"), None));
        push(&mut app, "a", rec("v2", Some("k"), None));
        push(&mut app, "a", rec("other", None, None));
        let thread = app.inbox.threads()[1].id; // the keyed thread
        let history = app.inbox.threads()[1].notes[1].id;
        app.on_key(key(KeyCode::Char('4'))); // marks read
        assert_eq!(app.take_pending_inbox(), [Op::MarkAllRead]);
        app.on_key(key(KeyCode::Char('1')));
        app.on_key(key(KeyCode::Char('4'))); // nothing unread: no request
        assert_eq!(app.take_pending_inbox(), []);

        app.on_key(key(KeyCode::Down)); // the keyed thread's head
        app.on_key(key(KeyCode::Right));
        app.on_key(key(KeyCode::Down)); // its history
        app.on_key(key(KeyCode::Char('d')));
        app.on_key(key(KeyCode::Char('d'))); // re-clamped onto the head
        app.on_key(key(KeyCode::Char('D')));
        assert_eq!(
            app.take_pending_inbox(),
            [Op::RemoveNote(history), Op::RemoveThread(thread), Op::Clear]
        );
        assert!(app.inbox.threads().is_empty());
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
    fn selection_follows_row_on_push() {
        let mut app = new_app();
        push(&mut app, "a", rec("1", None, None));
        push(&mut app, "a", rec("2", Some("k"), None));
        app.on_key(key(KeyCode::Char('4')));
        app.on_key(key(KeyCode::Down)); // "1"
        push(&mut app, "a", rec("3", None, None));
        assert_eq!(selected_msg(&app).as_deref(), Some("1"));
        // A thread moving from above the cursor to the top keeps it on "1".
        push(&mut app, "a", rec("2b", Some("k"), None));
        assert_eq!(selected_msg(&app).as_deref(), Some("1"));
        // Switching to grouped (a second instance) keeps it on "1" too.
        push(&mut app, "b", rec("b", None, None));
        assert_eq!(selected_msg(&app).as_deref(), Some("1"));
        // A cursor on the top row stays on top.
        while app.selected() > 0 {
            app.on_key(key(KeyCode::Up));
        }
        push(&mut app, "c", rec("4", None, None));
        assert_eq!(app.selected(), 0);
        assert_eq!(app.selected_inbox_row(), Some(InboxRow::Group("c-id".into())));
    }

    /// A reload (another dashboard changed the store) keeps the cursor on the
    /// same thread, wherever it moved to, and keeps the folds.
    #[test]
    fn reload_preserves_selection_by_thread_id() {
        let mut app = new_app();
        push(&mut app, "a", rec("v1", Some("k"), None));
        push(&mut app, "a", rec("v2", Some("k"), None));
        push(&mut app, "a", rec("other", None, None));
        app.on_key(key(KeyCode::Char('4')));
        app.on_key(key(KeyCode::Down)); // the keyed thread, now second
        app.on_key(key(KeyCode::Right)); // expanded
        let id = app.inbox.threads()[1].id;
        assert_eq!(rows(&app), ["other", "v2", "  v1"]);

        // Another writer dismissed "other" and added a record to the thread,
        // moving it to the top.
        let mut store = app.inbox.content.clone();
        store.apply(&Op::RemoveThread(store.threads[0].id));
        store.push("a-id".into(), "a".into(), rec("v3", Some("k"), None), true);
        store.push("b-id".into(), "b".into(), rec("b1", None, None), true);
        app.set_inbox(store);

        assert_eq!(app.inbox.threads()[1].id, id, "same thread, same id");
        assert_eq!(rows(&app), ["[b]", "b1", "[a]", "v3", "  v2", "  v1"]);
        assert_eq!(selected_msg(&app).as_deref(), Some("v3"), "cursor followed the thread");
        assert_eq!(app.selected(), 3);
    }

    #[test]
    fn enter_opens_link_only_when_present() {
        let mut app = new_app();
        push(&mut app, "a", rec("no link", None, None));
        push(&mut app, "a", rec("pr", None, Some("https://x/pr/1")));
        app.on_key(key(KeyCode::Char('4')));
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.take_pending_open().as_deref(), Some("https://x/pr/1"));
        assert!(matches!(app.modal, super::super::Modal::None));

        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.take_pending_open(), None);
    }

    #[test]
    fn enter_on_history_opens_its_own_link() {
        let mut app = new_app();
        push(&mut app, "a", rec("v1", Some("k"), Some("https://x/1")));
        push(&mut app, "a", rec("v2", Some("k"), Some("https://x/2")));
        app.on_key(key(KeyCode::Char('4')));
        app.on_key(key(KeyCode::Right));
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.take_pending_open().as_deref(), Some("https://x/1"));
    }

    #[test]
    fn enter_refuses_non_url_links() {
        let mut app = new_app();
        push(&mut app, "a", rec("x", None, Some("--help")));
        app.on_key(key(KeyCode::Char('4')));
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.take_pending_open(), None);
        assert!(app.status.as_deref().unwrap().starts_with("not a URL"));
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
    fn relative_time_then_date() {
        let now = 1_000_000_000;
        assert_eq!(when(now, now, 0), "just now");
        assert_eq!(when(now + 5, now, 0), "just now", "skewed clock");
        assert_eq!(when(now - 1, now, 0), "1s ago");
        assert_eq!(when(now - 59, now, 0), "59s ago");
        assert_eq!(when(now - 60, now, 0), "1 min ago");
        assert_eq!(when(now - 3599, now, 0), "59 min ago");
        assert_eq!(when(now - 2 * 3600, now, 0), "2h ago");
        assert_eq!(when(now - 86_400, now, 0), "1 day ago");
        assert_eq!(when(now - 2 * 86_400, now, 0), "2 days ago");
        assert_eq!(when(now - (7 * 86_400 - 1), now, 0), "6 days ago");
        // 1_000_000_000 is 2001-09-09 01:46:40 UTC.
        assert_eq!(when(now - 7 * 86_400, now, 0), "Sep 2 01:46");
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
