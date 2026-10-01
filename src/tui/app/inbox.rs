//! Inbox tab: container notifications (`devsbd notify`, docs/automations.md),
//! newest first. The event loop pushes what the bridge worker drains and saves
//! the inbox to `inbox.toml` (next to `state.toml`) whenever it is
//! [`Inbox::take_dirty`]; everything here is plain state so threading, cap,
//! tree, unread and selection rules stay unit-testable. Opening a link is only
//! *requested* here (`pending_open`); the loop spawns the opener.
//!
//! Records sharing an `(instance, key)` form a thread: the newest is the head
//! row, older ones are its foldable history. With more than one instance in
//! the inbox, threads are grouped under a foldable header per instance.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::devsbd::notify::{Level, Record};

use super::{App, Tab};

/// Records kept per instance, thread history included; the instance's oldest
/// record drops off beyond this. Per instance only, so a noisy container
/// never evicts another's.
pub const INBOX_INSTANCE_CAP: usize = 200;

/// One received record. `id` grows with arrival order across the whole inbox
/// (persisted), so "oldest" is well defined across threads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    pub id: u64,
    pub record: Record,
}

/// A keyed record's history, or a single unkeyed record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Thread {
    /// Stable id, so selection and fold state can follow the thread.
    pub id: u64,
    /// Instance (state key) the notifications came from.
    pub instance: String,
    /// Newest first, never empty: `notes[0]` is the head.
    pub notes: Vec<Note>,
    /// Only the head counts as unread; history is a trace, not news.
    pub unread: bool,
}

impl Thread {
    pub fn head(&self) -> &Record {
        &self.notes[0].record
    }

    /// Older records under the head.
    pub fn history(&self) -> usize {
        self.notes.len() - 1
    }
}

/// One visible Inbox row, indexing into [`Inbox::threads`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InboxRow {
    /// Instance header (only when more than one instance has entries).
    Group(String),
    /// A thread's head.
    Thread(usize),
    /// `notes[note]` (≥ 1) of thread `thread`, shown while it is expanded.
    History { thread: usize, note: usize },
}

/// Row identity that survives inserts and removals, for keeping the cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
enum RowId {
    Group(String),
    Thread(u64),
    Note(u64),
}

#[derive(Default)]
pub struct Inbox {
    /// Ordered by head, newest first.
    pub threads: Vec<Thread>,
    next_id: u64,
    /// Folded instance groups. Not persisted.
    collapsed: BTreeSet<String>,
    /// Threads (by id) whose history is shown. Not persisted.
    expanded: BTreeSet<u64>,
    /// Content changed since the event loop last saved.
    dirty: bool,
}

impl Inbox {
    fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Insert at the top. A keyed record joins the thread with the same
    /// `(instance, key)` as its new head, moving it to the top with `unread`.
    /// Keys are per instance, since two sandboxes can't know each other's.
    fn push(&mut self, instance: String, record: Record, unread: bool) {
        let note = Note { id: self.next_id(), record };
        let existing = note.record.key.as_ref().and_then(|key| {
            self.threads
                .iter()
                .position(|t| t.instance == instance && t.head().key.as_ref() == Some(key))
        });
        let thread = match existing {
            Some(pos) => {
                let mut t = self.threads.remove(pos);
                t.notes.insert(0, note);
                t.unread = unread;
                t
            }
            None => Thread { id: self.next_id(), instance, notes: vec![note], unread },
        };
        let instance = thread.instance.clone();
        self.threads.insert(0, thread);
        self.enforce_cap(&instance);
        self.dirty = true;
    }

    /// Drop `instance`'s oldest records until it is within the cap. The oldest
    /// is always some thread's last note (a head is newer than its history).
    fn enforce_cap(&mut self, instance: &str) {
        loop {
            let mine = self.threads.iter().filter(|t| t.instance == instance);
            if mine.clone().map(|t| t.notes.len()).sum::<usize>() <= INBOX_INSTANCE_CAP {
                return;
            }
            let Some(pos) = self
                .threads
                .iter()
                .enumerate()
                .filter(|(_, t)| t.instance == instance)
                .min_by_key(|(_, t)| t.notes.last().map_or(u64::MAX, |n| n.id))
                .map(|(i, _)| i)
            else {
                return;
            };
            self.threads[pos].notes.pop();
            if self.threads[pos].notes.is_empty() {
                let t = self.threads.remove(pos);
                self.expanded.remove(&t.id);
            }
        }
    }

    pub fn unread(&self) -> usize {
        self.threads.iter().filter(|t| t.unread).count()
    }

    /// Unread threads from `instance`, for the Instances-row badge and the
    /// group header.
    pub fn unread_for(&self, instance: &str) -> usize {
        self.threads
            .iter()
            .filter(|t| t.unread && t.instance == instance)
            .count()
    }

    /// Records (history included) from `instance`.
    pub fn count_for(&self, instance: &str) -> usize {
        self.threads
            .iter()
            .filter(|t| t.instance == instance)
            .map(|t| t.notes.len())
            .sum()
    }

    fn mark_all_read(&mut self) {
        for t in &mut self.threads {
            if t.unread {
                t.unread = false;
                self.dirty = true;
            }
        }
    }

    /// Whether rows are grouped per instance: only once a second instance
    /// shows up, so a single sender keeps a flat list.
    pub fn grouped(&self) -> bool {
        self.threads.iter().any(|t| t.instance != self.threads[0].instance)
    }

    pub fn is_collapsed(&self, instance: &str) -> bool {
        self.collapsed.contains(instance)
    }

    pub fn is_expanded(&self, thread: &Thread) -> bool {
        self.expanded.contains(&thread.id)
    }

    /// The visible rows for the current threads and fold state. Groups are
    /// ordered by their newest thread; cheap, recomputed on demand.
    pub fn rows(&self) -> Vec<InboxRow> {
        let mut rows = Vec::new();
        let push_thread = |rows: &mut Vec<InboxRow>, i: usize| {
            rows.push(InboxRow::Thread(i));
            if self.expanded.contains(&self.threads[i].id) {
                rows.extend((1..self.threads[i].notes.len()).map(|note| InboxRow::History { thread: i, note }));
            }
        };
        if !self.grouped() {
            for i in 0..self.threads.len() {
                push_thread(&mut rows, i);
            }
            return rows;
        }
        let mut seen: Vec<&str> = Vec::new();
        for t in &self.threads {
            if !seen.contains(&t.instance.as_str()) {
                seen.push(&t.instance);
            }
        }
        for instance in seen {
            rows.push(InboxRow::Group(instance.to_string()));
            if self.collapsed.contains(instance) {
                continue;
            }
            for (i, _) in self.threads.iter().enumerate().filter(|(_, t)| t.instance == instance) {
                push_thread(&mut rows, i);
            }
        }
        rows
    }

    fn row_id(&self, row: &InboxRow) -> RowId {
        match row {
            InboxRow::Group(name) => RowId::Group(name.clone()),
            InboxRow::Thread(i) => RowId::Thread(self.threads[*i].id),
            InboxRow::History { thread, note } => RowId::Note(self.threads[*thread].notes[*note].id),
        }
    }

    /// Take the "needs saving" flag.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// The `inbox.toml` contents.
    pub fn to_toml(&self) -> Result<String, String> {
        let saved = SavedInbox {
            threads: self
                .threads
                .iter()
                .map(|t| SavedThread {
                    instance: t.instance.clone(),
                    unread: t.unread,
                    notes: t
                        .notes
                        .iter()
                        .map(|n| SavedNote {
                            seq: n.id,
                            level: n.record.level.as_str().to_string(),
                            at: n.record.at,
                            key: n.record.key.clone(),
                            link: n.record.link.clone(),
                            msg: n.record.msg.clone(),
                        })
                        .collect(),
                })
                .collect(),
        };
        toml::to_string_pretty(&saved).map_err(|e| e.to_string())
    }

    /// Parse `inbox.toml` contents. Thread ids are fresh; note ids keep the
    /// saved arrival order.
    pub fn from_toml(text: &str) -> Result<Inbox, String> {
        let saved: SavedInbox = toml::from_str(text).map_err(|e| e.to_string())?;
        let mut inbox = Inbox {
            next_id: saved.threads.iter().flat_map(|t| &t.notes).map(|n| n.seq + 1).max().unwrap_or(0),
            ..Inbox::default()
        };
        for t in saved.threads {
            let notes = t
                .notes
                .into_iter()
                .map(|n| {
                    let level = Level::parse(&n.level).ok_or_else(|| format!("bad level `{}`", n.level))?;
                    Ok(Note {
                        id: n.seq,
                        record: Record { level, key: n.key, link: n.link, msg: n.msg, at: n.at },
                    })
                })
                .collect::<Result<Vec<_>, String>>()?;
            if notes.is_empty() {
                continue;
            }
            let id = inbox.next_id();
            inbox.threads.push(Thread { id, instance: t.instance, notes, unread: t.unread });
        }
        Ok(inbox)
    }
}

/// `inbox.toml` schema: threads newest first, each with its notes newest first.
#[derive(Serialize, Deserialize)]
struct SavedInbox {
    #[serde(default, rename = "thread", skip_serializing_if = "Vec::is_empty")]
    threads: Vec<SavedThread>,
}

#[derive(Serialize, Deserialize)]
struct SavedThread {
    instance: String,
    #[serde(default)]
    unread: bool,
    #[serde(default, rename = "note")]
    notes: Vec<SavedNote>,
}

#[derive(Serialize, Deserialize)]
struct SavedNote {
    /// Arrival order across the inbox (the cap drops the lowest first).
    seq: u64,
    level: String,
    at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    link: Option<String>,
    msg: String,
}

/// Whether `link` is an `http(s)://` URL. The link comes from inside the
/// container and ends up as the opener's argv, so only web links pass: no
/// bare paths, no leading `-` (an option to `xdg-open`/`open`), and no
/// `file:` or custom schemes that would hand the container a host handler.
fn is_url(link: &str) -> bool {
    ["http://", "https://"]
        .iter()
        .any(|p| link.len() > p.len() && link[..p.len()].eq_ignore_ascii_case(p))
}

impl App {
    /// Take one drained notification. Arriving while the Inbox tab is shown
    /// counts as seen; elsewhere it's unread (tab title + instance badge).
    /// A cursor on the top row stays on top (the newest); one moved down
    /// stays on the row it was on.
    #[cfg_attr(not(unix), allow(dead_code))] // fed by the unix-only bridge worker
    pub fn push_notification(&mut self, instance: String, record: Record) {
        let selected = match self.selected[Tab::Inbox.index()] {
            0 => None,
            _ => self.selected_inbox_row_id(),
        };
        let unread = self.tab != Tab::Inbox;
        self.inbox.push(instance, record, unread);
        self.reselect_inbox(selected);
    }

    /// Install an inbox loaded from disk (startup).
    pub fn set_inbox(&mut self, inbox: Inbox) {
        self.inbox = inbox;
        self.clamp_selection();
    }

    /// Switch tabs; entering the Inbox marks everything read, entering
    /// Instances refreshes the process rows (they aren't fetched elsewhere).
    pub(super) fn set_tab(&mut self, tab: Tab) {
        if tab == Tab::Instances && self.tab != tab {
            self.needs_proc_fetch = true;
        }
        self.tab = tab;
        if tab == Tab::Inbox {
            self.inbox.mark_all_read();
        }
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
            InboxRow::Thread(i) => self.inbox.threads.get(i).map(|t| (t, t.head())),
            InboxRow::History { thread, note } => {
                let t = self.inbox.threads.get(thread)?;
                Some((t, &t.notes.get(note)?.record))
            }
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
            Some(InboxRow::Group(name)) => {
                self.inbox.collapsed.remove(&name);
            }
            Some(InboxRow::Thread(i)) if self.inbox.threads[i].history() > 0 => {
                self.inbox.expanded.insert(self.inbox.threads[i].id);
            }
            _ => {}
        }
        self.clamp_selection();
    }

    /// `space` (Inbox tab): toggle the group or thread under the cursor.
    pub(super) fn inbox_toggle(&mut self) {
        match self.selected_inbox_row() {
            Some(InboxRow::Group(name)) => {
                if !self.inbox.collapsed.remove(&name) {
                    self.inbox.collapsed.insert(name);
                }
            }
            Some(InboxRow::Thread(i)) if self.inbox.threads[i].history() > 0 => {
                let id = self.inbox.threads[i].id;
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
            Some(InboxRow::Group(name)) => {
                self.inbox.collapsed.insert(name);
            }
            Some(InboxRow::Thread(i)) => {
                let t = &self.inbox.threads[i];
                if !self.inbox.expanded.remove(&t.id) && self.inbox.grouped() {
                    let group = RowId::Group(t.instance.clone());
                    if let Some(pos) = self.inbox_row_position(&group) {
                        self.selected[slot] = pos;
                    }
                }
            }
            Some(InboxRow::History { thread, .. }) => {
                let id = RowId::Thread(self.inbox.threads[thread].id);
                if let Some(pos) = self.inbox_row_position(&id) {
                    self.selected[slot] = pos;
                }
            }
            None => {}
        }
        self.clamp_selection();
    }

    /// `d` (Inbox tab): dismiss what's under the cursor: a history record, a
    /// whole thread (on its head), or an instance's entries (on its header).
    pub(super) fn dismiss_selected_notification(&mut self) {
        let row = self.selected_inbox_row();
        let inbox = &mut self.inbox;
        match row {
            Some(InboxRow::Group(name)) => {
                inbox.threads.retain(|t| t.instance != name);
                inbox.collapsed.remove(&name);
            }
            Some(InboxRow::Thread(i)) => {
                let t = inbox.threads.remove(i);
                inbox.expanded.remove(&t.id);
            }
            Some(InboxRow::History { thread, note }) => {
                inbox.threads[thread].notes.remove(note);
            }
            None => return,
        }
        self.inbox.dirty = true;
        self.clamp_selection();
    }

    /// `D` (Inbox tab): clear the inbox.
    pub(super) fn clear_notifications(&mut self) {
        self.inbox.threads.clear();
        self.inbox.collapsed.clear();
        self.inbox.expanded.clear();
        self.inbox.dirty = true;
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

/// `HH:MM` of unix time `at`, shifted by `utc_offset` seconds.
pub fn clock(at: u64, utc_offset: i64) -> String {
    let secs = (at as i64 + utc_offset).rem_euclid(86_400);
    format!("{:02}:{:02}", secs / 3600, secs % 3600 / 60)
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

    /// Thread heads, newest first.
    fn msgs(app: &App) -> Vec<(&str, &str)> {
        app.inbox
            .threads
            .iter()
            .map(|t| (t.instance.as_str(), t.head().msg.as_str()))
            .collect()
    }

    /// Visible rows rendered as short strings: `[inst]`, `msg`, `  msg`.
    fn rows(app: &App) -> Vec<String> {
        app.inbox
            .rows()
            .iter()
            .map(|r| match r {
                InboxRow::Group(name) => format!("[{name}]"),
                InboxRow::Thread(i) => app.inbox.threads[*i].head().msg.clone(),
                InboxRow::History { thread, note } => {
                    format!("  {}", app.inbox.threads[*thread].notes[*note].record.msg)
                }
            })
            .collect()
    }

    fn selected_msg(app: &App) -> Option<String> {
        app.selected_inbox_record().map(|(_, r)| r.msg.clone())
    }

    #[test]
    fn push_is_newest_first() {
        let mut app = new_app();
        app.push_notification("a".into(), rec("one", None, None));
        app.push_notification("b".into(), rec("two", None, None));
        assert_eq!(msgs(&app), [("b", "two"), ("a", "one")]);
    }

    #[test]
    fn same_key_threads_per_instance() {
        let mut app = new_app();
        app.push_notification("a".into(), rec("pr 1 v1", Some("pr-1"), None));
        app.push_notification("a".into(), rec("other", None, None));
        app.push_notification("b".into(), rec("b pr 1", Some("pr-1"), None));
        app.set_tab(Tab::Inbox); // read everything
        app.set_tab(Tab::Instances);
        app.push_notification("a".into(), rec("pr 1 v2", Some("pr-1"), None));
        // Same (instance, key) becomes the head of one thread, moved to the
        // top; same key from another instance is its own thread; unkeyed
        // records never thread.
        assert_eq!(msgs(&app), [("a", "pr 1 v2"), ("b", "b pr 1"), ("a", "other")]);
        let t = &app.inbox.threads[0];
        assert_eq!(t.notes.iter().map(|n| n.record.msg.as_str()).collect::<Vec<_>>(), ["pr 1 v2", "pr 1 v1"]);
        assert!(t.unread);
        assert_eq!(app.inbox.unread(), 1);
    }

    #[test]
    fn flat_with_one_instance_grouped_with_two() {
        let mut app = new_app();
        app.push_notification("a".into(), rec("a1", None, None));
        app.push_notification("a".into(), rec("a2", None, None));
        assert!(!app.inbox.grouped());
        assert_eq!(rows(&app), ["a2", "a1"]);
        app.push_notification("b".into(), rec("b1", None, None));
        app.push_notification("a".into(), rec("a3", None, None));
        // Groups ordered by their newest thread.
        assert_eq!(rows(&app), ["[a]", "a3", "a2", "a1", "[b]", "b1"]);
    }

    #[test]
    fn fold_groups_and_threads() {
        let mut app = new_app();
        app.push_notification("b".into(), rec("b1", None, None));
        app.push_notification("a".into(), rec("v1", Some("k"), None));
        app.push_notification("a".into(), rec("v2", Some("k"), None));
        app.push_notification("a".into(), rec("v3", Some("k"), None));
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
    fn cap_counts_history_and_drops_that_instances_oldest() {
        let mut app = new_app();
        app.push_notification("quiet".into(), rec("q0", None, None));
        app.push_notification("noisy".into(), rec("k0", Some("k"), None));
        for i in 1..INBOX_INSTANCE_CAP {
            app.push_notification("noisy".into(), rec(&i.to_string(), None, None));
        }
        assert_eq!(app.inbox.count_for("noisy"), INBOX_INSTANCE_CAP);
        // A new record in the keyed thread: noisy's oldest record is the
        // thread's own first one ("k0"), dropped from its history.
        app.push_notification("noisy".into(), rec("k1", Some("k"), None));
        assert_eq!(app.inbox.count_for("noisy"), INBOX_INSTANCE_CAP);
        let t = &app.inbox.threads[0];
        assert_eq!(t.notes.iter().map(|n| n.record.msg.as_str()).collect::<Vec<_>>(), ["k1"]);
        // Next: the oldest unkeyed one ("1") goes, its thread with it.
        app.push_notification("noisy".into(), rec("new", None, None));
        assert_eq!(app.inbox.count_for("noisy"), INBOX_INSTANCE_CAP);
        assert!(!msgs(&app).contains(&("noisy", "1")));
        assert!(msgs(&app).contains(&("noisy", "2")));
        assert_eq!(app.inbox.count_for("quiet"), 1);
    }

    #[test]
    fn unread_count_and_read_on_entering_tab() {
        let mut app = new_app();
        app.push_notification("a".into(), rec("one", None, None));
        app.push_notification("a".into(), rec("two", None, None));
        assert_eq!(app.inbox.unread(), 2);
        assert_eq!(app.tab_title(Tab::Inbox), "Inbox (2)");
        assert_eq!(app.tab_title(Tab::Ports), "Ports");

        app.on_key(key(KeyCode::Char('4')));
        assert_eq!(app.tab, Tab::Inbox);
        assert_eq!(app.inbox.unread(), 0);
        assert_eq!(app.tab_title(Tab::Inbox), "Inbox");

        // Arriving while the Inbox is shown counts as seen.
        app.push_notification("a".into(), rec("three", None, None));
        assert_eq!(app.inbox.unread(), 0);

        // Tab-cycling into the Inbox marks read too.
        app.on_key(key(KeyCode::Char('1')));
        app.push_notification("a".into(), rec("four", None, None));
        app.on_key(key(KeyCode::BackTab)); // Instances → Inbox (wraps)
        assert_eq!(app.tab, Tab::Inbox);
        assert_eq!(app.inbox.unread(), 0);
    }

    #[test]
    fn badge_count_per_instance() {
        let mut app = new_app();
        app.push_notification("a".into(), rec("1", None, None));
        app.push_notification("a".into(), rec("2", None, None));
        app.push_notification("a".into(), rec("3", Some("k"), None));
        app.push_notification("a".into(), rec("4", Some("k"), None));
        app.push_notification("b".into(), rec("5", None, None));
        // A thread counts once, however long its history.
        assert_eq!(app.inbox.unread_for("a"), 3);
        assert_eq!(app.inbox.unread_for("b"), 1);
        assert_eq!(app.inbox.unread_for("c"), 0);
        app.set_tab(Tab::Inbox);
        assert_eq!(app.inbox.unread_for("a"), 0);
    }

    #[test]
    fn dismiss_and_clear() {
        let mut app = new_app();
        for m in ["1", "2", "3"] {
            app.push_notification("a".into(), rec(m, None, None));
        }
        app.on_key(key(KeyCode::Char('4')));
        app.on_key(key(KeyCode::Down)); // "2"
        app.on_key(key(KeyCode::Char('d')));
        assert_eq!(msgs(&app), [("a", "3"), ("a", "1")]);
        assert_eq!(app.selected(), 1);
        app.on_key(key(KeyCode::Char('d'))); // last row: selection re-clamps
        assert_eq!(msgs(&app), [("a", "3")]);
        assert_eq!(app.selected(), 0);

        app.push_notification("a".into(), rec("4", None, None));
        app.on_key(key(KeyCode::Char('D')));
        assert!(app.inbox.threads.is_empty());
        assert_eq!(app.selected(), 0);
        // Empty inbox: `d` is a no-op, not a panic.
        app.on_key(key(KeyCode::Char('d')));
    }

    #[test]
    fn dismiss_history_thread_and_group() {
        let mut app = new_app();
        app.push_notification("b".into(), rec("b1", None, None));
        app.push_notification("b".into(), rec("b2", None, None));
        for v in ["v1", "v2", "v3"] {
            app.push_notification("a".into(), rec(v, Some("k"), None));
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

        app.push_notification("a".into(), rec("a1", None, None));
        assert_eq!(rows(&app), ["[a]", "a1", "[b]", "b2", "b1"]);
        assert_eq!(app.selected(), 4); // the cursor followed "b1"
        app.on_key(key(KeyCode::Up));
        app.on_key(key(KeyCode::Up));
        assert_eq!(app.selected_inbox_row(), Some(InboxRow::Group("b".into())));
        app.on_key(key(KeyCode::Char('d'))); // header: all of `b`
        assert_eq!(rows(&app), ["a1"]);
    }

    #[test]
    fn changes_mark_dirty() {
        let mut app = new_app();
        assert!(!app.inbox.take_dirty());
        app.push_notification("a".into(), rec("1", None, None));
        assert!(app.inbox.take_dirty());
        assert!(!app.inbox.take_dirty());
        app.on_key(key(KeyCode::Char('4'))); // marks read
        assert!(app.inbox.take_dirty());
        app.on_key(key(KeyCode::Char('1')));
        app.on_key(key(KeyCode::Char('4'))); // nothing unread: no save
        assert!(!app.inbox.take_dirty());
        app.on_key(key(KeyCode::Char('d')));
        assert!(app.inbox.take_dirty());
        app.on_key(key(KeyCode::Char('D')));
        assert!(app.inbox.take_dirty());
    }

    #[test]
    fn toml_roundtrip() {
        let mut app = new_app();
        app.push_notification("a".into(), rec("v1", Some("k"), Some("https://x/1")));
        app.push_notification("b".into(), rec("multi\nline \"q\"", None, None));
        app.push_notification("a".into(), Record { level: Level::Warn, at: 7, ..rec("v2", Some("k"), None) });
        app.set_tab(Tab::Inbox);
        app.push_notification("b".into(), rec("read", None, None));
        app.set_tab(Tab::Instances);
        app.push_notification("b".into(), rec("unread", None, None));

        let text = app.inbox.to_toml().unwrap();
        let loaded = Inbox::from_toml(&text).unwrap();
        let strip = |i: &Inbox| {
            i.threads
                .iter()
                .map(|t| (t.instance.clone(), t.unread, t.notes.clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(strip(&loaded), strip(&app.inbox));
        // Ids keep growing past the saved ones, so the cap's order holds.
        let mut app2 = new_app();
        app2.set_inbox(loaded);
        app2.push_notification("a".into(), rec("later", None, None));
        let max_saved = app.inbox.threads.iter().flat_map(|t| &t.notes).map(|n| n.id).max().unwrap();
        assert!(app2.inbox.threads[0].notes[0].id > max_saved);

        assert!(Inbox::from_toml("").unwrap().threads.is_empty());
        assert!(Inbox::from_toml(
            "[[thread]]\ninstance = \"a\"\n[[thread.note]]\nseq = 0\nlevel = \"loud\"\nat = 0\nmsg = \"x\"\n"
        )
        .is_err());
    }

    #[test]
    fn d_off_the_inbox_does_not_dismiss() {
        let mut app = new_app();
        app.push_notification("a".into(), rec("1", None, None));
        app.on_key(key(KeyCode::Char('d')));
        app.on_key(key(KeyCode::Char('D')));
        assert_eq!(app.inbox.threads.len(), 1);
    }

    #[test]
    fn selection_follows_row_on_push() {
        let mut app = new_app();
        app.push_notification("a".into(), rec("1", None, None));
        app.push_notification("a".into(), rec("2", Some("k"), None));
        app.on_key(key(KeyCode::Char('4')));
        app.on_key(key(KeyCode::Down)); // "1"
        app.push_notification("a".into(), rec("3", None, None));
        assert_eq!(selected_msg(&app).as_deref(), Some("1"));
        // A thread moving from above the cursor to the top keeps it on "1".
        app.push_notification("a".into(), rec("2b", Some("k"), None));
        assert_eq!(selected_msg(&app).as_deref(), Some("1"));
        // Switching to grouped (a second instance) keeps it on "1" too.
        app.push_notification("b".into(), rec("b", None, None));
        assert_eq!(selected_msg(&app).as_deref(), Some("1"));
        // A cursor on the top row stays on top.
        while app.selected() > 0 {
            app.on_key(key(KeyCode::Up));
        }
        app.push_notification("c".into(), rec("4", None, None));
        assert_eq!(app.selected(), 0);
        assert_eq!(app.selected_inbox_row(), Some(InboxRow::Group("c".into())));
    }

    #[test]
    fn enter_opens_link_only_when_present() {
        let mut app = new_app();
        app.push_notification("a".into(), rec("no link", None, None));
        app.push_notification("a".into(), rec("pr", None, Some("https://x/pr/1")));
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
        app.push_notification("a".into(), rec("v1", Some("k"), Some("https://x/1")));
        app.push_notification("a".into(), rec("v2", Some("k"), Some("https://x/2")));
        app.on_key(key(KeyCode::Char('4')));
        app.on_key(key(KeyCode::Right));
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.take_pending_open().as_deref(), Some("https://x/1"));
    }

    #[test]
    fn enter_refuses_non_url_links() {
        let mut app = new_app();
        app.push_notification("a".into(), rec("x", None, Some("--help")));
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
    fn clock_and_offset() {
        assert_eq!(clock(0, 0), "00:00");
        assert_eq!(clock(13 * 3600 + 7 * 60 + 59, 0), "13:07");
        assert_eq!(clock(23 * 3600, 2 * 3600), "01:00");
        assert_eq!(clock(3600, -2 * 3600), "23:00");
        assert_eq!(parse_utc_offset("+0200\n"), Some(7200));
        assert_eq!(parse_utc_offset("-0530"), Some(-(5 * 3600 + 30 * 60)));
        assert_eq!(parse_utc_offset("CEST"), None);
        assert_eq!(parse_utc_offset("+02"), None);
        assert_eq!(parse_utc_offset(""), None);
    }
}
