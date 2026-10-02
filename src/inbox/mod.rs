//! The Inbox model: container notifications (`devsbd notify`,
//! docs/automations.md) as threads, newest first. This is the persisted half,
//! out of `src/tui/` because the bridge writes records here while any number
//! of dashboards read them (see [`store`]); the TUI keeps only view state
//! (view, selection, the open thread).
//!
//! Two kinds of thread share the store:
//!
//! - **notify** ([`Kind::Notify`]): records sharing an `(owner, key)`, newest
//!   first, the newest being the head and the rest its history.
//! - **thread** ([`Kind::Thread`]): one dispatcher-owned item with a state, a
//!   status and actions (`devsbd thread put`, docs/inbox-threads.md). It has
//!   no notes at all; its history is the [`Entry`] timeline the host keeps.
//!
//! `owner` is the sending instance's `instance_id`, not its name, so a rename
//! or rebuild keeps the thread (`owner_name` is only what to show). Instance
//! ids are never reused, so `devsandbox rm` can archive a gone instance's
//! threads instead of deleting them.
//!
//! Everything here is plain state: threading, cap, unread, retention, the put
//! transition and the bridge's decision stay unit-testable without a store.

pub mod store;
pub mod thread;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::devsbd::notify::{Level, Message, Record};

pub use thread::{Action, Reply, State, ThreadPut};

/// Records kept per owner, thread history included; the owner's oldest record
/// drops off beyond this. Per owner only, so a noisy container never evicts
/// another's.
pub const INBOX_INSTANCE_CAP: usize = 200;

/// `inbox.toml` schema version written by this build. v1 (no `version` key)
/// identified threads by instance *name*; see [`Inbox::from_toml`]. Thread
/// records (v2.1) only add defaulted fields, so a v2 file still loads as is
/// and the version stays put.
pub const VERSION: u32 = 2;

/// How long an archived or `done` thread is kept after its last change.
pub const RETENTION: u64 = 14 * 86_400;

/// Whether `link` is an `http(s)://` URL. The link comes from inside the
/// container and ends up as the opener's argv, so only web links pass: no
/// bare paths, no leading `-` (an option to `xdg-open`/`open`), and no
/// `file:` or custom schemes that would hand the container a host handler.
/// Shared by the Inbox UI and the `thread put` schema.
pub fn is_url(link: &str) -> bool {
    ["http://", "https://"]
        .iter()
        .any(|p| link.len() > p.len() && link[..p.len()].eq_ignore_ascii_case(p))
}

/// A message folded onto one line, for row and status-line use.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// One received record. `id` grows with arrival order across the whole inbox
/// (persisted), so "oldest" is well defined across threads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    pub id: u64,
    pub record: Record,
}

/// Which of the two kinds a [`Thread`] is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// `devsbd notify` records: a log with a head and history.
    #[default]
    Notify,
    /// `devsbd thread put`: one item with state, actions and a timeline.
    Thread,
}

impl Kind {
    fn is_notify(&self) -> bool {
        matches!(self, Kind::Notify)
    }
}

/// Owner prefix for a v1 thread whose instance name no longer resolves. Such
/// a thread has no live sender, so it loads archived.
const UNRESOLVED_OWNER: &str = "name:";

/// One timeline entry on a thread-kind thread. Steps 6-7 add action, reply,
/// done and reopen kinds; the shape (a kind plus one text) already fits them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    Message,
    State,
    Status,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Arrival order across the whole inbox, like [`Note::id`], so the cap can
    /// compare an entry with a note.
    pub seq: u64,
    pub at: u64,
    pub kind: EntryKind,
    pub text: String,
}

/// A keyed record's history, or a single unkeyed record, or one dispatcher
/// thread. Thread-kind threads carry no notes, so nothing may index `notes`
/// without checking.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Thread {
    /// Stable id, persisted: selection and the open pane follow it across the
    /// reloads every dashboard does when the store changes.
    pub id: u64,
    /// `instance_id` of the sender (stable across stop/restart/rebuild).
    pub owner: String,
    /// The owner's state key when the newest record arrived. Display only;
    /// never an identity (instances get renamed).
    pub owner_name: String,
    /// Threading key, shared by every note (unkeyed threads hold one note).
    pub key: Option<String>,
    pub kind: Kind,
    /// The owner is gone (`devsandbox rm`, or a v1 name that no longer
    /// resolves). Read-only, hidden from the live list, dropped by retention.
    pub archived: bool,
    /// Only the head counts as unread; history is a trace, not news.
    pub unread: bool,
    /// Newest first; empty on a thread-kind thread.
    pub notes: Vec<Note>,
    // Thread-kind fields, replaced wholesale by each put.
    pub title: String,
    pub link: Option<String>,
    pub state: Option<State>,
    pub status: Option<String>,
    pub child: Option<String>,
    pub message: Option<String>,
    pub actions: Vec<Action>,
    pub reply: Option<Reply>,
    /// Unix seconds of the last put; 0 on a notify thread, which dates itself
    /// from its head record.
    pub updated_at: u64,
    /// Oldest first, so appending is the common case.
    pub entries: Vec<Entry>,
}

impl Thread {
    /// The newest record, or `None` on a thread-kind thread (which has none).
    pub fn head(&self) -> Option<&Record> {
        self.notes.first().map(|n| &n.record)
    }

    /// When the thread last changed: the newest put, else its head record.
    pub fn changed_at(&self) -> u64 {
        self.updated_at.max(self.head().map_or(0, |r| r.at))
    }

    /// Whether the thread is waiting on the user: what the **Needs you** view
    /// and both badges count. A dispatcher thread earns it by saying so
    /// (`needs-you`); a plain notify record by being unread, so a
    /// non-dispatcher's `notify` still surfaces (docs/inbox-threads.md,
    /// *Decisions*). An archived thread has no live owner, so it never counts.
    pub fn needs_you(&self) -> bool {
        !self.archived
            && match self.kind {
                Kind::Notify => self.unread,
                Kind::Thread => self.state == Some(State::NeedsYou),
            }
    }
}

/// A mutation a dashboard asks the store to apply. Row indices can't cross the
/// process boundary (another dashboard may have changed the list), so every
/// variant names what it touches by a stable id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    /// Mark one thread read: the Inbox marks what the user actually opened,
    /// not everything the tab happened to show.
    MarkRead(u64),
    /// Mark every notify record read: they were on screen when the user left
    /// the Inbox. Dispatcher threads are only read by opening them, since
    /// their state, not their unread flag, is what asks for attention.
    MarkNotifyRead,
    /// Dismiss a whole thread (its history with it).
    RemoveThread(u64),
    /// Dismiss every notify record. Dispatcher threads are state a dispatcher
    /// re-asserts, so clearing them would only make them come back.
    ClearNotify,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Inbox {
    /// Ordered by head, newest first.
    pub threads: Vec<Thread>,
    next_id: u64,
}

impl Inbox {
    fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Insert at the top. A keyed record joins the thread with the same
    /// `(owner, key)` as its new head, moving it to the top with `unread`.
    /// Keys are per owner, since two sandboxes can't know each other's.
    pub fn push(&mut self, owner: String, owner_name: String, record: Record, unread: bool) {
        let note = Note { id: self.next_id(), record };
        let key = note.record.key.clone();
        // A dispatcher thread with the same key is a different object; a
        // notify record never joins it.
        let existing = key.as_ref().and_then(|key| {
            self.threads
                .iter()
                .position(|t| t.owner == owner && t.kind == Kind::Notify && t.key.as_ref() == Some(key))
        });
        let thread = match existing {
            Some(pos) => {
                let mut t = self.threads.remove(pos);
                t.notes.insert(0, note);
                t.unread = unread;
                // A rename shows up on the next record; the thread is kept.
                t.owner_name = owner_name;
                t.archived = false;
                t
            }
            None => {
                let id = self.next_id();
                Thread {
                    id,
                    owner: owner.clone(),
                    owner_name,
                    key,
                    notes: vec![note],
                    unread,
                    ..Thread::default()
                }
            }
        };
        self.threads.insert(0, thread);
        self.enforce_cap(&owner);
    }

    /// Apply one `devsbd thread put` from `owner`, at container time `at`.
    /// Pure: the store decides what to write from the outcome.
    ///
    /// A put that changes nothing returns [`PutOutcome::Unchanged`] without
    /// touching a field, so the store's content comparison skips the write and
    /// the file's mtime doesn't move. That idempotence is the point: a
    /// dispatcher is meant to re-assert every thread on every pass.
    pub fn put(&mut self, owner: &str, owner_name: &str, at: u64, put: ThreadPut) -> PutOutcome {
        let pos = self.threads.iter().position(|t| {
            t.owner == owner && t.kind == Kind::Thread && t.key.as_deref() == Some(put.key.as_str())
        });
        let Some(pos) = pos else {
            let id = self.next_id();
            let mut thread = Thread {
                id,
                owner: owner.to_string(),
                owner_name: owner_name.to_string(),
                key: Some(put.key.clone()),
                kind: Kind::Thread,
                unread: true,
                updated_at: at,
                ..Thread::default()
            };
            // Seed the timeline with what the thread already says, so one
            // created mid-conversation doesn't open blank.
            let seeds = changed_entries(&put, true, true, true);
            apply_put(&mut thread, put);
            self.append_entries(&mut thread, at, seeds);
            let entered = thread.state == Some(State::NeedsYou);
            self.threads.insert(0, thread);
            self.enforce_cap(owner);
            return PutOutcome::Applied { created: true, entered_needs_you: entered };
        };

        let old = &self.threads[pos];
        let message = old.message != put.message;
        let state = old.state != Some(put.state);
        let status = old.status != put.status;
        let cosmetic = old.owner_name != owner_name
            || old.title != put.title
            || old.link != put.link
            || old.child != put.child
            || old.actions != put.actions
            || old.reply != put.reply
            || old.archived;
        if !(message || state || status || cosmetic) {
            return PutOutcome::Unchanged;
        }
        let entered = state && put.state == State::NeedsYou;
        let seeds = changed_entries(&put, message, state, status);
        let mut thread = self.threads.remove(pos);
        thread.owner_name = owner_name.to_string();
        // A put from a live container means the owner is back; only `rm`
        // archives, and ids are never reused.
        thread.archived = false;
        // Cosmetic changes (a new label, a rename) are not news.
        thread.unread = thread.unread || message || state || status;
        thread.updated_at = at;
        apply_put(&mut thread, put);
        self.append_entries(&mut thread, at, seeds);
        self.threads.insert(0, thread);
        self.enforce_cap(owner);
        PutOutcome::Applied { created: false, entered_needs_you: entered }
    }

    /// `devsbd thread rm <key>`: drop `owner`'s thread with that key.
    pub fn thread_rm(&mut self, owner: &str, key: &str) -> bool {
        let before = self.threads.len();
        self.threads
            .retain(|t| !(t.owner == owner && t.kind == Kind::Thread && t.key.as_deref() == Some(key)));
        before != self.threads.len()
    }

    /// Mark everything from a removed instance read-only. Called by
    /// `devsandbox rm`: the threads are a record of work that happened, and
    /// the instance id can never come back, so they're kept until retention
    /// drops them rather than deleted on the spot.
    pub fn archive_owner(&mut self, owner: &str) -> usize {
        let mut archived = 0;
        for t in self.threads.iter_mut().filter(|t| t.owner == owner && !t.archived) {
            t.archived = true;
            archived += 1;
        }
        archived
    }

    /// Drop archived and `done` threads whose last change is older than
    /// [`RETENTION`]. Live threads are never dropped by age: only the
    /// per-owner cap bounds those.
    pub fn prune(&mut self, now: u64) -> usize {
        let before = self.threads.len();
        self.threads.retain(|t| {
            let retired = t.archived || t.state == Some(State::Done);
            !(retired && now.saturating_sub(t.changed_at()) > RETENTION)
        });
        before - self.threads.len()
    }

    /// Apply one decoded [`SinkAction`] from the bridge and say what to
    /// surface. Retention runs here because this is the one write path every
    /// container message takes, so a store nobody else touches still ages out.
    pub fn apply_sink(&mut self, owner: &str, owner_name: &str, now: u64, action: SinkAction) -> Option<Shown> {
        self.prune(now);
        match action {
            SinkAction::Push(record) => {
                let shown = Shown {
                    line: format!("{owner_name}: {}", one_line(&record.msg)),
                    popup: Some(ShownPopup {
                        key: record.key.clone(),
                        level: record.level,
                        body: match &record.link {
                            Some(link) => format!("{}\n{link}", record.msg),
                            None => record.msg.clone(),
                        },
                    }),
                };
                self.push(owner.to_string(), owner_name.to_string(), record, true);
                Some(shown)
            }
            SinkAction::Rm { key } => {
                self.thread_rm(owner, &key);
                None
            }
            SinkAction::Put { at, put } => {
                let key = put.key.clone();
                let line = match &put.status {
                    Some(status) => format!("{owner_name}: {} — {status}", put.title),
                    None => format!("{owner_name}: {}", put.title),
                };
                let body = match &put.message {
                    Some(message) => format!("{}\n{message}", put.title),
                    None => put.title.clone(),
                };
                match self.put(owner, owner_name, at, put) {
                    // No entry, no unread, no write, and nothing on the status
                    // line: a re-asserted thread is invisible.
                    PutOutcome::Unchanged => None,
                    PutOutcome::Applied { entered_needs_you, .. } => Some(Shown {
                        line,
                        popup: entered_needs_you.then(|| ShownPopup {
                            key: Some(format!("thread:{key}")),
                            level: Level::Warn,
                            body,
                        }),
                    }),
                }
            }
        }
    }

    /// Append `(kind, text)` pairs as timeline entries stamped `at`, each with
    /// its own arrival id so the cap can order them against notes.
    fn append_entries(&mut self, thread: &mut Thread, at: u64, seeds: Vec<(EntryKind, String)>) {
        for (kind, text) in seeds {
            let seq = self.next_id();
            thread.entries.push(Entry { seq, at, kind, text });
        }
    }

    /// Apply one dashboard-requested [`Op`].
    pub fn apply(&mut self, op: &Op) {
        match op {
            Op::MarkNotifyRead => {
                for t in self.threads.iter_mut().filter(|t| t.kind == Kind::Notify) {
                    t.unread = false;
                }
            }
            Op::MarkRead(id) => {
                if let Some(t) = self.threads.iter_mut().find(|t| t.id == *id) {
                    t.unread = false;
                }
            }
            Op::RemoveThread(id) => self.threads.retain(|t| t.id != *id),
            Op::ClearNotify => self.threads.retain(|t| t.kind != Kind::Notify),
        }
    }

    /// Keep `owner` within the cap: archived threads go first, then `done`
    /// ones, and only then does a live thread lose its oldest record or
    /// timeline entry. Retired threads are the cheapest thing to lose, so a
    /// busy dispatcher doesn't shed the history of what it's working on now.
    fn enforce_cap(&mut self, owner: &str) {
        while self.weight_for(owner) > INBOX_INSTANCE_CAP {
            let retired = self
                .oldest_thread(owner, |t| t.archived)
                .or_else(|| self.oldest_thread(owner, |t| t.state == Some(State::Done)));
            if let Some(pos) = retired {
                self.threads.remove(pos);
                continue;
            }
            if !self.drop_oldest_item(owner) {
                return;
            }
        }
    }

    /// What `owner` holds against the cap: records and timeline entries alike.
    fn weight_for(&self, owner: &str) -> usize {
        self.threads
            .iter()
            .filter(|t| t.owner == owner)
            .map(|t| t.notes.len() + t.entries.len())
            .sum()
    }

    /// Index of `owner`'s least recently changed thread matching `pick`.
    fn oldest_thread(&self, owner: &str, pick: impl Fn(&Thread) -> bool) -> Option<usize> {
        self.threads
            .iter()
            .enumerate()
            .filter(|(_, t)| t.owner == owner && pick(t))
            .min_by_key(|(_, t)| t.changed_at())
            .map(|(i, _)| i)
    }

    /// Drop `owner`'s single oldest record or timeline entry; `false` when
    /// there is nothing left to drop, so [`enforce_cap`](Self::enforce_cap)
    /// stops instead of spinning.
    fn drop_oldest_item(&mut self, owner: &str) -> bool {
        let Some(pos) = self
            .threads
            .iter()
            .enumerate()
            .filter(|(_, t)| t.owner == owner)
            .filter_map(|(i, t)| oldest_item(t).map(|seq| (i, seq)))
            .min_by_key(|&(_, seq)| seq)
            .map(|(i, _)| i)
        else {
            return false;
        };
        let thread = &mut self.threads[pos];
        // Notes are newest first, entries oldest first.
        let note = thread.notes.last().map(|n| n.id);
        let entry = thread.entries.first().map(|e| e.seq);
        match (note, entry) {
            (Some(n), Some(e)) if e < n => drop(thread.entries.remove(0)),
            (Some(_), _) => drop(thread.notes.pop()),
            (None, Some(_)) => drop(thread.entries.remove(0)),
            (None, None) => return false,
        }
        // A notify thread with no records left has nothing to show; a thread
        // one still carries its state.
        if thread.kind == Kind::Notify && thread.notes.is_empty() {
            self.threads.remove(pos);
        }
        true
    }

    /// Threads waiting on the user ([`Thread::needs_you`]): the tab-title count.
    pub fn needs_you(&self) -> usize {
        self.threads.iter().filter(|t| t.needs_you()).count()
    }

    /// The same count for owner `owner` (an `instance_id`), for the
    /// Instances-row badge. By id, which every snapshot row carries, so a
    /// renamed instance keeps its badge and a reused name never inherits one.
    pub fn needs_you_for(&self, owner: &str) -> usize {
        self.threads.iter().filter(|t| t.needs_you() && t.owner == owner).count()
    }

    /// The `inbox.toml` contents (always v2).
    pub fn to_toml(&self) -> Result<String, String> {
        let saved = SavedInbox {
            version: VERSION,
            threads: self
                .threads
                .iter()
                .map(|t| SavedThread {
                    id: t.id,
                    owner: t.owner.clone(),
                    owner_name: t.owner_name.clone(),
                    key: t.key.clone(),
                    instance: None,
                    kind: t.kind,
                    archived: t.archived,
                    unread: t.unread,
                    title: t.title.clone(),
                    link: t.link.clone(),
                    state: t.state,
                    status: t.status.clone(),
                    child: t.child.clone(),
                    message: t.message.clone(),
                    updated_at: t.updated_at,
                    reply: t.reply.clone(),
                    actions: t.actions.clone(),
                    entries: t.entries.clone(),
                    notes: t
                        .notes
                        .iter()
                        .map(|n| SavedNote {
                            seq: n.id,
                            level: n.record.level.as_str().to_string(),
                            at: n.record.at,
                            key: None,
                            link: n.record.link.clone(),
                            msg: n.record.msg.clone(),
                        })
                        .collect(),
                })
                .collect(),
        };
        toml::to_string_pretty(&saved).map_err(|e| e.to_string())
    }

    /// Parse `inbox.toml` contents. `names` maps instance names (state keys)
    /// to `instance_id`s, and is only used by a v1 file, which identified
    /// threads by name: a name that no longer resolves keeps its thread under
    /// the placeholder owner `name:<instance>` (step 3 archives those).
    pub fn from_toml(text: &str, names: &BTreeMap<String, String>) -> Result<Inbox, String> {
        let saved: SavedInbox = toml::from_str(text).map_err(|e| e.to_string())?;
        let mut inbox = Inbox::default();
        for t in saved.threads {
            // v1 repeated the key on every note; it belongs to the thread.
            let v1_key = t.notes.first().and_then(|n| n.key.clone());
            let notes = t
                .notes
                .into_iter()
                .map(|n| {
                    let level = Level::parse(&n.level).ok_or_else(|| format!("bad level `{}`", n.level))?;
                    Ok(Note {
                        id: n.seq,
                        record: Record { level, key: None, link: n.link, msg: n.msg, at: n.at },
                    })
                })
                .collect::<Result<Vec<_>, String>>()?;
            // A notify thread with no records is nothing; a dispatcher thread
            // carries its state with no records at all.
            if notes.is_empty() && t.kind == Kind::Notify {
                continue;
            }
            // v1: `instance` is a name; v2 carries the id and the key.
            let (owner, owner_name, key) = match t.instance {
                Some(name) => {
                    let owner =
                        names.get(&name).cloned().unwrap_or_else(|| format!("{UNRESOLVED_OWNER}{name}"));
                    (owner, name, v1_key)
                }
                None => (t.owner, t.owner_name, t.key),
            };
            let id = if saved.version >= VERSION { t.id } else { inbox.next_id() };
            let notes = notes
                .into_iter()
                .map(|n| Note { record: Record { key: key.clone(), ..n.record }, ..n })
                .collect();
            inbox.threads.push(Thread {
                id,
                // A v1 name that no longer resolves has no live owner: the
                // thread is history, so it is read-only from here on.
                archived: t.archived || owner.starts_with(UNRESOLVED_OWNER),
                owner,
                owner_name,
                key,
                kind: t.kind,
                unread: t.unread,
                notes,
                title: t.title,
                link: t.link,
                state: t.state,
                status: t.status,
                child: t.child,
                message: t.message,
                actions: t.actions,
                reply: t.reply,
                updated_at: t.updated_at,
                entries: t.entries,
            });
        }
        // Ids keep growing past everything loaded, so arrival order (which the
        // cap reads) and thread identity stay unique.
        inbox.next_id = inbox
            .threads
            .iter()
            .flat_map(|t| {
                std::iter::once(t.id)
                    .chain(t.notes.iter().map(|n| n.id))
                    .chain(t.entries.iter().map(|e| e.seq))
            })
            .max()
            .map_or(0, |m| m + 1);
        Ok(inbox)
    }
}

/// What [`Inbox::put`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PutOutcome {
    /// Every field already matched: nothing was touched.
    Unchanged,
    Applied {
        created: bool,
        /// The thread entered `needs-you` (or was created in it): the one
        /// case that earns a desktop popup.
        entered_needs_you: bool,
    },
}

/// Copy a put's fields onto a thread. Actions and reply are replaced
/// wholesale: the put is the dispatcher's whole current view of the thread.
fn apply_put(thread: &mut Thread, put: ThreadPut) {
    thread.title = put.title;
    thread.link = put.link;
    thread.state = Some(put.state);
    thread.status = put.status;
    thread.child = put.child;
    thread.message = put.message;
    thread.actions = put.actions;
    thread.reply = put.reply;
}

/// Timeline entries for the fields a put changed, in reading order. Only the
/// three fields that carry news get one; a new label or link is not history.
fn changed_entries(put: &ThreadPut, message: bool, state: bool, status: bool) -> Vec<(EntryKind, String)> {
    let mut out = Vec::new();
    if let (true, Some(text)) = (message, &put.message) {
        out.push((EntryKind::Message, text.clone()));
    }
    if state {
        out.push((EntryKind::State, put.state.as_str().to_string()));
    }
    if let (true, Some(text)) = (status, &put.status) {
        out.push((EntryKind::Status, text.clone()));
    }
    out
}

/// The oldest arrival id a thread still holds (its last note or first entry),
/// or `None` when it holds neither.
fn oldest_item(thread: &Thread) -> Option<u64> {
    let note = thread.notes.last().map(|n| n.id);
    let entry = thread.entries.first().map(|e| e.seq);
    match (note, entry) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// What the bridge's notify sink does with one decoded message. Split out of
/// the sink so authorization and the schema check are testable without a
/// store, a container or a dashboard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SinkAction {
    /// Store a notify record: the container's own, or an `error` one carrying
    /// back why a thread message was refused.
    Push(Record),
    Put { at: u64, put: ThreadPut },
    Rm { key: String },
}

/// Decide what to do with `message` from `instance` (its state key).
/// `declares_dispatcher` is `commands::dispatch::declares_dispatcher`, which
/// the caller only evaluates for thread messages (it loads state).
///
/// A refusal is never silent: it becomes an `error` record from the same
/// instance, keyed by the thread, so the dispatcher's author sees the reason
/// in the Inbox instead of a message that quietly never appears.
pub fn decide(instance: &str, declares_dispatcher: bool, message: Message) -> SinkAction {
    let (at, key, body) = match message {
        Message::Notify(record) => return SinkAction::Push(record),
        Message::ThreadPut { at, key, body } => (at, key, Some(body)),
        Message::ThreadRm { at, key } => (at, key, None),
    };
    let verb = if body.is_some() { "put" } else { "rm" };
    if !declares_dispatcher {
        let why = format!("`{instance}` doesn't declare a dispatcher");
        return refused(at, &key, &format!("thread {verb} denied: {why}"));
    }
    let Some(body) = body else { return SinkAction::Rm { key } };
    match thread::parse(&body) {
        Ok(put) if put.key != key => {
            let why = format!("body key `{}` is not the queued key", put.key);
            refused(at, &key, &format!("thread put rejected: {why}"))
        }
        Ok(put) => SinkAction::Put { at, put },
        Err(why) => refused(at, &key, &format!("thread put rejected: {why}")),
    }
}

fn refused(at: u64, key: &str, msg: &str) -> SinkAction {
    SinkAction::Push(Record {
        level: Level::Error,
        key: Some(format!("thread:{key}")),
        link: None,
        msg: msg.to_string(),
        at,
    })
}

/// What an applied message should surface: the dashboard status line, and the
/// desktop popup when it earns one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shown {
    pub line: String,
    pub popup: Option<ShownPopup>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShownPopup {
    /// What `desktop::RateLimit` coalesces repeats on.
    pub key: Option<String>,
    pub level: Level,
    pub body: String,
}

/// `inbox.toml` schema: threads newest first, each with its notes newest
/// first. One struct serves both versions, since a v1 file is just a v2 one
/// with `instance` instead of `owner`/`owner_name` and the key on the notes.
#[derive(Serialize, Deserialize)]
struct SavedInbox {
    /// Absent in v1 files, which this build migrates on load.
    #[serde(default)]
    version: u32,
    #[serde(default, rename = "thread", skip_serializing_if = "Vec::is_empty")]
    threads: Vec<SavedThread>,
}

/// Field order is load-bearing: TOML wants every scalar before the tables
/// (`reply`) and arrays of tables (`action`, `entry`, `note`). Everything
/// added for thread records defaults and is skipped when empty, so a v2 file
/// written before them loads and round-trips unchanged.
#[derive(Serialize, Deserialize)]
struct SavedThread {
    #[serde(default)]
    id: u64,
    #[serde(default)]
    owner: String,
    #[serde(default)]
    owner_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key: Option<String>,
    /// v1 only: the owner's state key. Never written back.
    #[serde(default, skip_serializing)]
    instance: Option<String>,
    #[serde(default, skip_serializing_if = "Kind::is_notify")]
    kind: Kind,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    archived: bool,
    #[serde(default)]
    unread: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    link: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    state: Option<State>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    child: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero")]
    updated_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reply: Option<Reply>,
    #[serde(default, rename = "action", skip_serializing_if = "Vec::is_empty")]
    actions: Vec<Action>,
    #[serde(default, rename = "entry", skip_serializing_if = "Vec::is_empty")]
    entries: Vec<Entry>,
    #[serde(default, rename = "note", skip_serializing_if = "Vec::is_empty")]
    notes: Vec<SavedNote>,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

#[derive(Serialize, Deserialize)]
struct SavedNote {
    /// Arrival order across the inbox (the cap drops the lowest first).
    seq: u64,
    level: String,
    at: u64,
    /// v1 only on read: the key now lives on the thread.
    #[serde(default, skip_serializing)]
    key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    link: Option<String>,
    msg: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(msg: &str, key: Option<&str>, link: Option<&str>) -> Record {
        Record {
            level: Level::Info,
            key: key.map(str::to_string),
            link: link.map(str::to_string),
            msg: msg.into(),
            at: 0,
        }
    }

    /// Push from owner `id`, whose name is the id without its `-id` suffix.
    fn push(inbox: &mut Inbox, owner: &str, record: Record, unread: bool) {
        inbox.push(format!("{owner}-id"), owner.into(), record, unread);
    }

    /// Thread heads, newest first.
    fn msgs(inbox: &Inbox) -> Vec<(&str, &str)> {
        inbox
            .threads
            .iter()
            .map(|t| (t.owner_name.as_str(), t.head().unwrap().msg.as_str()))
            .collect()
    }

    fn names(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(n, id)| (n.to_string(), id.to_string())).collect()
    }

    fn put_body(key: &str) -> ThreadPut {
        ThreadPut {
            key: key.into(),
            title: "PR 1".into(),
            state: State::Active,
            status: Some("running ci".into()),
            message: Some("ci started".into()),
            ..ThreadPut::default()
        }
    }

    /// A thread's timeline as `(kind, text)`, oldest first.
    fn timeline(t: &Thread) -> Vec<(&str, &str)> {
        t.entries
            .iter()
            .map(|e| {
                let kind = match e.kind {
                    EntryKind::Message => "message",
                    EntryKind::State => "state",
                    EntryKind::Status => "status",
                };
                (kind, e.text.as_str())
            })
            .collect()
    }

    fn find<'a>(inbox: &'a Inbox, key: &str) -> Option<&'a Thread> {
        inbox.threads.iter().find(|t| t.key.as_deref() == Some(key))
    }

    #[test]
    fn put_creates_a_thread_with_its_opening_timeline() {
        let mut inbox = Inbox::default();
        let out = inbox.put("web-id", "web", 100, put_body("pr-1"));
        assert_eq!(out, PutOutcome::Applied { created: true, entered_needs_you: false });
        let t = &inbox.threads[0];
        assert_eq!((t.kind, t.unread, t.archived), (Kind::Thread, true, false));
        assert!(t.notes.is_empty(), "a dispatcher thread has no records");
        assert_eq!(t.changed_at(), 100);
        assert_eq!(t.state, Some(State::Active));
        assert_eq!(
            timeline(t),
            [("message", "ci started"), ("state", "active"), ("status", "running ci")]
        );
    }

    /// The main clutter fix: a dispatcher re-asserting a thread every pass
    /// costs nothing, down to not touching a single field (so the store skips
    /// the write and no dashboard sees a change).
    #[test]
    fn an_identical_put_changes_nothing() {
        let mut inbox = Inbox::default();
        inbox.put("web-id", "web", 100, put_body("pr-1"));
        let before = inbox.clone();
        assert_eq!(inbox.put("web-id", "web", 200, put_body("pr-1")), PutOutcome::Unchanged);
        assert_eq!(inbox, before);
    }

    #[test]
    fn put_logs_one_entry_per_changed_field_and_only_news_is_unread() {
        let mut inbox = Inbox::default();
        inbox.put("web-id", "web", 100, put_body("pr-1"));
        inbox.put("web-id", "web", 100, ThreadPut { key: "other".into(), ..put_body("other") });
        inbox.threads.iter_mut().for_each(|t| t.unread = false);

        // Status and message change; title/link/actions don't.
        let changed = ThreadPut {
            status: Some("merged".into()),
            message: Some("ci green".into()),
            ..put_body("pr-1")
        };
        let out = inbox.put("web-id", "web", 300, changed);
        assert_eq!(out, PutOutcome::Applied { created: false, entered_needs_you: false });
        let t = &inbox.threads[0];
        assert_eq!(t.key.as_deref(), Some("pr-1"), "a changed thread moves to the top");
        assert!(t.unread);
        assert_eq!(t.updated_at, 300);
        assert_eq!(
            timeline(t)[3..],
            [("message", "ci green"), ("status", "merged")],
            "one entry per changed field, no `state` entry"
        );

        // A cosmetic change is applied but is not news.
        inbox.threads[0].unread = false;
        let renamed = ThreadPut {
            title: "PR 1 (renamed)".into(),
            status: Some("merged".into()),
            message: Some("ci green".into()),
            ..put_body("pr-1")
        };
        assert!(matches!(inbox.put("web-id", "web", 400, renamed), PutOutcome::Applied { .. }));
        let t = &inbox.threads[0];
        assert_eq!(t.title, "PR 1 (renamed)");
        assert!(!t.unread, "a new title is not news");
        assert_eq!(timeline(t).len(), 5, "and leaves no timeline entry");
    }

    #[test]
    fn entering_needs_you_is_reported_once() {
        let mut inbox = Inbox::default();
        let needs = ThreadPut { state: State::NeedsYou, ..put_body("pr-1") };
        // Created straight into needs-you.
        let out = inbox.put("web-id", "web", 10, needs.clone());
        assert_eq!(out, PutOutcome::Applied { created: true, entered_needs_you: true });
        // Re-asserting it is not a new transition.
        let again = ThreadPut { status: Some("still".into()), ..needs.clone() };
        assert_eq!(
            inbox.put("web-id", "web", 20, again),
            PutOutcome::Applied { created: false, entered_needs_you: false }
        );
        // Out and back in is.
        let active = ThreadPut { state: State::Active, status: Some("still".into()), ..needs.clone() };
        inbox.put("web-id", "web", 30, active);
        let back = ThreadPut { status: Some("still".into()), ..needs };
        assert_eq!(
            inbox.put("web-id", "web", 40, back),
            PutOutcome::Applied { created: false, entered_needs_you: true }
        );
    }

    #[test]
    fn keys_are_per_owner_and_per_kind() {
        let mut inbox = Inbox::default();
        push(&mut inbox, "web", rec("note", Some("pr-1"), None), true);
        inbox.put("web-id", "web", 10, put_body("pr-1"));
        inbox.put("other-id", "other", 10, put_body("pr-1"));
        assert_eq!(inbox.threads.len(), 3, "a notify record never joins a dispatcher thread");
        // rm touches one owner's thread only, and leaves the notify record.
        assert!(inbox.thread_rm("web-id", "pr-1"));
        assert!(!inbox.thread_rm("web-id", "pr-1"), "already gone");
        assert_eq!(inbox.threads.len(), 2);
        assert!(find(&inbox, "pr-1").is_some_and(|t| t.kind == Kind::Notify || t.owner == "other-id"));
    }

    #[test]
    fn archived_and_done_threads_age_out() {
        let mut inbox = Inbox::default();
        let day = 86_400;
        inbox.put("web-id", "web", day, ThreadPut { state: State::Done, ..put_body("old-done") });
        inbox.put("web-id", "web", day, put_body("old-active"));
        inbox.put("gone-id", "gone", day, put_body("orphan"));
        push(&mut inbox, "gone", Record { at: day, ..rec("note", None, None) }, true);

        assert_eq!(inbox.archive_owner("gone-id"), 2, "threads and records alike");
        assert_eq!(inbox.archive_owner("gone-id"), 0, "idempotent");
        assert!(inbox.threads.iter().filter(|t| t.owner == "gone-id").all(|t| t.archived));

        // Nothing is dropped inside the retention window.
        assert_eq!(inbox.prune(day + RETENTION), 0);
        // Past it, the archived and the done go; the live thread stays.
        assert_eq!(inbox.prune(day + RETENTION + 1), 3);
        assert_eq!(inbox.threads.len(), 1);
        assert_eq!(inbox.threads[0].key.as_deref(), Some("old-active"));

        // A put from a live container un-archives (ids are never reused, so
        // this only happens to a thread archived in error).
        inbox.put("web-id", "web", day, put_body("revived"));
        inbox.archive_owner("web-id");
        inbox.put("web-id", "web", day, put_body("revived"));
        assert!(!find(&inbox, "revived").unwrap().archived);
    }

    #[test]
    fn cap_evicts_archived_then_done_then_the_oldest() {
        let mut inbox = Inbox::default();
        // One entry each (no message, no status), so the arithmetic is plain.
        let bare = |key: &str, state: State| ThreadPut {
            key: key.into(),
            title: "t".into(),
            state,
            ..ThreadPut::default()
        };
        inbox.put("w-id", "w", 1, bare("archived", State::Active));
        inbox.archive_owner("w-id");
        inbox.put("w-id", "w", 2, bare("done", State::Done));
        inbox.put("w-id", "w", 3, bare("live", State::Active));
        assert_eq!(inbox.weight_for("w-id"), 3, "entries count toward the cap");

        let mut next = 0;
        let mut fill = |inbox: &mut Inbox, n: usize| {
            for _ in 0..n {
                push(inbox, "w", rec(&format!("n{next}"), None, None), true);
                next += 1;
            }
        };
        fill(&mut inbox, INBOX_INSTANCE_CAP - 2);
        assert!(find(&inbox, "archived").is_none(), "archived goes first");
        assert!(find(&inbox, "done").is_some() && find(&inbox, "live").is_some());
        fill(&mut inbox, 1);
        assert!(find(&inbox, "done").is_none(), "then done");
        fill(&mut inbox, 1);
        // Then the owner's single oldest item: the live thread's one entry,
        // queued before every record.
        assert!(find(&inbox, "live").is_some_and(|t| t.entries.is_empty()));
        fill(&mut inbox, 1);
        let notes: Vec<&str> =
            inbox.threads.iter().filter_map(|t| t.head()).map(|r| r.msg.as_str()).collect();
        assert!(!notes.contains(&"n0"), "and then its oldest record");
        assert_eq!(inbox.weight_for("w-id"), INBOX_INSTANCE_CAP);
    }

    #[test]
    fn the_sink_denies_rejects_and_applies() {
        let body = r#"{"key":"pr-1","title":"t","state":"active"}"#;
        let put = Message::ThreadPut { at: 7, key: "pr-1".into(), body: body.into() };
        // Not a dispatcher: refused, with the reason delivered as an error
        // record from the same instance, keyed by the thread.
        let SinkAction::Push(r) = decide("web", false, put.clone()) else { panic!("not refused") };
        assert_eq!((r.level, r.key.as_deref(), r.at), (Level::Error, Some("thread:pr-1"), 7));
        assert!(r.msg.contains("thread put denied"), "{}", r.msg);
        assert!(r.msg.contains("`web` doesn't declare a dispatcher"), "{}", r.msg);
        // A dispatcher's valid put is applied.
        assert!(matches!(decide("web", true, put), SinkAction::Put { at: 7, .. }));
        // A schema reject carries its one-line reason back the same way.
        let bad = Message::ThreadPut { at: 8, key: "pr-1".into(), body: r#"{"key":"pr-1"}"#.into() };
        let SinkAction::Push(r) = decide("web", true, bad) else { panic!("not rejected") };
        assert!(r.msg.starts_with("thread put rejected:"), "{}", r.msg);
        // A body keyed differently from the record can't be routed.
        let swapped = r#"{"key":"pr-2","title":"t","state":"active"}"#;
        let mismatch = Message::ThreadPut { at: 8, key: "pr-1".into(), body: swapped.into() };
        let SinkAction::Push(r) = decide("web", true, mismatch) else { panic!("not rejected") };
        assert!(r.msg.contains("is not the queued key"), "{}", r.msg);
        // `rm` is authorized the same way.
        let rm = Message::ThreadRm { at: 9, key: "pr-1".into() };
        assert_eq!(decide("web", true, rm.clone()), SinkAction::Rm { key: "pr-1".into() });
        let SinkAction::Push(r) = decide("web", false, rm) else { panic!("not refused") };
        assert!(r.msg.contains("thread rm denied"), "{}", r.msg);
        // A plain notify never needs a dispatcher.
        let note = rec("hi", None, None);
        assert_eq!(decide("web", false, Message::Notify(note.clone())), SinkAction::Push(note));
    }

    #[test]
    fn the_sink_surfaces_only_what_is_new() {
        let mut inbox = Inbox::default();
        let action = |state: &str, status: &str, at: u64| {
            let body = format!(r#"{{"key":"pr-1","title":"PR 1","state":"{state}","status":"{status}"}}"#);
            decide("web", true, Message::ThreadPut { at, key: "pr-1".into(), body })
        };
        // Created active: a status line, no popup.
        let shown = inbox.apply_sink("web-id", "web", 1000, action("active", "ci", 10)).unwrap();
        assert_eq!(shown.line, "web: PR 1 — ci");
        assert!(shown.popup.is_none());
        // Re-asserted: nothing at all, not even a status line.
        assert!(inbox.apply_sink("web-id", "web", 1000, action("active", "ci", 11)).is_none());
        // Into needs-you: a popup, coalescable per thread.
        let shown = inbox.apply_sink("web-id", "web", 1000, action("needs-you", "ci", 12)).unwrap();
        let popup = shown.popup.unwrap();
        assert_eq!((popup.key.as_deref(), popup.level), (Some("thread:pr-1"), Level::Warn));
        assert!(popup.body.starts_with("PR 1"));
        // Still needs-you, new status: shown, but no second popup.
        let shown = inbox.apply_sink("web-id", "web", 1000, action("needs-you", "merged", 13)).unwrap();
        assert!(shown.popup.is_none());
        // A notify record still pops up as ever.
        let note = decide("web", false, Message::Notify(rec("hi", None, Some("https://x"))));
        let shown = inbox.apply_sink("web-id", "web", 1000, note).unwrap();
        assert_eq!(shown.line, "web: hi");
        assert_eq!(shown.popup.unwrap().body, "hi\nhttps://x");
        // `rm` applies silently.
        let rm = SinkAction::Rm { key: "pr-1".into() };
        assert!(inbox.apply_sink("web-id", "web", 1000, rm).is_none());
        assert!(find(&inbox, "pr-1").is_none());
    }

    #[test]
    fn push_is_newest_first() {
        let mut inbox = Inbox::default();
        push(&mut inbox, "a", rec("one", None, None), true);
        push(&mut inbox, "b", rec("two", None, None), true);
        assert_eq!(msgs(&inbox), [("b", "two"), ("a", "one")]);
    }

    #[test]
    fn same_key_threads_per_owner() {
        let mut inbox = Inbox::default();
        push(&mut inbox, "a", rec("pr 1 v1", Some("pr-1"), None), false);
        push(&mut inbox, "a", rec("other", None, None), false);
        push(&mut inbox, "b", rec("b pr 1", Some("pr-1"), None), false);
        push(&mut inbox, "a", rec("pr 1 v2", Some("pr-1"), None), true);
        // Same (owner, key) becomes the head of one thread, moved to the top;
        // same key from another owner is its own thread; unkeyed records never
        // thread.
        assert_eq!(msgs(&inbox), [("a", "pr 1 v2"), ("b", "b pr 1"), ("a", "other")]);
        let t = &inbox.threads[0];
        assert_eq!(t.notes.iter().map(|n| n.record.msg.as_str()).collect::<Vec<_>>(), ["pr 1 v2", "pr 1 v1"]);
        assert!(t.unread);
        assert_eq!(inbox.threads.iter().filter(|t| t.unread).count(), 1);
    }

    #[test]
    fn a_renamed_owner_keeps_its_threads() {
        let mut inbox = Inbox::default();
        inbox.push("id-1".into(), "web".into(), rec("v1", Some("k"), None), true);
        inbox.push("id-1".into(), "web-2".into(), rec("v2", Some("k"), None), true);
        assert_eq!(inbox.threads.len(), 1);
        assert_eq!(inbox.threads[0].owner_name, "web-2");
        // The badge follows the id, not whatever the instance is called now.
        assert_eq!(inbox.needs_you_for("id-1"), 1);
        assert_eq!(inbox.needs_you_for("web"), 0);
    }

    #[test]
    fn cap_counts_history_and_drops_that_owners_oldest() {
        let mut inbox = Inbox::default();
        push(&mut inbox, "quiet", rec("q0", None, None), true);
        push(&mut inbox, "noisy", rec("k0", Some("k"), None), true);
        for i in 1..INBOX_INSTANCE_CAP {
            push(&mut inbox, "noisy", rec(&i.to_string(), None, None), true);
        }
        assert_eq!(inbox.weight_for("noisy-id"), INBOX_INSTANCE_CAP);
        // A new record in the keyed thread: noisy's oldest record is the
        // thread's own first one ("k0"), dropped from its history.
        push(&mut inbox, "noisy", rec("k1", Some("k"), None), true);
        assert_eq!(inbox.weight_for("noisy-id"), INBOX_INSTANCE_CAP);
        assert_eq!(
            inbox.threads[0].notes.iter().map(|n| n.record.msg.as_str()).collect::<Vec<_>>(),
            ["k1"]
        );
        // Next: the oldest unkeyed one ("1") goes, its thread with it.
        push(&mut inbox, "noisy", rec("new", None, None), true);
        assert_eq!(inbox.weight_for("noisy-id"), INBOX_INSTANCE_CAP);
        assert!(!msgs(&inbox).contains(&("noisy", "1")));
        assert!(msgs(&inbox).contains(&("noisy", "2")));
        assert_eq!(inbox.weight_for("quiet-id"), 1);
    }

    #[test]
    fn ops_remove_by_identity() {
        let mut inbox = Inbox::default();
        push(&mut inbox, "a", rec("v1", Some("k"), None), true);
        push(&mut inbox, "a", rec("v2", Some("k"), None), true);
        push(&mut inbox, "a", rec("solo", None, None), true);
        push(&mut inbox, "b", rec("b1", None, None), true);

        let thread_id = inbox.threads.iter().find(|t| t.key.is_some()).unwrap().id;
        inbox.apply(&Op::RemoveThread(thread_id));
        assert_eq!(msgs(&inbox), [("b", "b1"), ("a", "solo")]);
        // Unknown ids are no-ops, not panics.
        inbox.apply(&Op::RemoveThread(thread_id));
        inbox.apply(&Op::MarkRead(thread_id));
        assert_eq!(msgs(&inbox), [("b", "b1"), ("a", "solo")]);

        // Leaving the Inbox reads every notify record, and only those.
        inbox.put("a-id", "a", 10, put_body("pr-1"));
        inbox.apply(&Op::MarkNotifyRead);
        let unread: Vec<Kind> = inbox.threads.iter().filter(|t| t.unread).map(|t| t.kind).collect();
        assert_eq!(unread, [Kind::Thread]);
        inbox.apply(&Op::ClearNotify);
        assert_eq!(inbox.threads.len(), 1, "the dispatcher thread stays");
    }

    /// What the Needs-you view and both badges count, and the two ops the
    /// Inbox pane leans on.
    #[test]
    fn needs_you_counts_and_targeted_ops() {
        let mut inbox = Inbox::default();
        push(&mut inbox, "a", rec("unread note", None, None), true);
        push(&mut inbox, "a", rec("read note", Some("k"), None), false);
        inbox.put("a-id", "a", 10, ThreadPut { state: State::NeedsYou, ..put_body("asks") });
        inbox.put("a-id", "a", 10, put_body("busy"));
        inbox.put("b-id", "b", 10, ThreadPut { state: State::Done, ..put_body("over") });
        // Unread notify + needs-you threads, nothing else.
        assert_eq!(inbox.needs_you(), 2);
        assert_eq!(inbox.needs_you_for("a-id"), 2);
        assert_eq!(inbox.needs_you_for("b-id"), 0);

        // Marking the unread record read drops only its own count; an unknown
        // id is a no-op, and a needs-you thread is not an unread flag.
        let note = inbox.threads.iter().find(|t| t.key.is_none()).unwrap().id;
        inbox.apply(&Op::MarkRead(note));
        inbox.apply(&Op::MarkRead(u64::MAX));
        assert_eq!(inbox.needs_you(), 1);
        let asks = find(&inbox, "asks").unwrap().id;
        inbox.apply(&Op::MarkRead(asks));
        assert_eq!(inbox.needs_you(), 1);

        // An archived thread is history: it never asks for anything.
        inbox.archive_owner("a-id");
        assert_eq!(inbox.needs_you(), 0);

        // `D` clears notify records and leaves the dispatcher's threads.
        inbox.apply(&Op::ClearNotify);
        assert_eq!(inbox.threads.len(), 3);
        assert!(inbox.threads.iter().all(|t| t.kind == Kind::Thread));
    }

    #[test]
    fn toml_roundtrip() {
        let mut inbox = Inbox::default();
        push(&mut inbox, "a", rec("v1", Some("k"), Some("https://x/1")), true);
        push(&mut inbox, "b", rec("multi\nline \"q\"", None, None), true);
        push(&mut inbox, "a", Record { level: Level::Warn, at: 7, ..rec("v2", Some("k"), None) }, true);
        push(&mut inbox, "b", rec("read", None, None), false);

        let text = inbox.to_toml().unwrap();
        assert!(text.contains("version = 2"), "{text}");
        let loaded = Inbox::from_toml(&text, &BTreeMap::new()).unwrap();
        // Thread ids survive, so selection and the open pane follow a reload.
        assert_eq!(loaded, inbox);
        // Notify-only threads write exactly what they did before threads
        // existed, so a v2 file from either build reads the same.
        for added in ["kind =", "archived =", "title =", "updated_at =", "[[thread.entry]]"] {
            assert!(!text.contains(added), "{added} in a notify-only file:\n{text}");
        }

        // Ids keep growing past the saved ones, so the cap's order holds.
        let mut loaded = loaded;
        let max_saved = inbox.threads.iter().flat_map(|t| &t.notes).map(|n| n.id).max().unwrap();
        push(&mut loaded, "a", rec("later", None, None), true);
        assert!(loaded.threads[0].notes[0].id > max_saved);

        assert!(Inbox::from_toml("", &BTreeMap::new()).unwrap().threads.is_empty());
        assert!(Inbox::from_toml(
            "version = 2\n[[thread]]\nowner = \"a\"\n[[thread.note]]\nseq = 0\nlevel = \"loud\"\nat = 0\nmsg = \"x\"\n",
            &BTreeMap::new()
        )
        .is_err());
    }

    /// Thread records survive a save/load with every field, including the
    /// nested host verb and the timeline (TOML wants tables after scalars, so
    /// field order in `SavedThread` is load-bearing).
    #[test]
    fn toml_roundtrip_with_threads() {
        let mut inbox = Inbox::default();
        push(&mut inbox, "a", rec("a note", None, None), true);
        let full = ThreadPut {
            key: "pr-1".into(),
            title: "#1 feat/x".into(),
            link: Some("https://x/1".into()),
            state: State::NeedsYou,
            status: Some("review".into()),
            child: Some("pr-1".into()),
            message: Some("multi\nline \"q\"".into()),
            actions: vec![
                Action {
                    id: "open".into(),
                    label: "Open draft".into(),
                    host: Some(thread::HostVerb::Vscode(thread::Vscode {
                        path: Some("a/b.md".into()),
                        line: Some(3),
                        col: None,
                    })),
                    ..Action::default()
                },
                Action { id: "post".into(), label: "Post".into(), notify: true, ..Action::default() },
            ],
            reply: Some(Reply { placeholder: Some("next run".into()) }),
        };
        inbox.put("a-id", "a", 7, full);
        inbox.put("a-id", "a", 9, ThreadPut { status: Some("merged".into()), ..put_body("pr-2") });
        inbox.archive_owner("a-id");

        let text = inbox.to_toml().unwrap();
        assert_eq!(Inbox::from_toml(&text, &BTreeMap::new()).unwrap(), inbox, "{text}");
    }

    /// A v1 file (no `version`, threads keyed by instance name, the key on the
    /// notes) loads with owners resolved through the state's names.
    #[test]
    fn migrates_v1() {
        let v1 = "\
[[thread]]
instance = \"web\"
unread = true
[[thread.note]]
seq = 5
level = \"warn\"
at = 7
key = \"pr-1\"
link = \"https://x/1\"
msg = \"v2\"
[[thread.note]]
seq = 2
level = \"info\"
at = 1
key = \"pr-1\"
msg = \"v1\"

[[thread]]
instance = \"gone\"
[[thread.note]]
seq = 1
level = \"info\"
at = 0
msg = \"orphan\"
";
        let mut inbox = Inbox::from_toml(v1, &names(&[("web", "web-abc1")])).unwrap();
        assert_eq!(inbox.threads.len(), 2);
        let web = &inbox.threads[0];
        assert_eq!((web.owner.as_str(), web.owner_name.as_str()), ("web-abc1", "web"));
        assert_eq!(web.key.as_deref(), Some("pr-1"));
        assert_eq!(web.notes.iter().map(|n| n.id).collect::<Vec<_>>(), [5, 2]);
        assert_eq!(web.head().unwrap().key.as_deref(), Some("pr-1"), "the key stays on every record");
        assert_eq!(web.head().unwrap().level, Level::Warn);
        assert!(web.unread);
        assert!(!web.archived);
        // An unresolved name keeps its thread under a placeholder owner, and is
        // archived: there is no instance that could ever write to it again.
        let gone = &inbox.threads[1];
        assert_eq!((gone.owner.as_str(), gone.owner_name.as_str()), ("name:gone", "gone"));
        assert!(!gone.unread);
        assert!(gone.archived);
        // Fresh thread ids, and seqs keep growing past the saved notes.
        assert_ne!(web.id, gone.id);
        push(&mut inbox, "web", rec("new", None, None), true);
        assert!(inbox.threads[0].notes[0].id > 5);
        // Re-reading what v1 became is a plain v2 load.
        let text = inbox.to_toml().unwrap();
        assert_eq!(Inbox::from_toml(&text, &BTreeMap::new()).unwrap(), inbox);
    }
}
