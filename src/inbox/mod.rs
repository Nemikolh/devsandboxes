//! The Inbox model: container notifications (`devsbd notify`,
//! docs/automations.md) and owner threads (`devsbd thread put`,
//! docs/inbox-redesign.md), newest first. This is the persisted half, out of
//! `src/tui/` because the bridge writes here while any number of dashboards
//! and API clients read it (see [`store`]); the TUI keeps only view state
//! (view, selection, the open thread).
//!
//! Two kinds of thread share the store:
//!
//! - **notify** ([`Kind::Notify`]): records sharing an `(owner, key)`, newest
//!   first, the newest being the head and the rest its history.
//! - **thread** ([`Kind::Thread`]): an owner's conversation. A small header
//!   the owner re-asserts with each put (title, state, status, actions,
//!   `compose`), and a [`feed`]: its messages, the user's replies and
//!   actions, and markers for done/reopen and state/status changes.
//!
//! `owner` is the sending instance's `instance_id`, not its name, so a rename
//! or rebuild keeps the thread (`owner_name` is only what to show). Instance
//! ids are never reused, so `devsandbox rm` can archive a gone instance's
//! threads instead of deleting them.
//!
//! An owner thread also holds its pending **events** ([`Event`]): what the
//! user did in a client (an action, a reply, done, reopen), held here until
//! the owner pulls and acks them (`devsbd events`, `commands::dispatch`).
//! They are delivery state, not history: each thread keeps at most
//! [`MAX_EVENTS`], oldest dropped, and no put touches them. Events never say
//! which client made them; the feed records that for the user alone.
//!
//! Everything here is plain state: threading, caps, unread, retention, the put
//! transition and the bridge's decision stay unit-testable without a store.

pub mod feed;
pub mod form;
pub mod message;
pub mod ops;
pub mod sanitize;
pub mod store;
pub mod thread;
pub mod view;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::devsbd::notify::{Level, Message, Record};

pub use feed::{Block, FeedItem, Field, ItemKind, Marker, MAX_FEED};
pub use form::{Answer, FormRecord, FormState};
pub use message::MessageSend;
pub use sanitize::sanitize;
pub use thread::{Action, Compose, Reply, State, ThreadPut};
pub use view::View;

/// Threads kept per owner; the least valuable goes beyond this
/// ([`Inbox::enforce_cap`]). Per owner only, so a noisy container never
/// evicts another's.
pub const THREADS_PER_OWNER: usize = 200;

/// Records kept per notify thread (newest first; the oldest drops off).
pub const MAX_NOTES: usize = MAX_FEED;

/// `inbox.json` schema version written and read by this build. Older stores
/// were `inbox.toml` (v1, v2), of which only notify records are carried over
/// ([`Inbox::import_v2`]).
pub const VERSION: u32 = 3;

/// The last `inbox.toml` version: its thread records keep their ids.
const V2: u32 = 2;

/// How long an archived or `done` thread is kept after its last change.
pub const RETENTION: u64 = 14 * 86_400;

/// Unacked events kept per thread; the oldest is dropped past this. A
/// dispatcher that never acks can't grow the store without bound, and a user
/// clicking a hundred times ahead of one is not a case worth keeping.
pub const MAX_EVENTS: usize = 100;

/// Longest reply kept, in chars; the rest is cut. Only bounds a paste.
pub const MAX_REPLY: usize = 2000;

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
    /// `devsbd thread put`: a header and a feed.
    Thread,
}

/// Owner prefix for a v1 thread whose instance name no longer resolves. Such
/// a thread has no live sender, so it loads archived.
const UNRESOLVED_OWNER: &str = "name:";

/// What an [`Event`] tells the owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventKind {
    /// A dispatcher action (no `host`, or `notify: true`); `action` = its id.
    Action,
    /// `text` = the reply.
    Reply,
    /// The thread was set done: by `d` (no `action`), or by a `done: true`
    /// action (`action` = its id). One event either way, so a dispatcher
    /// handles "done" in one place and still knows which button it was.
    Done,
    /// `u` on a done thread.
    Reopen,
    /// A form was submitted: `message`, `form`, and every question's
    /// `answers` (defaults filled in).
    Submit,
}

impl EventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::Action => "action",
            EventKind::Reply => "reply",
            EventKind::Done => "done",
            EventKind::Reopen => "reopen",
            EventKind::Submit => "submit",
        }
    }
}

/// One pending event for a thread's owner. No client: an owner must not be
/// able to tell where an event came from (docs/inbox-redesign.md,
/// *Principles*).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// `e-<unix secs:010>-<4 hex>` (`control::valid_event_id`), unique across
    /// the store: what the owner acks, and what stops two dispatchers' reads
    /// (or two dashboards) handling one click twice.
    pub id: String,
    /// Arrival order across the inbox, like [`FeedItem::seq`]: orders events
    /// across threads within one second. Never sent to the owner.
    pub seq: u64,
    pub kind: EventKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// `submit`: the message id and the form id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub form: Option<String>,
    /// `submit`: [`form::answers_json`], the shape the owner receives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answers: Option<serde_json::Value>,
    pub at: u64,
}

impl Event {
    /// An event of `kind` with no payload, not yet stamped: [`Inbox::apply`]
    /// mints its id, seq and time under the store lock.
    fn of(kind: EventKind) -> Event {
        Event { id: String::new(), seq: 0, kind, action: None, text: None, message: None, form: None, answers: None, at: 0 }
    }
}

/// A keyed record's history, or a single unkeyed record, or one owner
/// thread. Owner threads carry no notes, so nothing may index `notes`
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
    /// Newest first; empty on an owner thread.
    pub notes: Vec<Note>,
    // The header, replaced wholesale by each put.
    pub title: String,
    pub link: Option<String>,
    pub state: Option<State>,
    pub status: Option<String>,
    pub child: Option<String>,
    pub actions: Vec<Action>,
    /// Present when the thread takes replies.
    pub compose: Option<Compose>,
    /// Unix seconds of the last put; 0 on a notify thread, which dates itself
    /// from its head record.
    pub updated_at: u64,
    /// First-insert order (oldest first); empty on a notify thread.
    pub feed: Vec<FeedItem>,
    /// Unacked events for the owner, oldest first.
    pub events: Vec<Event>,
}

impl Thread {
    /// The newest record, or `None` on an owner thread (which has none).
    pub fn head(&self) -> Option<&Record> {
        self.notes.first().map(|n| &n.record)
    }

    /// When the thread last changed: the newest put, else its head record.
    pub fn changed_at(&self) -> u64 {
        self.updated_at.max(self.head().map_or(0, |r| r.at))
    }

    /// Whether the thread is waiting on the user: what the **Needs you** view
    /// and both badges count. An owner thread earns it by saying so
    /// (`needs-you`); a plain notify record by being unread, so a
    /// non-owner's `notify` still surfaces. An archived thread has no live
    /// owner, so it never counts.
    pub fn needs_you(&self) -> bool {
        !self.archived
            && match self.kind {
                Kind::Notify => self.unread,
                Kind::Thread => self.state == Some(State::NeedsYou),
            }
    }

    /// The thread as its owner last put it (`devsbd thread ls`): the put
    /// shape, so a dispatcher can feed it straight back.
    pub fn to_put(&self) -> ThreadPut {
        ThreadPut {
            key: self.key.clone().unwrap_or_default(),
            title: self.title.clone(),
            link: self.link.clone(),
            state: self.state.unwrap_or_default(),
            status: self.status.clone(),
            child: self.child.clone(),
            actions: self.actions.clone(),
            compose: self.compose.clone(),
            // v2 put compat, removed in step 13b: what a v2 dispatcher put.
            message: feed::header_message(&self.feed),
            reply: self.compose.as_ref().map(|c| Reply { placeholder: c.placeholder.clone() }),
        }
    }
}

/// A mutation a client asks the store to apply. Row indices can't cross the
/// process boundary (another dashboard may have changed the list), so every
/// variant names what it touches by a stable id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    /// Mark one thread read: the Inbox marks what the user actually opened,
    /// not everything the tab happened to show.
    MarkRead(u64),
    /// Mark every notify record read: they were on screen when the user left
    /// the Inbox. Owner threads are only read by opening them, since their
    /// state, not their unread flag, is what asks for attention.
    MarkNotifyRead,
    /// Dismiss a whole thread (its history with it).
    RemoveThread(u64),
    /// Dismiss every notify record. Owner threads are state an owner
    /// re-asserts, so clearing them would only make them come back.
    ClearNotify,
    // The user ops below each add feed items and enqueue an event for the
    // owner. Their ids and times are minted under the store lock
    // ([`Inbox::apply`]), so a dashboard never applies them to its own copy.
    /// `1`-`9` on a dispatcher action (the dispatcher half of a button; the
    /// host half runs in the dashboard). A `done: true` action also sets the
    /// thread done.
    Act { thread: u64, action: String },
    /// `r`: a free-text reply, on a thread that allows them.
    Reply { thread: u64, text: String },
    /// `d`: set the thread done.
    MarkDone(u64),
    /// `u`: reopen a done thread (back to `active` until the dispatcher says
    /// otherwise: it owns what the thread means).
    Reopen(u64),
    /// Merge `answers` (any subset of the questions) into the draft of
    /// message `message`'s open form. No event, no feed item.
    SaveDraft { thread: u64, message: String, answers: BTreeMap<String, Answer> },
    /// Submit message `message`'s open form: `answers` over the draft over
    /// the defaults ([`form::resolve_submission`]).
    Submit { thread: u64, message: String, answers: BTreeMap<String, Answer> },
}

impl Op {
    /// Whether this op enqueues an event, so only the store may apply it.
    pub fn enqueues_event(&self) -> bool {
        matches!(self, Op::Act { .. } | Op::Reply { .. } | Op::MarkDone(_) | Op::Reopen(_) | Op::Submit { .. })
    }
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
        // An owner thread with the same key is a different object; a notify
        // record never joins it.
        let existing = key.as_ref().and_then(|key| {
            self.threads
                .iter()
                .position(|t| t.owner == owner && t.kind == Kind::Notify && t.key.as_ref() == Some(key))
        });
        let thread = match existing {
            Some(pos) => {
                let mut t = self.threads.remove(pos);
                t.notes.insert(0, note);
                t.notes.truncate(MAX_NOTES);
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
        let id = thread.id;
        self.threads.insert(0, thread);
        self.enforce_cap(&owner, id);
    }

    /// Apply one `devsbd thread put` from `owner`, at container time `at`.
    /// Pure: the store decides what to write from the outcome.
    ///
    /// A put only touches the header. State and status changes leave a
    /// marker in the feed (status changes collapse, [`feed::push`]); nothing
    /// else does. A put that changes nothing returns [`PutOutcome::Unchanged`]
    /// without touching a field, so the store's content comparison skips the
    /// write and the file's mtime doesn't move. That idempotence is the
    /// point: a dispatcher is meant to re-assert every thread on every pass.
    pub fn put(&mut self, owner: &str, owner_name: &str, at: u64, put: ThreadPut) -> PutOutcome {
        let compose = put.compose();
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
            // No markers: nothing changed yet, the header says it all.
            // v2 put compat, removed in step 13b.
            let header = feed::header_change(&thread.feed, put.message.as_deref());
            self.apply_header(&mut thread, header, at);
            apply_put(&mut thread, put, compose);
            let entered = thread.state == Some(State::NeedsYou);
            self.threads.insert(0, thread);
            self.enforce_cap(owner, id);
            return PutOutcome::Applied { created: true, entered_needs_you: entered };
        };

        let old = &self.threads[pos];
        // v2 put compat, removed in step 13b.
        let header = feed::header_change(&old.feed, put.message.as_deref());
        let state = old.state != Some(put.state);
        let status = old.status != put.status;
        let cosmetic = old.owner_name != owner_name
            || old.title != put.title
            || old.link != put.link
            || old.child != put.child
            || old.actions != put.actions
            || old.compose != compose
            || old.archived;
        if !(state || status || cosmetic || header != feed::HeaderChange::Unchanged) {
            return PutOutcome::Unchanged;
        }
        let entered = state && put.state == State::NeedsYou;
        // A new message is news; an edit or a withdrawal isn't, nor is a
        // cosmetic change (a new label, a rename).
        let news = state || status || matches!(header, feed::HeaderChange::Insert(_));
        let mut markers = Vec::new();
        if state {
            markers.push(Marker::State { from: old.state.unwrap_or_default(), to: put.state });
        }
        if status {
            markers.push(Marker::Status { from: old.status.clone(), to: put.status.clone() });
        }
        let mut thread = self.threads.remove(pos);
        thread.owner_name = owner_name.to_string();
        // A put from a live container means the owner is back; only `rm`
        // archives, and ids are never reused.
        thread.archived = false;
        thread.unread = thread.unread || news;
        thread.updated_at = at;
        self.apply_header(&mut thread, header, at);
        for marker in markers {
            let seq = self.next_id();
            feed::push(&mut thread.feed, FeedItem { seq, at, kind: ItemKind::Marker(marker) });
        }
        feed::cap(&mut thread.feed, MAX_FEED);
        apply_put(&mut thread, put, compose);
        let id = thread.id;
        self.threads.insert(0, thread);
        self.enforce_cap(owner, id);
        PutOutcome::Applied { created: false, entered_needs_you: entered }
    }

    /// v2 put compat, removed in step 13b: apply what a put's `message` does
    /// to the `header-message` feed item.
    fn apply_header(&mut self, thread: &mut Thread, change: feed::HeaderChange, at: u64) {
        let seq = match change {
            feed::HeaderChange::Insert(_) => self.next_id(),
            _ => 0,
        };
        feed::apply_header_change(&mut thread.feed, change, seq, at);
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
    /// per-owner cap bounds those. A done thread with unacked events stays
    /// until its owner acks them (they're delivery state, e.g. the `done`
    /// itself); an archived owner is gone and can never ack, so it doesn't.
    pub fn prune(&mut self, now: u64) -> usize {
        let before = self.threads.len();
        self.threads.retain(|t| {
            let retired = t.archived || (t.state == Some(State::Done) && t.events.is_empty());
            !(retired && now.saturating_sub(t.changed_at()) > RETENTION)
        });
        before - self.threads.len()
    }

    /// Apply one decoded [`SinkAction`] from the bridge and say what to
    /// surface. Retention runs here because this is the one write path every
    /// container message takes, so a store nobody else touches still ages out;
    /// for the same reason its text is [`sanitize`]d here, before it is stored
    /// or shown in the status line or a desktop popup.
    pub fn apply_sink(&mut self, owner: &str, owner_name: &str, now: u64, action: SinkAction) -> Option<Shown> {
        self.prune(now);
        match sanitize_action(action) {
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
                // v2 put compat, removed in step 13b: the popup quotes the
                // put's message.
                let body = match &put.message {
                    Some(message) => format!("{}\n{message}", put.title),
                    None => put.title.clone(),
                };
                match self.put(owner, owner_name, at, put) {
                    // No marker, no unread, no write, and nothing on the
                    // status line: a re-asserted thread is invisible.
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
            SinkAction::Send { at, send } => {
                let key = send.thread.clone();
                let text = feed::markdown_of(&send.blocks);
                match self.send(owner, at, send) {
                    // Refused like a bad body: the owner's author sees why.
                    None => {
                        let why = format!("thread send rejected: no thread `{key}`; put it first");
                        self.apply_sink(owner, owner_name, now, refused(at, &key, &why))
                    }
                    // A repeat is invisible; an edit is quiet (no unread, no
                    // popup, no status line), unless it opened a form.
                    Some(feed::Sent::Unchanged | feed::Sent::Edited { form_opened: false }) => None,
                    Some(feed::Sent::Inserted | feed::Sent::Edited { form_opened: true }) => {
                        let t = self.owner_thread(owner, &key).map(|pos| &self.threads[pos])?;
                        let summary = one_line(&text);
                        let line = match summary.is_empty() {
                            true => format!("{owner_name}: {}", t.title),
                            false => format!("{owner_name}: {} — {summary}", t.title),
                        };
                        // A new message (or a form entering the feed) on a
                        // thread waiting on the user is what earns a popup.
                        let popup = (t.state == Some(State::NeedsYou)).then(|| ShownPopup {
                            key: Some(format!("thread:{key}")),
                            level: Level::Warn,
                            body: match text.is_empty() {
                                true => t.title.clone(),
                                false => format!("{}\n{text}", t.title),
                            },
                        });
                        Some(Shown { line, popup })
                    }
                }
            }
            SinkAction::Withdraw { thread, id } => {
                self.withdraw(owner, &thread, &id);
                None
            }
        }
    }

    /// Position of `owner`'s owner thread `key`.
    fn owner_thread(&self, owner: &str, key: &str) -> Option<usize> {
        self.threads
            .iter()
            .position(|t| t.owner == owner && t.kind == Kind::Thread && t.key.as_deref() == Some(key))
    }

    /// Apply one `devsbd thread send` from `owner`, at container time `at`
    /// (the message's time if it's new, like a put's markers). `None` when
    /// the owner has no such thread: a message needs its header first.
    ///
    /// See [`feed::send`] for insert / replace / no-op (and what a re-send
    /// does to a form). A new message marks the thread unread and moves it to
    /// the top, and so does an edit that opened a form; any other edit leaves
    /// both alone (it isn't news), only refreshing `updated_at` for retention.
    pub fn send(&mut self, owner: &str, at: u64, send: MessageSend) -> Option<feed::Sent> {
        let pos = self.owner_thread(owner, &send.thread)?;
        let mut thread = self.threads.remove(pos);
        let sent = feed::send(&mut thread.feed, &send.id, send.blocks, || self.next_id(), at);
        match sent {
            feed::Sent::Unchanged => self.threads.insert(pos, thread),
            feed::Sent::Edited { form_opened: false } => {
                thread.updated_at = thread.updated_at.max(at);
                self.threads.insert(pos, thread);
            }
            feed::Sent::Inserted | feed::Sent::Edited { form_opened: true } => {
                feed::cap(&mut thread.feed, MAX_FEED);
                thread.unread = true;
                thread.updated_at = thread.updated_at.max(at);
                self.threads.insert(0, thread);
            }
        }
        Some(sent)
    }

    /// `devsbd thread withdraw <thread> <id>`: tag the message withdrawn,
    /// keeping it. An unknown thread or id is a silent no-op (the owner may
    /// be cleaning up after a send that never landed). Whether it changed.
    pub fn withdraw(&mut self, owner: &str, thread: &str, id: &str) -> bool {
        match self.owner_thread(owner, thread) {
            Some(pos) => feed::withdraw(&mut self.threads[pos].feed, id),
            None => false,
        }
    }

    /// Apply one client-requested [`Op`] at unix time `now` (the stamp of any
    /// feed item and event it adds). `client` (`tui`, `cli`, `api:<name>`)
    /// is recorded on the user's feed items, for the user's audit only.
    pub fn apply(&mut self, op: &Op, now: u64, client: &str) {
        let client = client.to_string();
        match op {
            Op::Act { thread, action } => self.user_op(*thread, now, |t| {
                let a = t.actions.iter().find(|a| &a.id == action)?;
                // A host-only button never reaches the owner; refusing it
                // here keeps a stale or forged op from inventing one.
                if !a.enqueues_event() {
                    return None;
                }
                let pressed = ItemKind::Action { action: a.id.clone(), label: a.label.clone(), client: client.clone() };
                Some(match a.done {
                    // What was pressed, then what it did.
                    true => UserOp {
                        state: Some(State::Done),
                        items: vec![pressed, ItemKind::Marker(Marker::Done { client })],
                        event: Event { action: Some(a.id.clone()), ..Event::of(EventKind::Done) },
                        submitted: None,
                    },
                    false => UserOp {
                        state: None,
                        items: vec![pressed],
                        event: Event { action: Some(a.id.clone()), ..Event::of(EventKind::Action) },
                        submitted: None,
                    },
                })
            }),
            Op::Reply { thread, text } => self.user_op(*thread, now, |t| {
                let text = text.trim();
                if t.compose.is_none() || text.is_empty() {
                    return None;
                }
                let text: String = text.chars().take(MAX_REPLY).collect();
                Some(UserOp {
                    state: None,
                    items: vec![ItemKind::Reply { text: text.clone(), client }],
                    event: Event { text: Some(text), ..Event::of(EventKind::Reply) },
                    submitted: None,
                })
            }),
            // Only a real transition counts: a second `d` (or `u` on a live
            // thread) would hand the owner an event for nothing.
            Op::MarkDone(thread) => self.user_op(*thread, now, |t| {
                (t.state != Some(State::Done)).then(|| UserOp {
                    state: Some(State::Done),
                    items: vec![ItemKind::Marker(Marker::Done { client })],
                    event: Event::of(EventKind::Done),
                    submitted: None,
                })
            }),
            Op::Reopen(thread) => self.user_op(*thread, now, |t| {
                (t.state == Some(State::Done)).then(|| UserOp {
                    state: Some(State::Active),
                    items: vec![ItemKind::Marker(Marker::Reopen { client })],
                    event: Event::of(EventKind::Reopen),
                    submitted: None,
                })
            }),
            Op::SaveDraft { thread, message, answers } => {
                let Some(t) = self.threads.iter_mut().find(|t| t.id == *thread) else { return };
                if t.kind != Kind::Thread || t.archived {
                    return;
                }
                let Some((form, record)) = feed::form_of(&t.feed, message) else { return };
                let Ok(answers) = form::validate_draft(form, record, answers) else { return };
                if let Some(record) = feed::form_record_mut(&mut t.feed, message) {
                    record.draft.extend(answers);
                }
            }
            Op::Submit { thread, message, answers } => self.user_op(*thread, now, |t| {
                let (form, record) = feed::form_of(&t.feed, message)?;
                let resolved = form::resolve_submission(form, record, answers).ok()?;
                Some(UserOp {
                    state: None,
                    items: vec![ItemKind::Submission {
                        message: message.clone(),
                        form: form.id.clone(),
                        answers: resolved.clone(),
                        client: client.clone(),
                    }],
                    event: Event {
                        message: Some(message.clone()),
                        form: Some(form.id.clone()),
                        answers: Some(form::answers_json(form, &resolved)),
                        ..Event::of(EventKind::Submit)
                    },
                    submitted: Some((message.clone(), FormState::Submitted { at: now, answers: resolved, client })),
                })
            }),
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

    /// Apply a user op to owner thread `id`: `decide` says what it does
    /// (`None`: nothing, e.g. a stale action id). An archived thread's owner
    /// is gone, so it takes no new events; a notify thread has no owner to
    /// answer it.
    fn user_op(&mut self, id: u64, now: u64, decide: impl FnOnce(&Thread) -> Option<UserOp>) {
        let Some(pos) = self.threads.iter().position(|t| t.id == id) else { return };
        let t = &self.threads[pos];
        if t.kind != Kind::Thread || t.archived {
            return;
        }
        let Some(op) = decide(t) else { return };
        let event_id = self.mint_event_id(now);
        let seqs: Vec<u64> = op.items.iter().map(|_| self.next_id()).collect();
        let event_seq = self.next_id();
        let t = &mut self.threads[pos];
        if let Some(state) = op.state {
            // A `done` thread's child is marked done too, but that flag lives
            // in `state.toml`: the dashboard queues it next to this op.
            t.state = Some(state);
        }
        t.updated_at = t.updated_at.max(now);
        if let Some((message, state)) = op.submitted {
            if let Some(record) = feed::form_record_mut(&mut t.feed, &message) {
                record.state = state;
                record.draft.clear();
            }
        }
        for (seq, kind) in seqs.into_iter().zip(op.items) {
            feed::push(&mut t.feed, FeedItem { seq, at: now, kind });
        }
        feed::cap(&mut t.feed, MAX_FEED);
        t.events.push(Event { id: event_id, seq: event_seq, at: now, ..op.event });
        if t.events.len() > MAX_EVENTS {
            let extra = t.events.len() - MAX_EVENTS;
            t.events.drain(..extra);
        }
    }

    /// A fresh event id for time `now`, unique in the store. The 4 hex digits
    /// come from std's per-process random hash keys: cheap, no dependency,
    /// and a collision (same second, same digits) is simply retried, which
    /// always ends since events are bounded far below 65536 per second.
    fn mint_event_id(&self, now: u64) -> String {
        use std::hash::{BuildHasher, Hasher};
        loop {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u64(now);
            let id = format!("e-{now:010}-{:04x}", h.finish() as u16);
            if !self.threads.iter().any(|t| t.events.iter().any(|e| e.id == id)) {
                return id;
            }
        }
    }

    /// `owner`'s pending events with their thread keys, oldest first: all of
    /// them, not just new ones, since delivery is at least once until acked.
    pub fn events_for(&self, owner: &str) -> Vec<(String, Event)> {
        let mut out: Vec<(String, Event)> = self
            .threads
            .iter()
            .filter(|t| t.owner == owner && t.kind == Kind::Thread)
            .flat_map(|t| {
                let key = t.key.clone().unwrap_or_default();
                t.events.iter().map(move |e| (key.clone(), e.clone()))
            })
            .collect();
        out.sort_by_key(|(_, e)| e.seq);
        out
    }

    /// Drop `owner`'s events with these ids; how many were dropped. Unknown
    /// ids (already acked, or another owner's) are skipped, so a retried ack
    /// is harmless and an owner can't ack what isn't its own.
    pub fn ack(&mut self, owner: &str, ids: &[String]) -> usize {
        let mut dropped = 0;
        for t in self.threads.iter_mut().filter(|t| t.owner == owner) {
            let before = t.events.len();
            t.events.retain(|e| !ids.contains(&e.id));
            dropped += before - t.events.len();
        }
        dropped
    }

    /// `owner`'s live owner threads, for `devsbd thread ls`: what a
    /// dispatcher that lost its own state can read back. Archived ones are
    /// history it can no longer change.
    pub fn threads_for(&self, owner: &str) -> Vec<&Thread> {
        self.threads
            .iter()
            .filter(|t| t.owner == owner && t.kind == Kind::Thread && !t.archived)
            .collect()
    }

    /// Keep `owner` within [`THREADS_PER_OWNER`], never evicting `keep` (the
    /// thread just written). Retired threads are the cheapest to lose:
    /// archived first, then `done` ones, then the least recently changed
    /// with no pending events. A thread holding unacked events is never
    /// evicted (they're the user's answers, not yet pulled), so the cap goes
    /// soft rather than lose one; an archived thread goes regardless, since
    /// its owner can never pull them.
    fn enforce_cap(&mut self, owner: &str, keep: u64) {
        while self.threads.iter().filter(|t| t.owner == owner).count() > THREADS_PER_OWNER {
            let mine = |t: &Thread| t.owner == owner && t.id != keep;
            let pos = self
                .oldest_thread(|t| mine(t) && t.archived)
                .or_else(|| self.oldest_thread(|t| mine(t) && t.state == Some(State::Done) && t.events.is_empty()))
                .or_else(|| self.oldest_thread(|t| mine(t) && t.events.is_empty()));
            let Some(pos) = pos else { return };
            self.threads.remove(pos);
        }
    }

    /// Index of the least recently changed thread matching `pick`.
    fn oldest_thread(&self, pick: impl Fn(&Thread) -> bool) -> Option<usize> {
        self.threads
            .iter()
            .enumerate()
            .filter(|(_, t)| pick(t))
            .min_by_key(|(_, t)| t.changed_at())
            .map(|(i, _)| i)
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

    /// The `inbox.json` contents (always v3).
    pub fn to_json(&self) -> Result<String, String> {
        let saved = SavedInbox {
            version: VERSION,
            next_id: self.next_id,
            threads: self
                .threads
                .iter()
                .map(|t| SavedThread {
                    id: t.id,
                    owner: t.owner.clone(),
                    owner_name: t.owner_name.clone(),
                    key: t.key.clone(),
                    kind: t.kind,
                    archived: t.archived,
                    unread: t.unread,
                    title: t.title.clone(),
                    link: t.link.clone(),
                    state: t.state,
                    status: t.status.clone(),
                    child: t.child.clone(),
                    actions: t.actions.clone(),
                    compose: t.compose.clone(),
                    updated_at: t.updated_at,
                    feed: t.feed.clone(),
                    events: t.events.clone(),
                    notes: t
                        .notes
                        .iter()
                        .map(|n| SavedNote {
                            seq: n.id,
                            level: n.record.level.as_str().to_string(),
                            at: n.record.at,
                            link: n.record.link.clone(),
                            msg: n.record.msg.clone(),
                        })
                        .collect(),
                })
                .collect(),
        };
        serde_json::to_string_pretty(&saved).map(|mut s| {
            s.push('\n');
            s
        }).map_err(|e| e.to_string())
    }

    /// Parse `inbox.json` contents. Any version but [`VERSION`] is an error:
    /// a store written by a newer build is not this one's to rewrite.
    pub fn from_json(text: &str) -> Result<Inbox, String> {
        #[derive(Deserialize)]
        struct Probe {
            version: Option<u32>,
        }
        let probe: Probe = serde_json::from_str(text).map_err(|e| e.to_string())?;
        match probe.version {
            Some(VERSION) => {}
            Some(v) => return Err(format!("store version {v}, this build reads version {VERSION}")),
            None => return Err("no `version`".into()),
        }
        let saved: SavedInbox = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let mut inbox = Inbox::default();
        for t in saved.threads {
            let notes = t
                .notes
                .into_iter()
                .map(|n| {
                    let level = Level::parse(&n.level).ok_or_else(|| format!("bad level `{}`", n.level))?;
                    let record = Record { level, key: t.key.clone(), link: n.link, msg: n.msg, at: n.at };
                    Ok(Note { id: n.seq, record })
                })
                .collect::<Result<Vec<_>, String>>()?;
            inbox.threads.push(Thread {
                id: t.id,
                owner: t.owner,
                owner_name: t.owner_name,
                key: t.key,
                kind: t.kind,
                archived: t.archived,
                unread: t.unread,
                notes,
                title: t.title,
                link: t.link,
                state: t.state,
                status: t.status,
                child: t.child,
                actions: t.actions,
                compose: t.compose,
                updated_at: t.updated_at,
                feed: t.feed,
                events: t.events,
            });
        }
        inbox.next_id = saved.next_id.max(inbox.max_id().map_or(0, |m| m + 1));
        Ok(inbox)
    }

    /// Carry a v1/v2 `inbox.toml` over: its **notify** threads (notes,
    /// unread, archived) and nothing else. Owner threads are dropped: they
    /// are projections a dispatcher re-puts every pass, and their v2 shape
    /// (a header `message`, per-field timeline entries) has no v3 meaning.
    /// `names` maps instance names (state keys) to `instance_id`s, and is
    /// only used by a v1 file, which identified threads by name: a name that
    /// no longer resolves keeps its thread, archived, under the placeholder
    /// owner `name:<instance>`.
    pub fn import_v2(text: &str, names: &BTreeMap<String, String>) -> Result<Inbox, String> {
        let saved: V2Inbox = toml::from_str(text).map_err(|e| e.to_string())?;
        let mut inbox = Inbox::default();
        for t in saved.threads.into_iter().filter(|t| t.kind == Kind::Notify) {
            // v1 repeated the key on every note; it belongs to the thread.
            let v1_key = t.notes.first().and_then(|n| n.key.clone());
            let mut notes = t
                .notes
                .into_iter()
                .map(|n| {
                    let level = Level::parse(&n.level).ok_or_else(|| format!("bad level `{}`", n.level))?;
                    Ok(Note { id: n.seq, record: Record { level, key: None, link: n.link, msg: n.msg, at: n.at } })
                })
                .collect::<Result<Vec<_>, String>>()?;
            if notes.is_empty() {
                continue;
            }
            notes.truncate(MAX_NOTES);
            // v1: `instance` is a name; v2 carries the id and the key.
            let (owner, owner_name, key) = match t.instance {
                Some(name) => {
                    let owner =
                        names.get(&name).cloned().unwrap_or_else(|| format!("{UNRESOLVED_OWNER}{name}"));
                    (owner, name, v1_key)
                }
                None => (t.owner, t.owner_name, t.key),
            };
            let id = if saved.version >= V2 { t.id } else { inbox.next_id() };
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
                unread: t.unread,
                notes,
                ..Thread::default()
            });
        }
        inbox.next_id = inbox.next_id.max(inbox.max_id().map_or(0, |m| m + 1));
        Ok(inbox)
    }

    /// The largest id or seq anything holds, so the counter can keep growing
    /// past everything loaded: arrival order and thread identity stay unique.
    fn max_id(&self) -> Option<u64> {
        self.threads
            .iter()
            .flat_map(|t| {
                std::iter::once(t.id)
                    .chain(t.notes.iter().map(|n| n.id))
                    .chain(t.feed.iter().map(|i| i.seq))
                    .chain(t.events.iter().map(|e| e.seq))
            })
            .max()
    }
}

/// What one user op does to a thread: an optional state change, the feed
/// items, the event for the owner (unstamped, [`Event::of`]), and a form
/// the op submitted (message id, its new state).
struct UserOp {
    state: Option<State>,
    items: Vec<ItemKind>,
    event: Event,
    submitted: Option<(String, FormState)>,
}

/// `2026-10-02T12:00:01Z` for unix time `at`: the `at` of an event as the
/// owner sees it (std has no dates; Howard Hinnant's `civil_from_days`).
pub fn rfc3339(at: u64) -> String {
    let (days, secs) = ((at / 86_400) as i64, at % 86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
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

/// Copy a put's header onto a thread. Actions and compose are replaced
/// wholesale: the put is the owner's whole current view of the header.
fn apply_put(thread: &mut Thread, put: ThreadPut, compose: Option<Compose>) {
    thread.title = put.title;
    thread.link = put.link;
    thread.state = Some(put.state);
    thread.status = put.status;
    thread.child = put.child;
    thread.actions = put.actions;
    thread.compose = compose;
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
    /// `thread send`, schema-checked; the thread may still not exist (the
    /// store is what knows, [`Inbox::apply_sink`]).
    Send { at: u64, send: MessageSend },
    Withdraw { thread: String, id: String },
}

/// Decide what to do with `message` from `instance` (its state key).
/// `declares_inbox` is `commands::dispatch::declares_inbox`, which the
/// caller only evaluates for thread messages (it loads state).
///
/// A refusal is never silent: it becomes an `error` record from the same
/// instance, keyed by the thread, so the owner's author sees the reason
/// in the Inbox instead of a message that quietly never appears.
pub fn decide(instance: &str, declares_inbox: bool, message: Message) -> SinkAction {
    let message = match message {
        Message::Notify(record) => return SinkAction::Push(record),
        thread_op => thread_op,
    };
    let verb = match &message {
        Message::Notify(_) => unreachable!("returned above"),
        Message::ThreadPut { .. } => "put",
        Message::ThreadRm { .. } => "rm",
        Message::ThreadSend { .. } => "send",
        Message::ThreadWithdraw { .. } => "withdraw",
    };
    let (at, key) = (message.at(), message.key().unwrap_or_default().to_string());
    if !declares_inbox {
        let why = format!("`{instance}` doesn't declare inbox = true (add it to the sandbox in devsandboxes.toml)");
        return refused(at, &key, &format!("thread {verb} denied: {why}"));
    }
    let rejected = |why: &str| refused(at, &key, &format!("thread {verb} rejected: {why}"));
    match message {
        Message::Notify(_) => unreachable!("returned above"),
        Message::ThreadRm { key, .. } => SinkAction::Rm { key },
        Message::ThreadWithdraw { key, id, .. } => SinkAction::Withdraw { thread: key, id },
        Message::ThreadPut { body, .. } => match thread::parse(&body) {
            Ok(put) if put.key != key => rejected(&format!("body key `{}` is not the queued key", put.key)),
            Ok(put) => SinkAction::Put { at, put },
            Err(why) => rejected(&why),
        },
        Message::ThreadSend { id, body, .. } => match message::parse(&body) {
            Ok(send) if send.thread != key => {
                rejected(&format!("body thread `{}` is not the queued thread", send.thread))
            }
            Ok(send) if send.id != id => rejected(&format!("body id `{}` is not the queued id", send.id)),
            Ok(send) => SinkAction::Send { at, send },
            Err(why) => rejected(&why),
        },
    }
}

/// Every container-provided string in `action`, [`sanitize`]d. Keys and ids
/// are already held to `[a-z0-9-]` (put) but go through too, so no field can
/// be missed when one is added.
fn sanitize_action(action: SinkAction) -> SinkAction {
    use thread::HostVerb;
    let opt = |s: Option<String>| s.map(|s| sanitize(&s));
    match action {
        SinkAction::Push(r) => SinkAction::Push(Record {
            key: opt(r.key),
            link: opt(r.link),
            msg: sanitize(&r.msg),
            ..r
        }),
        SinkAction::Rm { key } => SinkAction::Rm { key: sanitize(&key) },
        SinkAction::Withdraw { thread, id } => SinkAction::Withdraw { thread: sanitize(&thread), id: sanitize(&id) },
        SinkAction::Send { at, send } => {
            let blocks = send
                .blocks
                .into_iter()
                .map(|b| match b {
                    Block::Markdown { text } => Block::Markdown { text: sanitize(&text) },
                    Block::Fields { items } => Block::Fields {
                        items: items
                            .into_iter()
                            .map(|f| Field { label: sanitize(&f.label), value: sanitize(&f.value) })
                            .collect(),
                    },
                    Block::Form(f) => Block::Form(form::sanitized(f)),
                })
                .collect();
            let send = MessageSend { thread: sanitize(&send.thread), id: sanitize(&send.id), blocks };
            SinkAction::Send { at, send }
        }
        SinkAction::Put { at, put } => {
            let actions = put
                .actions
                .into_iter()
                .map(|a| {
                    let host = a.host.map(|h| match h {
                        HostVerb::Vscode(v) => HostVerb::Vscode(thread::Vscode { path: opt(v.path), ..v }),
                        HostVerb::Open(o) => HostVerb::Open(thread::Open { url: sanitize(&o.url) }),
                        other => other,
                    });
                    Action { id: sanitize(&a.id), label: sanitize(&a.label), host, ..a }
                })
                .collect();
            let put = ThreadPut {
                key: sanitize(&put.key),
                title: sanitize(&put.title),
                link: opt(put.link),
                status: opt(put.status),
                child: opt(put.child),
                actions,
                compose: put.compose.map(|c| Compose { placeholder: opt(c.placeholder), hint: opt(c.hint) }),
                // v2 put compat, removed in step 13b.
                message: opt(put.message),
                reply: put.reply.map(|r| Reply { placeholder: opt(r.placeholder) }),
                ..put
            };
            SinkAction::Put { at, put }
        }
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

/// `inbox.json` schema: threads newest first, each with its notes newest
/// first and its feed oldest first. Separate from the model because a note
/// is stored flat (the model wraps a [`Record`], which carries its thread's
/// key again); feed items, actions and events are stored as they are.
#[derive(Serialize, Deserialize)]
struct SavedInbox {
    version: u32,
    /// The id counter, so ids stay unique even past dropped threads.
    #[serde(default)]
    next_id: u64,
    #[serde(default)]
    threads: Vec<SavedThread>,
}

#[derive(Serialize, Deserialize)]
struct SavedThread {
    id: u64,
    owner: String,
    owner_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key: Option<String>,
    #[serde(default)]
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    actions: Vec<Action>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    compose: Option<Compose>,
    #[serde(default, skip_serializing_if = "is_zero")]
    updated_at: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    feed: Vec<FeedItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    events: Vec<Event>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    notes: Vec<SavedNote>,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

#[derive(Serialize, Deserialize)]
struct SavedNote {
    /// Arrival order across the inbox.
    seq: u64,
    level: String,
    at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    link: Option<String>,
    msg: String,
}

/// The part of a v1/v2 `inbox.toml` [`Inbox::import_v2`] reads: notify
/// threads. Everything an owner thread carried (`message`, `[[thread.entry]]`,
/// `[[thread.action]]`, …) is ignored, as are unknown keys. A v1 file is a
/// v2 one with `instance` instead of `owner`/`owner_name` and the key on the
/// notes.
#[derive(Deserialize)]
struct V2Inbox {
    /// Absent in v1 files.
    #[serde(default)]
    version: u32,
    #[serde(default, rename = "thread")]
    threads: Vec<V2Thread>,
}

#[derive(Deserialize)]
struct V2Thread {
    #[serde(default)]
    id: u64,
    #[serde(default)]
    owner: String,
    #[serde(default)]
    owner_name: String,
    #[serde(default)]
    key: Option<String>,
    /// v1 only: the owner's state key.
    #[serde(default)]
    instance: Option<String>,
    #[serde(default)]
    kind: Kind,
    #[serde(default)]
    archived: bool,
    #[serde(default)]
    unread: bool,
    #[serde(default, rename = "note")]
    notes: Vec<V2Note>,
}

#[derive(Deserialize)]
struct V2Note {
    seq: u64,
    level: String,
    at: u64,
    /// v1 only: the key now lives on the thread.
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
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
            ..ThreadPut::default()
        }
    }

    /// A thread's feed as short strings, oldest first.
    fn feed(t: &Thread) -> Vec<String> {
        t.feed
            .iter()
            .map(|i| match &i.kind {
                ItemKind::Message { id, blocks, edited, withdrawn, .. } => {
                    let tags = match (edited, withdrawn) {
                        (_, true) => " (withdrawn)",
                        (true, false) => " (edited)",
                        _ => "",
                    };
                    format!("message {id}: {}{tags}", feed::markdown_of(blocks))
                }
                ItemKind::Reply { text, client } => format!("reply {text} [{client}]"),
                ItemKind::Submission { message, form, answers, client } => {
                    format!("submission {message}/{form}: {} answers [{client}]", answers.len())
                }
                ItemKind::Action { action, label, client } => format!("action {action} {label} [{client}]"),
                ItemKind::Marker(Marker::Done { client }) => format!("done [{client}]"),
                ItemKind::Marker(Marker::Reopen { client }) => format!("reopen [{client}]"),
                ItemKind::Marker(Marker::State { from, to }) => format!("state {} -> {}", from.as_str(), to.as_str()),
                ItemKind::Marker(Marker::Status { from, to }) => {
                    format!("status {} -> {}", from.as_deref().unwrap_or("-"), to.as_deref().unwrap_or("-"))
                }
            })
            .collect()
    }

    fn find<'a>(inbox: &'a Inbox, key: &str) -> Option<&'a Thread> {
        inbox.threads.iter().find(|t| t.key.as_deref() == Some(key))
    }

    fn owned(inbox: &Inbox, owner: &str) -> usize {
        inbox.threads.iter().filter(|t| t.owner == owner).count()
    }

    #[test]
    fn put_creates_a_thread_with_an_empty_feed() {
        let mut inbox = Inbox::default();
        let out = inbox.put("web-id", "web", 100, put_body("pr-1"));
        assert_eq!(out, PutOutcome::Applied { created: true, entered_needs_you: false });
        let t = &inbox.threads[0];
        assert_eq!((t.kind, t.unread, t.archived), (Kind::Thread, true, false));
        assert!(t.notes.is_empty(), "an owner thread has no records");
        assert_eq!(t.changed_at(), 100);
        assert_eq!(t.state, Some(State::Active));
        assert_eq!(t.status.as_deref(), Some("running ci"));
        assert!(t.feed.is_empty(), "the header says it all: no markers on creation");
    }

    /// The main clutter fix: a dispatcher re-asserting a thread every pass
    /// costs nothing, down to not touching a single field (so the store skips
    /// the write and no dashboard sees a change).
    #[test]
    fn an_identical_put_changes_nothing() {
        let mut inbox = Inbox::default();
        let full = ThreadPut { message: Some("ci started".into()), compose: Some(Compose::default()), ..put_body("pr-1") };
        inbox.put("web-id", "web", 100, full.clone());
        let before = inbox.clone();
        assert_eq!(inbox.put("web-id", "web", 200, full), PutOutcome::Unchanged);
        assert_eq!(inbox, before);
    }

    #[test]
    fn a_put_leaves_markers_for_state_and_status_only() {
        let mut inbox = Inbox::default();
        inbox.put("web-id", "web", 100, put_body("pr-1"));
        inbox.put("web-id", "web", 100, put_body("other"));
        inbox.threads.iter_mut().for_each(|t| t.unread = false);

        // State and status change; title/link/actions/compose don't.
        let changed = ThreadPut { state: State::NeedsYou, status: Some("review".into()), ..put_body("pr-1") };
        let out = inbox.put("web-id", "web", 300, changed.clone());
        assert_eq!(out, PutOutcome::Applied { created: false, entered_needs_you: true });
        let t = &inbox.threads[0];
        assert_eq!(t.key.as_deref(), Some("pr-1"), "a changed thread moves to the top");
        assert!(t.unread);
        assert_eq!(t.updated_at, 300);
        assert_eq!(feed(t), ["state active -> needs-you", "status running ci -> review"]);

        // Header-only changes are applied but leave nothing and aren't news.
        inbox.threads[0].unread = false;
        let cosmetic = ThreadPut {
            title: "PR 1 (renamed)".into(),
            link: Some("https://x/1".into()),
            actions: vec![Action { id: "go".into(), label: "Go".into(), ..Action::default() }],
            compose: Some(Compose { placeholder: Some("p".into()), hint: Some("h".into()) }),
            ..changed.clone()
        };
        assert!(matches!(inbox.put("web-id", "web", 400, cosmetic), PutOutcome::Applied { .. }));
        let t = &inbox.threads[0];
        assert_eq!((t.title.as_str(), t.compose.as_ref().unwrap().hint.as_deref()), ("PR 1 (renamed)", Some("h")));
        assert!(!t.unread, "a new title is not news");
        assert_eq!(t.feed.len(), 2, "and leaves no marker");

        // Status-only changes in a row collapse into the last status marker.
        for status in ["posting", "posted"] {
            inbox.put("web-id", "web", 500, ThreadPut { status: Some(status.into()), ..changed.clone() });
        }
        let t = find(&inbox, "pr-1").unwrap();
        assert_eq!(feed(t), ["state active -> needs-you", "status running ci -> posted"]);
        assert_eq!(t.feed[1].at, 500);
        // ... and one that lands back where it started leaves none.
        inbox.put("web-id", "web", 600, ThreadPut { status: Some("running ci".into()), ..changed });
        assert_eq!(feed(find(&inbox, "pr-1").unwrap()), ["state active -> needs-you"]);
    }

    /// v2 put compat, removed in step 13b: `message` is the `header-message`
    /// feed item, `reply` is `compose`.
    #[test]
    fn a_v2_message_is_one_feed_item_replaced_in_place() {
        let mut inbox = Inbox::default();
        let v2 = |message: Option<&str>| ThreadPut {
            message: message.map(str::to_string),
            reply: Some(Reply { placeholder: Some("next run".into()) }),
            ..put_body("pr-1")
        };
        inbox.put("web-id", "web", 1, v2(Some("ci started")));
        let t = &inbox.threads[0];
        assert_eq!(feed(t), ["message header-message: ci started"]);
        assert_eq!(t.compose, Some(Compose { placeholder: Some("next run".into()), hint: None }), "reply is compose");
        assert_eq!(inbox.put("web-id", "web", 2, v2(Some("ci started"))), PutOutcome::Unchanged);

        // Changed: replaced in place, `edited`, not news.
        inbox.threads[0].unread = false;
        assert!(matches!(inbox.put("web-id", "web", 3, v2(Some("ci green"))), PutOutcome::Applied { .. }));
        let t = &inbox.threads[0];
        assert_eq!(feed(t), ["message header-message: ci green (edited)"]);
        assert_eq!(t.feed[0].at, 1, "first-insert time");
        assert!(!t.unread, "an edit is not news");

        // A status change in between: the message stays where it was.
        inbox.put("web-id", "web", 4, ThreadPut { status: Some("merged".into()), ..v2(Some("ci green")) });
        // Omitted: withdrawn, kept; an empty one counts as omitted.
        inbox.put("web-id", "web", 5, ThreadPut { status: Some("merged".into()), ..v2(None) });
        let t = &inbox.threads[0];
        assert_eq!(feed(t), ["message header-message: ci green (withdrawn)", "status running ci -> merged"]);
        assert_eq!(t.to_put().message, None);
        assert_eq!(inbox.put("web-id", "web", 6, ThreadPut { status: Some("merged".into()), ..v2(Some(" ")) }), PutOutcome::Unchanged);
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
        assert_eq!(inbox.threads.len(), 3, "a notify record never joins an owner thread");
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

        // A done thread whose owner hasn't acked its events yet is kept.
        inbox.put("web-id", "web", day, ThreadPut { state: State::Done, ..put_body("unacked") });
        let event = Event { id: "e-0000086400-abcd".into(), seq: 0, kind: EventKind::Done, action: None, text: None, message: None, form: None, answers: None, at: day };
        inbox.threads.iter_mut().find(|t| t.key.as_deref() == Some("unacked")).unwrap().events.push(event);
        assert_eq!(inbox.prune(day + RETENTION + 1), 0);

        // A put from a live container un-archives (ids are never reused, so
        // this only happens to a thread archived in error).
        inbox.put("web-id", "web", day, put_body("revived"));
        inbox.archive_owner("web-id");
        inbox.put("web-id", "web", day, put_body("revived"));
        assert!(!find(&inbox, "revived").unwrap().archived);
    }

    /// The per-owner cap counts threads: archived ones go first, then done
    /// ones, then the least recently changed without pending events, and the
    /// thread just written never.
    #[test]
    fn the_thread_cap_evicts_archived_then_done_then_the_oldest() {
        let mut inbox = Inbox::default();
        let bare = |key: &str, state: State| ThreadPut { key: key.into(), title: "t".into(), state, ..ThreadPut::default() };
        inbox.put("w-id", "w", 1, bare("archived", State::Active));
        inbox.archive_owner("w-id");
        inbox.put("w-id", "w", 2, bare("done", State::Done));
        inbox.put("w-id", "w", 3, ThreadPut { compose: Some(Compose::default()), ..bare("pending", State::Active) });
        let pending = find(&inbox, "pending").unwrap().id;
        inbox.apply(&Op::Reply { thread: pending, text: "hi".into() }, 3, "tui");
        inbox.put("w-id", "w", 4, bare("live", State::Active));
        // Another owner's threads are never the price.
        inbox.put("x-id", "x", 0, bare("theirs", State::Done));

        let mut next = 0;
        let mut fill = |inbox: &mut Inbox, n: usize| {
            for _ in 0..n {
                push(inbox, "w", Record { at: 10 + next, ..rec(&format!("n{next}"), None, None) }, true);
                next += 1;
            }
        };
        fill(&mut inbox, THREADS_PER_OWNER - 4);
        assert_eq!(owned(&inbox, "w-id"), THREADS_PER_OWNER);
        assert!(find(&inbox, "archived").is_some(), "within the cap: nothing goes");
        fill(&mut inbox, 1);
        assert!(find(&inbox, "archived").is_none(), "archived goes first");
        fill(&mut inbox, 1);
        assert!(find(&inbox, "done").is_none(), "then done");
        fill(&mut inbox, 1);
        assert!(find(&inbox, "live").is_none(), "then the oldest without events");
        assert!(find(&inbox, "pending").is_some(), "the one holding an event outlasts it");
        fill(&mut inbox, 1);
        assert!(!inbox.threads.iter().filter_map(|t| t.head()).any(|r| r.msg == "n0"), "then the oldest record");
        assert_eq!(owned(&inbox, "w-id"), THREADS_PER_OWNER);
        assert!(find(&inbox, "theirs").is_some());
        // Many more: `pending` still holds its unpulled event, so it stays.
        fill(&mut inbox, THREADS_PER_OWNER);
        assert!(find(&inbox, "pending").is_some(), "a thread holding events is never evicted");
    }

    #[test]
    fn a_feed_keeps_its_newest_300_and_a_notify_thread_its_newest_records() {
        let mut inbox = Inbox::default();
        let put = ThreadPut { compose: Some(Compose::default()), ..put_body("pr-1") };
        inbox.put("web-id", "web", 1, put.clone());
        inbox.put("web-id", "web", 2, ThreadPut { state: State::NeedsYou, ..put });
        let id = find(&inbox, "pr-1").unwrap().id;
        for i in 0..MAX_FEED {
            inbox.apply(&Op::Reply { thread: id, text: format!("r{i}") }, 10, "tui");
        }
        let t = find(&inbox, "pr-1").unwrap();
        assert_eq!(t.feed.len(), MAX_FEED);
        assert_eq!(feed(t)[0], "reply r0 [tui]", "the state marker went first");
        inbox.apply(&Op::Reply { thread: id, text: "last".into() }, 10, "tui");
        let t = find(&inbox, "pr-1").unwrap();
        assert_eq!((t.feed.len(), feed(t)[0].as_str()), (MAX_FEED, "reply r1 [tui]"));

        for i in 0..MAX_NOTES + 2 {
            push(&mut inbox, "web", rec(&format!("n{i}"), Some("k"), None), true);
        }
        let n = find(&inbox, "k").unwrap();
        assert_eq!(n.notes.len(), MAX_NOTES);
        assert_eq!(n.notes.last().unwrap().record.msg, "n2", "the oldest two went");
    }

    #[test]
    fn the_sink_denies_rejects_and_applies() {
        let body = r#"{"key":"pr-1","title":"t","state":"active"}"#;
        let put = Message::ThreadPut { at: 7, key: "pr-1".into(), body: body.into() };
        // No `inbox = true`: refused, with the reason delivered as an error
        // record from the same instance, keyed by the thread.
        let SinkAction::Push(r) = decide("web", false, put.clone()) else { panic!("not refused") };
        assert_eq!((r.level, r.key.as_deref(), r.at), (Level::Error, Some("thread:pr-1"), 7));
        assert_eq!(
            r.msg,
            "thread put denied: `web` doesn't declare inbox = true (add it to the sandbox in devsandboxes.toml)"
        );
        // An owner's valid put is applied.
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
        // A plain notify never needs `inbox = true`.
        let note = rec("hi", None, None);
        assert_eq!(decide("web", false, Message::Notify(note.clone())), SinkAction::Push(note));
    }

    #[test]
    fn the_sink_surfaces_only_what_is_new() {
        let mut inbox = Inbox::default();
        let action = |state: &str, status: &str, at: u64| {
            let body = format!(r#"{{"key":"pr-1","title":"PR 1","state":"{state}","status":"{status}","message":"look"}}"#);
            decide("web", true, Message::ThreadPut { at, key: "pr-1".into(), body })
        };
        // Created active: a status line, no popup.
        let shown = inbox.apply_sink("web-id", "web", 1000, action("active", "ci", 10)).unwrap();
        assert_eq!(shown.line, "web: PR 1 — ci");
        assert!(shown.popup.is_none());
        // Re-asserted: nothing at all, not even a status line.
        assert!(inbox.apply_sink("web-id", "web", 1000, action("active", "ci", 11)).is_none());
        // Into needs-you: a popup, coalescable per thread, quoting the v2
        // message.
        let shown = inbox.apply_sink("web-id", "web", 1000, action("needs-you", "ci", 12)).unwrap();
        let popup = shown.popup.unwrap();
        assert_eq!((popup.key.as_deref(), popup.level), (Some("thread:pr-1"), Level::Warn));
        assert_eq!(popup.body, "PR 1\nlook");
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

    /// Control characters never reach the store, the status line or a popup:
    /// a notify record, and a put built directly (past `thread::parse`, which
    /// rejects them, so the sink's own pass is what's tested).
    #[test]
    fn the_sink_strips_control_characters() {
        let mut inbox = Inbox::default();
        let note = rec("\x1b]0;pwned\x07hi\u{9b}2J\u{202E}", Some("k\x1b"), Some("https://x\x1b[0m"));
        let shown = inbox.apply_sink("web-id", "web", 1000, SinkAction::Push(note)).unwrap();
        assert_eq!(shown.line, "web: ]0;pwnedhi2J");
        let popup = shown.popup.unwrap();
        assert_eq!((popup.key.as_deref(), popup.body.as_str()), (Some("k"), "]0;pwnedhi2J\nhttps://x[0m"));
        let r = inbox.threads[0].head().unwrap();
        assert_eq!((r.msg.as_str(), r.link.as_deref()), ("]0;pwnedhi2J", Some("https://x[0m")));

        let put = ThreadPut {
            title: "t\x1b[1m".into(),
            status: Some("s\x07".into()),
            message: Some("a\tb\r\nc\u{85}".into()),
            actions: vec![Action { id: "go".into(), label: "Go\x1b".into(), ..Action::default() }],
            compose: Some(Compose { placeholder: Some("p\x1b".into()), hint: Some("h\x07".into()) }),
            ..put_body("pr-1")
        };
        inbox.apply_sink("web-id", "web", 1000, SinkAction::Put { at: 5, put });
        let t = find(&inbox, "pr-1").unwrap();
        assert_eq!(t.title, "t[1m");
        assert_eq!(t.status.as_deref(), Some("s"));
        assert_eq!(feed::header_message(&t.feed).as_deref(), Some("a   b\nc"));
        assert_eq!(t.actions[0].label, "Go");
        let compose = t.compose.as_ref().unwrap();
        assert_eq!((compose.placeholder.as_deref(), compose.hint.as_deref()), (Some("p"), Some("h")));
        // The v2 `reply` goes through too.
        let put = ThreadPut { reply: Some(Reply { placeholder: Some("r\x1b".into()) }), ..put_body("pr-2") };
        inbox.apply_sink("web-id", "web", 1000, SinkAction::Put { at: 5, put });
        assert_eq!(find(&inbox, "pr-2").unwrap().compose.as_ref().unwrap().placeholder.as_deref(), Some("r"));
    }

    fn send_body(thread: &str, id: &str, text: &str) -> String {
        format!(r#"{{"thread":"{thread}","id":"{id}","blocks":[{{"type":"markdown","text":"{text}"}}]}}"#)
    }

    fn send_msg(at: u64, thread: &str, id: &str, text: &str) -> Message {
        Message::ThreadSend { at, key: thread.into(), id: id.into(), body: send_body(thread, id, text) }
    }

    #[test]
    fn the_sink_authorizes_and_checks_sends_and_withdraws() {
        let send = send_msg(7, "pr-1", "run-1", "hi");
        let SinkAction::Push(r) = decide("web", false, send.clone()) else { panic!("not refused") };
        assert_eq!((r.level, r.key.as_deref(), r.at), (Level::Error, Some("thread:pr-1"), 7));
        assert!(r.msg.starts_with("thread send denied: `web` doesn't declare inbox = true"), "{}", r.msg);
        let SinkAction::Send { at: 7, send: s } = decide("web", true, send) else { panic!("not applied") };
        assert_eq!((s.thread.as_str(), s.id.as_str()), ("pr-1", "run-1"));
        assert_eq!(s.blocks, [Block::Markdown { text: "hi".into() }]);
        // Schema rejects and routing mismatches come back as error records.
        let reject = |m: Message| match decide("web", true, m) {
            SinkAction::Push(r) => r.msg,
            other => panic!("not rejected: {other:?}"),
        };
        let unknown = r#"{"thread":"pr-1","id":"a","blocks":[{"type":"poll","id":"f"}]}"#;
        let msg = reject(Message::ThreadSend { at: 8, key: "pr-1".into(), id: "a".into(), body: unknown.into() });
        assert!(msg.starts_with("thread send rejected: unknown variant `poll`"), "{msg}");
        let msg = reject(Message::ThreadSend { at: 8, key: "pr-2".into(), id: "a".into(), body: send_body("pr-1", "a", "x") });
        assert!(msg.contains("body thread `pr-1` is not the queued thread"), "{msg}");
        let msg = reject(Message::ThreadSend { at: 8, key: "pr-1".into(), id: "b".into(), body: send_body("pr-1", "a", "x") });
        assert!(msg.contains("body id `a` is not the queued id"), "{msg}");
        // Withdraw: authorized like the rest, then passed through.
        let w = Message::ThreadWithdraw { at: 9, key: "pr-1".into(), id: "a".into() };
        assert_eq!(decide("web", true, w.clone()), SinkAction::Withdraw { thread: "pr-1".into(), id: "a".into() });
        let SinkAction::Push(r) = decide("web", false, w) else { panic!("not refused") };
        assert!(r.msg.starts_with("thread withdraw denied:"), "{}", r.msg);
    }

    /// `thread send` / `withdraw` end to end through `decide` and the sink:
    /// a send needs its thread; a new id is news, an edit and a repeat
    /// aren't; a withdrawal keeps the message; order is first-insert order.
    #[test]
    fn sends_insert_edit_and_withdraw_through_the_sink() {
        let mut inbox = Inbox::default();
        let sink = |inbox: &mut Inbox, m: Message| inbox.apply_sink("web-id", "web", 1000, decide("web", true, m));

        // No thread yet: an error record naming the fix, nothing else.
        let shown = sink(&mut inbox, send_msg(5, "pr-1", "a", "early")).unwrap();
        assert_eq!(shown.line, "web: thread send rejected: no thread `pr-1`; put it first");
        assert_eq!(shown.popup.unwrap().level, Level::Error);
        assert_eq!(inbox.threads.len(), 1);
        assert_eq!(inbox.threads[0].kind, Kind::Notify);

        let put = r#"{"key":"pr-1","title":"PR 1","state":"active"}"#;
        sink(&mut inbox, Message::ThreadPut { at: 6, key: "pr-1".into(), body: put.into() });
        let id = find(&inbox, "pr-1").unwrap().id;
        inbox.apply(&Op::MarkRead(id), 1000, "tui");

        // A new message: unread, on top, a status line; no popup while active.
        let shown = sink(&mut inbox, send_msg(10, "pr-1", "a", "one")).unwrap();
        assert_eq!(shown.line, "web: PR 1 — one");
        assert!(shown.popup.is_none());
        let t = find(&inbox, "pr-1").unwrap();
        assert!(t.unread);
        assert_eq!(inbox.threads[0].id, id, "moved to the top");
        assert_eq!(feed(t), ["message a: one"]);
        sink(&mut inbox, send_msg(11, "pr-1", "b", "two"));
        inbox.apply(&Op::MarkRead(id), 1000, "tui");

        // The same again: invisible. New content: edited in place, quiet.
        assert!(sink(&mut inbox, send_msg(12, "pr-1", "a", "one")).is_none());
        assert!(sink(&mut inbox, send_msg(13, "pr-1", "a", "uno")).is_none());
        let t = find(&inbox, "pr-1").unwrap();
        assert!(!t.unread, "an edit isn't news");
        assert_eq!(feed(t), ["message a: uno (edited)", "message b: two"]);
        assert_eq!(t.feed[0].at, 10, "first-insert time kept");

        // Withdrawn: kept, tagged; unknown ids and threads are no-ops.
        assert!(sink(&mut inbox, Message::ThreadWithdraw { at: 14, key: "pr-1".into(), id: "b".into() }).is_none());
        assert!(sink(&mut inbox, Message::ThreadWithdraw { at: 14, key: "pr-1".into(), id: "zz".into() }).is_none());
        assert!(sink(&mut inbox, Message::ThreadWithdraw { at: 14, key: "nope".into(), id: "b".into() }).is_none());
        let t = find(&inbox, "pr-1").unwrap();
        assert_eq!(feed(t), ["message a: uno (edited)", "message b: two (withdrawn)"]);
        assert_eq!(inbox.threads.len(), 2, "no error record for an unknown withdraw");
        // Sent again after a withdrawal: back in place, edited, still quiet.
        assert!(sink(&mut inbox, send_msg(15, "pr-1", "b", "two")).is_none());
        assert_eq!(feed(find(&inbox, "pr-1").unwrap()), ["message a: uno (edited)", "message b: two (edited)"]);

        // On a needs-you thread a new message pops up (quoting it); an edit
        // still doesn't.
        let put = r#"{"key":"pr-1","title":"PR 1","state":"needs-you"}"#;
        sink(&mut inbox, Message::ThreadPut { at: 16, key: "pr-1".into(), body: put.into() });
        let popup = sink(&mut inbox, send_msg(17, "pr-1", "c", "your call")).unwrap().popup.unwrap();
        assert_eq!((popup.key.as_deref(), popup.level, popup.body.as_str()), (Some("thread:pr-1"), Level::Warn, "PR 1\nyour call"));
        assert!(sink(&mut inbox, send_msg(18, "pr-1", "c", "your call, edited")).is_none());
        // Another owner can't send into this thread: it isn't theirs.
        let shown = inbox.apply_sink("other-id", "other", 1000, decide("other", true, send_msg(19, "pr-1", "x", "hi"))).unwrap();
        assert!(shown.line.contains("no thread `pr-1`"), "{}", shown.line);
    }

    #[test]
    fn the_sink_strips_control_characters_from_messages() {
        let mut inbox = Inbox::default();
        inbox.apply_sink("web-id", "web", 1000, SinkAction::Put { at: 5, put: put_body("pr-1") });
        let send = MessageSend {
            thread: "pr-1".into(),
            id: "a".into(),
            blocks: vec![
                Block::Markdown { text: "a\x1b[1mb".into() },
                Block::Fields { items: vec![Field { label: "L\x07".into(), value: "v\u{9b}2J".into() }] },
            ],
        };
        inbox.apply_sink("web-id", "web", 1000, SinkAction::Send { at: 6, send });
        let ItemKind::Message { blocks, .. } = &find(&inbox, "pr-1").unwrap().feed.last().unwrap().kind else { panic!() };
        assert_eq!(
            blocks,
            &[
                Block::Markdown { text: "a[1mb".into() },
                Block::Fields { items: vec![Field { label: "L".into(), value: "v2J".into() }] },
            ]
        );
    }

    /// A send of message `id` on `thread`: a markdown block, then `form`.
    fn send_form(at: u64, thread: &str, id: &str, text: &str, form: &str) -> Message {
        let body = format!(r#"{{"thread":"{thread}","id":"{id}","blocks":[{{"type":"markdown","text":"{text}"}},{form}]}}"#);
        Message::ThreadSend { at, key: thread.into(), id: id.into(), body }
    }

    fn form_state(inbox: &Inbox, key: &str, message: &str) -> Option<FormRecord> {
        let t = find(inbox, key).unwrap();
        feed::form_of(&t.feed, message).map(|(_, r)| r.clone()).or_else(|| {
            t.feed.iter().find_map(|i| match &i.kind {
                ItemKind::Message { id, form, .. } if id == message => form.clone(),
                _ => None,
            })
        })
    }

    /// Forms through the sink: a form entering the feed is news (unread, and
    /// a popup on a needs-you thread) even as an edit; a re-send keeps the
    /// draft answers that still fit; withdrawing withdraws the form.
    #[test]
    fn forms_through_the_sink() {
        let mut inbox = Inbox::default();
        let sink = |inbox: &mut Inbox, m: Message| inbox.apply_sink("web-id", "web", 1000, decide("web", true, m));
        let put = r#"{"key":"pr-1","title":"PR 1","state":"needs-you"}"#;
        sink(&mut inbox, Message::ThreadPut { at: 1, key: "pr-1".into(), body: put.into() });
        let id = find(&inbox, "pr-1").unwrap().id;
        sink(&mut inbox, send_msg(2, "pr-1", "run-1", "no form yet"));
        inbox.apply(&Op::MarkRead(id), 1000, "tui");

        // The same message, now with a form: unread, a status line, a popup.
        let confirm = r#"{"type":"form","id":"f","questions":[{"id":"ok","label":"Merge?","type":"confirm"},{"id":"why","label":"Why","type":"text"}]}"#;
        let shown = sink(&mut inbox, send_form(3, "pr-1", "run-1", "no form yet", confirm)).unwrap();
        assert_eq!(shown.popup.unwrap().body, "PR 1\nno form yet");
        assert!(find(&inbox, "pr-1").unwrap().unread);
        assert_eq!(form_state(&inbox, "pr-1", "run-1"), Some(FormRecord::open("f")));
        inbox.apply(&Op::MarkRead(id), 1000, "tui");
        // Re-asserted: invisible.
        assert!(sink(&mut inbox, send_form(4, "pr-1", "run-1", "no form yet", confirm)).is_none());

        // A draft, then a changed form: the answer to the gone question goes.
        let draft = [("ok".to_string(), Answer::Confirm(true)), ("why".to_string(), Answer::Text("because".into()))];
        inbox.apply(&Op::SaveDraft { thread: id, message: "run-1".into(), answers: draft.into() }, 1000, "tui");
        let changed = r#"{"type":"form","id":"f","questions":[{"id":"ok","label":"Merge now?","type":"confirm"}]}"#;
        assert!(sink(&mut inbox, send_form(5, "pr-1", "run-1", "no form yet", changed)).is_none(), "a quiet edit");
        let r = form_state(&inbox, "pr-1", "run-1").unwrap();
        assert_eq!(r.draft, [("ok".to_string(), Answer::Confirm(true))].into());
        assert!(!find(&inbox, "pr-1").unwrap().unread);

        // Withdrawn with the message.
        sink(&mut inbox, Message::ThreadWithdraw { at: 6, key: "pr-1".into(), id: "run-1".into() });
        assert_eq!(form_state(&inbox, "pr-1", "run-1").unwrap().state, FormState::Withdrawn);
        // On an active thread a new form is news but no popup.
        let put = r#"{"key":"pr-1","title":"PR 1","state":"active"}"#;
        sink(&mut inbox, Message::ThreadPut { at: 7, key: "pr-1".into(), body: put.into() });
        let shown = sink(&mut inbox, send_form(8, "pr-1", "run-2", "again", confirm)).unwrap();
        assert!(shown.popup.is_none());
        // Form text is sanitized on the way in (escapes are a schema reject;
        // tabs and CRs pass and are normalized).
        let dirty = r#"{"type":"form","id":"g","title":"T\tx","questions":[{"id":"q","label":"L\tx","context":"a\r\nb","type":"confirm","yes":"Y\tx"}]}"#;
        sink(&mut inbox, send_form(9, "pr-1", "run-3", "x", dirty));
        let t = find(&inbox, "pr-1").unwrap();
        let (f, _) = feed::form_of(&t.feed, "run-3").unwrap();
        assert_eq!(f.title.as_deref(), Some("T   x"));
        assert_eq!((f.questions[0].label.as_str(), f.questions[0].context.as_deref()), ("L   x", Some("a\nb")));
        assert!(matches!(&f.questions[0].kind, form::QuestionKind::Confirm { yes: Some(y), .. } if y == "Y   x"));
        let escape = r#"{"type":"form","id":"h","title":"T\u001b[1m","questions":[{"id":"q","label":"L","type":"confirm"}]}"#;
        assert!(sink(&mut inbox, send_form(10, "pr-1", "run-4", "x", escape)).unwrap().line.contains("control character"));
    }

    /// The thread `pr-1` of `web-id`, with message `run-1` carrying the
    /// plan's example form; its store id.
    fn with_example_form(inbox: &mut Inbox) -> u64 {
        inbox.apply_sink("web-id", "web", 1000, SinkAction::Put { at: 1, put: put_body("pr-1") });
        let body = format!(r#"{{"thread":"pr-1","id":"run-1","blocks":[{}]}}"#, form::tests::EXAMPLE);
        let SinkAction::Send { at, send } = decide("web", true, Message::ThreadSend { at: 2, key: "pr-1".into(), id: "run-1".into(), body }) else {
            panic!("refused")
        };
        inbox.apply_sink("web-id", "web", 1000, SinkAction::Send { at, send });
        find(inbox, "pr-1").unwrap().id
    }

    #[test]
    fn drafts_merge_quietly_and_bad_ones_are_dropped() {
        let mut inbox = Inbox::default();
        let id = with_example_form(&mut inbox);
        let draft = |inbox: &mut Inbox, pairs: &[(&str, Answer)]| {
            let answers = pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
            inbox.apply(&Op::SaveDraft { thread: id, message: "run-1".into(), answers }, 50, "tui");
        };
        draft(&mut inbox, &[("notes", Answer::Text("a".into()))]);
        draft(&mut inbox, &[("c-3726888733", Answer::Choice("skip".into()))]);
        draft(&mut inbox, &[("notes", Answer::Text("b\x1b".into()))]);
        let r = form_state(&inbox, "pr-1", "run-1").unwrap();
        assert_eq!(
            r.draft,
            [("c-3726888733".to_string(), Answer::Choice("skip".into())), ("notes".to_string(), Answer::Text("b".into()))].into()
        );
        // A bad answer refuses the whole op; no event, no feed item ever.
        draft(&mut inbox, &[("notes", Answer::Text("c".into())), ("c-3726888733", Answer::Choice("nope".into()))]);
        draft(&mut inbox, &[("unknown", Answer::Text("x".into()))]);
        assert_eq!(form_state(&inbox, "pr-1", "run-1").unwrap(), r);
        let t = find(&inbox, "pr-1").unwrap();
        assert!(t.events.is_empty());
        assert_eq!(t.feed.len(), 1);
    }

    #[test]
    fn a_submission_freezes_the_form_and_enqueues_one_event() {
        let mut inbox = Inbox::default();
        let id = with_example_form(&mut inbox);
        let draft = [("notes".to_string(), Answer::Text("from the draft".into()))].into();
        inbox.apply(&Op::SaveDraft { thread: id, message: "run-1".into(), answers: draft }, 50, "tui");
        // Missing required answers or a bad value: nothing happens.
        let given = [("c-3726888733".to_string(), Answer::Choice("nope".into()))].into();
        inbox.apply(&Op::Submit { thread: id, message: "run-1".into(), answers: given }, 60, "tui");
        assert!(find(&inbox, "pr-1").unwrap().events.is_empty());

        let given = [("c-3726888733".to_string(), Answer::Choice("skip".into()))].into();
        inbox.apply(&Op::Submit { thread: id, message: "run-1".into(), answers: given }, 70, "api:x");
        let t = find(&inbox, "pr-1").unwrap();
        let reply = "Already batched in `flush()` (src/cost.ts:88), so this would double-buffer.";
        let answers = serde_json::json!({"c-3726888733": "skip", "c-3726888733-text": reply, "notes": "from the draft"});
        let [e] = &t.events[..] else { panic!("{:?}", t.events) };
        assert_eq!(e.kind, EventKind::Submit);
        assert_eq!((e.message.as_deref(), e.form.as_deref(), e.answers.as_ref(), e.at), (Some("run-1"), Some("drafts"), Some(&answers), 70));
        assert_eq!(feed(t).last().unwrap(), "submission run-1/drafts: 3 answers [api:x]");
        let r = form_state(&inbox, "pr-1", "run-1").unwrap();
        let FormState::Submitted { at: 70, client, answers: stored } = &r.state else { panic!("{r:?}") };
        assert_eq!(client, "api:x");
        assert_eq!(form::answers_json(&form::tests::example(), stored), answers);
        assert!(r.draft.is_empty());

        // Closed: no second submission, no draft.
        inbox.apply(&Op::Submit { thread: id, message: "run-1".into(), answers: BTreeMap::new() }, 80, "tui");
        let notes = [("notes".to_string(), Answer::Text("late".into()))].into();
        inbox.apply(&Op::SaveDraft { thread: id, message: "run-1".into(), answers: notes }, 80, "tui");
        let t = find(&inbox, "pr-1").unwrap();
        assert_eq!(t.events.len(), 1);
        assert_eq!(form_state(&inbox, "pr-1", "run-1").unwrap(), r);
        // Unknown message: a no-op.
        inbox.apply(&Op::Submit { thread: id, message: "nope".into(), answers: BTreeMap::new() }, 80, "tui");
        assert_eq!(find(&inbox, "pr-1").unwrap().events.len(), 1);
        // The event never says which client.
        let json = serde_json::to_string(&find(&inbox, "pr-1").unwrap().events).unwrap();
        assert!(!json.contains("api:x"), "{json}");
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
    fn ops_remove_by_identity() {
        let mut inbox = Inbox::default();
        push(&mut inbox, "a", rec("v1", Some("k"), None), true);
        push(&mut inbox, "a", rec("v2", Some("k"), None), true);
        push(&mut inbox, "a", rec("solo", None, None), true);
        push(&mut inbox, "b", rec("b1", None, None), true);

        let thread_id = inbox.threads.iter().find(|t| t.key.is_some()).unwrap().id;
        inbox.apply(&Op::RemoveThread(thread_id), 0, "tui");
        assert_eq!(msgs(&inbox), [("b", "b1"), ("a", "solo")]);
        // Unknown ids are no-ops, not panics.
        inbox.apply(&Op::RemoveThread(thread_id), 0, "tui");
        inbox.apply(&Op::MarkRead(thread_id), 0, "tui");
        assert_eq!(msgs(&inbox), [("b", "b1"), ("a", "solo")]);

        // Leaving the Inbox reads every notify record, and only those.
        inbox.put("a-id", "a", 10, put_body("pr-1"));
        inbox.apply(&Op::MarkNotifyRead, 0, "tui");
        let unread: Vec<Kind> = inbox.threads.iter().filter(|t| t.unread).map(|t| t.kind).collect();
        assert_eq!(unread, [Kind::Thread]);
        inbox.apply(&Op::ClearNotify, 0, "tui");
        assert_eq!(inbox.threads.len(), 1, "the owner thread stays");
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
        inbox.apply(&Op::MarkRead(note), 0, "tui");
        inbox.apply(&Op::MarkRead(u64::MAX), 0, "tui");
        assert_eq!(inbox.needs_you(), 1);
        let asks = find(&inbox, "asks").unwrap().id;
        inbox.apply(&Op::MarkRead(asks), 0, "tui");
        assert_eq!(inbox.needs_you(), 1);

        // An archived thread is history: it never asks for anything.
        inbox.archive_owner("a-id");
        assert_eq!(inbox.needs_you(), 0);

        // `D` clears notify records and leaves the owners' threads.
        inbox.apply(&Op::ClearNotify, 0, "tui");
        assert_eq!(inbox.threads.len(), 3);
        assert!(inbox.threads.iter().all(|t| t.kind == Kind::Thread));
    }

    #[test]
    fn json_roundtrip() {
        let mut inbox = Inbox::default();
        push(&mut inbox, "a", rec("v1", Some("k"), Some("https://x/1")), true);
        push(&mut inbox, "b", rec("multi\nline \"q\"", None, None), true);
        push(&mut inbox, "a", Record { level: Level::Warn, at: 7, ..rec("v2", Some("k"), None) }, true);
        push(&mut inbox, "b", rec("read", None, None), false);

        let text = inbox.to_json().unwrap();
        assert!(text.contains("\"version\": 3"), "{text}");
        let loaded = Inbox::from_json(&text).unwrap();
        // Thread ids survive, so selection and the open pane follow a reload.
        assert_eq!(loaded, inbox);
        assert_eq!(loaded.to_json().unwrap(), text, "byte for byte, so an unchanged store isn't rewritten");

        // Ids keep growing past the saved ones.
        let mut loaded = loaded;
        let max_saved = inbox.threads.iter().flat_map(|t| &t.notes).map(|n| n.id).max().unwrap();
        push(&mut loaded, "a", rec("later", None, None), true);
        assert!(loaded.threads[0].notes[0].id > max_saved);

        // Another version, or none, is not this build's to read.
        let v4 = text.replace("\"version\": 3", "\"version\": 4");
        assert!(Inbox::from_json(&v4).unwrap_err().contains("version 4"));
        assert!(Inbox::from_json("{}").is_err());
        assert!(Inbox::from_json("").is_err());
        let bad_level = r#"{"version":3,"threads":[{"id":1,"owner":"a","owner_name":"a","notes":[{"seq":0,"level":"loud","at":0,"msg":"x"}]}]}"#;
        assert!(Inbox::from_json(bad_level).is_err());
    }

    /// Owner threads survive a save/load with every field: the nested host
    /// verb, compose, every feed item kind with its client, the events.
    #[test]
    fn json_roundtrip_with_threads() {
        let mut inbox = Inbox::default();
        push(&mut inbox, "a", rec("a note", None, None), true);
        let full = ThreadPut {
            key: "pr-1".into(),
            title: "#1 feat/x".into(),
            link: Some("https://x/1".into()),
            state: State::NeedsYou,
            status: Some("review".into()),
            child: Some("pr-1".into()),
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
                Action { id: "fin".into(), label: "Finish".into(), done: true, ..Action::default() },
            ],
            compose: Some(Compose { placeholder: Some("next run".into()), hint: Some("starts a run".into()) }),
            message: Some("multi\nline \"q\"".into()),
            reply: None,
        };
        inbox.put("a-id", "a", 7, full.clone());
        let id = find(&inbox, "pr-1").unwrap().id;
        inbox.apply(&Op::Act { thread: id, action: "post".into() }, 8, "api:x");
        inbox.apply(&Op::Reply { thread: id, text: "multi \"q\" \\ x".into() }, 8, "cli");
        inbox.apply(&Op::Act { thread: id, action: "fin".into() }, 8, "tui");
        inbox.apply(&Op::Reopen(id), 8, "tui");
        inbox.put("a-id", "a", 9, ThreadPut { status: Some("merged".into()), message: None, ..full });
        inbox.archive_owner("a-id");

        let text = inbox.to_json().unwrap();
        assert!(text.contains("\"events\""), "{text}");
        let loaded = Inbox::from_json(&text).unwrap();
        assert_eq!(loaded, inbox, "{text}");
        assert_eq!(
            feed(find(&loaded, "pr-1").unwrap()),
            [
                "message header-message: multi\nline \"q\" (withdrawn)",
                "action post Post [api:x]",
                "reply multi \"q\" \\ x [cli]",
                "action fin Finish [tui]",
                "done [tui]",
                "reopen [tui]",
                "state active -> needs-you",
                "status review -> merged",
            ]
        );
        // Every seq counts toward the id counter, so nothing minted later
        // reuses one.
        let max = inbox.max_id().unwrap();
        assert!(loaded.next_id > max);
    }

    /// A v2 `inbox.toml`: notify threads come over with their notes, unread
    /// and archived flags and ids; owner threads are dropped whole.
    #[test]
    fn imports_v2_notify_threads_and_drops_owner_threads() {
        let v2 = r#"version = 2

[[thread]]
id = 4
owner = "web-id"
owner_name = "web"
key = "pr-1"
unread = true

[[thread.note]]
seq = 9
level = "warn"
at = 7
link = "https://x/1"
msg = "v2"

[[thread.note]]
seq = 3
level = "info"
at = 1
msg = "v1"

[[thread]]
id = 5
owner = "web-id"
owner_name = "web"
key = "pr-1"
kind = "thread"
unread = true
title = "PR 1"
state = "needs-you"
message = "look"
updated_at = 10

[thread.reply]
placeholder = "p"

[[thread.action]]
id = "go"
label = "Go"

[[thread.entry]]
seq = 20
at = 10
kind = "message"
text = "look"

[[thread.event]]
id = "e-0000000010-abcd"
seq = 21
kind = "reply"
text = "hi"
at = 10

[[thread]]
id = 6
owner = "gone-id"
owner_name = "gone"
archived = true
unread = false

[[thread.note]]
seq = 2
level = "error"
at = 0
msg = "old"
"#;
        let mut inbox = Inbox::import_v2(v2, &BTreeMap::new()).unwrap();
        assert_eq!(inbox.threads.len(), 2, "the owner thread is dropped");
        let web = &inbox.threads[0];
        assert_eq!((web.id, web.kind, web.owner.as_str(), web.key.as_deref()), (4, Kind::Notify, "web-id", Some("pr-1")));
        assert_eq!(web.notes.iter().map(|n| n.id).collect::<Vec<_>>(), [9, 3]);
        assert_eq!(web.head().unwrap().key.as_deref(), Some("pr-1"));
        assert_eq!((web.head().unwrap().level, web.head().unwrap().link.as_deref()), (Level::Warn, Some("https://x/1")));
        assert!(web.unread && !web.archived);
        let gone = &inbox.threads[1];
        assert!(gone.archived && !gone.unread);
        assert!(inbox.events_for("web-id").is_empty(), "its events went with it");
        // Ids keep growing past the imported ones; the result saves as v3.
        push(&mut inbox, "web", rec("new", None, None), true);
        assert!(inbox.threads[0].notes[0].id > 9 && inbox.threads[0].id > 6);
        assert_eq!(Inbox::from_json(&inbox.to_json().unwrap()).unwrap(), inbox);
        assert!(Inbox::import_v2("version = 2\n[[thread]]\nowner = \"a\"\n[[thread.note]]\nseq = 0\nlevel = \"loud\"\nat = 0\nmsg = \"x\"\n", &BTreeMap::new()).is_err());
        assert!(Inbox::import_v2("", &BTreeMap::new()).unwrap().threads.is_empty());
    }

    /// The thread pr-1 from `web-id`, with a reply box and three buttons: a
    /// dispatcher action, a host-only one and a `done: true` one.
    fn asking(inbox: &mut Inbox) -> u64 {
        let put = ThreadPut {
            state: State::NeedsYou,
            actions: vec![
                Action { id: "post".into(), label: "Post replies".into(), ..Action::default() },
                Action {
                    id: "look".into(),
                    label: "Look".into(),
                    host: Some(thread::HostVerb::Terminal(thread::NoArgs {})),
                    ..Action::default()
                },
                Action { id: "fin".into(), label: "Finish".into(), done: true, ..Action::default() },
            ],
            compose: Some(Compose::default()),
            ..put_body("pr-1")
        };
        inbox.put("web-id", "web", 100, put);
        find(inbox, "pr-1").unwrap().id
    }

    fn events(inbox: &Inbox) -> Vec<(EventKind, Option<&str>, Option<&str>)> {
        let t = find(inbox, "pr-1").unwrap();
        t.events.iter().map(|e| (e.kind, e.action.as_deref(), e.text.as_deref())).collect()
    }

    #[test]
    fn user_ops_add_feed_items_with_their_client_and_an_event() {
        let mut inbox = Inbox::default();
        let id = asking(&mut inbox);

        inbox.apply(&Op::Act { thread: id, action: "post".into() }, 200, "tui");
        inbox.apply(&Op::Reply { thread: id, text: "  rename it  ".into() }, 201, "api:my-ext");
        let t = find(&inbox, "pr-1").unwrap();
        assert_eq!(t.state, Some(State::NeedsYou), "a plain action leaves the state to the dispatcher");
        assert_eq!(t.updated_at, 201);
        assert_eq!(feed(t), ["action post Post replies [tui]", "reply rename it [api:my-ext]"]);
        assert_eq!(events(&inbox), [
            (EventKind::Action, Some("post"), None),
            (EventKind::Reply, None, Some("rename it")),
        ]);

        // `done: true`: what was pressed, then the done marker; one `done`
        // event names the button.
        inbox.apply(&Op::Act { thread: id, action: "fin".into() }, 202, "cli");
        assert_eq!(find(&inbox, "pr-1").unwrap().state, Some(State::Done));
        assert_eq!(events(&inbox)[2], (EventKind::Done, Some("fin"), None));
        // Already done: `d` is no transition, so nothing at all.
        let snapshot = inbox.clone();
        inbox.apply(&Op::MarkDone(id), 203, "tui");
        assert_eq!(inbox, snapshot);
        // `u` reopens (active), and only a done thread.
        inbox.apply(&Op::Reopen(id), 204, "tui");
        inbox.apply(&Op::Reopen(id), 205, "tui");
        assert_eq!(find(&inbox, "pr-1").unwrap().state, Some(State::Active));
        // `d` on a live thread.
        inbox.apply(&Op::MarkDone(id), 206, "tui");
        let t = find(&inbox, "pr-1").unwrap();
        assert_eq!(t.state, Some(State::Done));
        assert_eq!(feed(t)[2..], ["action fin Finish [cli]", "done [cli]", "reopen [tui]", "done [tui]"]);
        assert_eq!(events(&inbox)[3..], [(EventKind::Reopen, None, None), (EventKind::Done, None, None)]);
        assert_eq!(t.events.iter().map(|e| e.at).collect::<Vec<_>>(), [200, 201, 202, 204, 206]);
    }

    /// The client is the user's audit: no event an owner can read carries it.
    #[test]
    fn the_client_never_reaches_the_owner() {
        let mut inbox = Inbox::default();
        let id = asking(&mut inbox);
        inbox.apply(&Op::Act { thread: id, action: "post".into() }, 1, "api:secret-gui");
        inbox.apply(&Op::Reply { thread: id, text: "hi".into() }, 2, "api:secret-gui");
        inbox.apply(&Op::Act { thread: id, action: "fin".into() }, 3, "api:secret-gui");
        inbox.apply(&Op::Reopen(id), 4, "api:secret-gui");
        let events = inbox.events_for("web-id");
        assert_eq!(events.len(), 4);
        let json = serde_json::to_string(&events).unwrap();
        assert!(!json.contains("client") && !json.contains("secret-gui"), "{json}");
        // The thread ls shape doesn't carry it either.
        let ls = serde_json::to_string(&inbox.threads_for("web-id")[0].to_put()).unwrap();
        assert!(!ls.contains("secret-gui"), "{ls}");
        // It is in the store, for the user.
        assert!(inbox.to_json().unwrap().contains("api:secret-gui"));
    }

    #[test]
    fn user_ops_that_do_nothing() {
        let mut inbox = Inbox::default();
        let id = asking(&mut inbox);
        let snapshot = inbox.clone();
        for op in [
            // A host-only button, an unknown action, an empty reply.
            Op::Act { thread: id, action: "look".into() },
            Op::Act { thread: id, action: "gone".into() },
            Op::Reply { thread: id, text: " \n ".into() },
            Op::Reopen(id),
            Op::MarkDone(u64::MAX),
        ] {
            inbox.apply(&op, 300, "tui");
        }
        assert_eq!(inbox, snapshot);

        // No compose: no reply.
        inbox.put("web-id", "web", 100, put_body("plain"));
        let plain = find(&inbox, "plain").unwrap().id;
        inbox.apply(&Op::Reply { thread: plain, text: "hi".into() }, 300, "tui");
        assert!(find(&inbox, "plain").unwrap().events.is_empty());

        // A notify thread has nobody to answer it.
        push(&mut inbox, "web", rec("note", None, None), true);
        let note = inbox.threads.iter().find(|t| t.kind == Kind::Notify).unwrap().id;
        inbox.apply(&Op::MarkDone(note), 300, "tui");
        assert!(inbox.threads.iter().all(|t| t.events.is_empty() || t.key.as_deref() == Some("pr-1")));

        // Archived: read-only, no new events.
        inbox.archive_owner("web-id");
        let snapshot = inbox.clone();
        inbox.apply(&Op::Act { thread: id, action: "post".into() }, 300, "tui");
        inbox.apply(&Op::MarkDone(id), 300, "tui");
        assert_eq!(inbox, snapshot);
    }

    #[test]
    fn a_long_reply_is_cut() {
        let mut inbox = Inbox::default();
        let id = asking(&mut inbox);
        inbox.apply(&Op::Reply { thread: id, text: "é".repeat(MAX_REPLY + 5) }, 1, "tui");
        assert_eq!(events(&inbox)[0].2.unwrap().chars().count(), MAX_REPLY);
    }

    #[test]
    fn event_ids_have_the_shape_and_are_unique() {
        let mut inbox = Inbox::default();
        let id = asking(&mut inbox);
        // Same second, many events: ids never repeat.
        for _ in 0..MAX_EVENTS {
            inbox.apply(&Op::Act { thread: id, action: "post".into() }, 1_790_900_001, "tui");
        }
        let t = find(&inbox, "pr-1").unwrap();
        let mut ids: Vec<&str> = t.events.iter().map(|e| e.id.as_str()).collect();
        assert!(ids.iter().all(|id| crate::devsbd::control::valid_event_id(id)), "{ids:?}");
        assert!(ids.iter().all(|id| id.starts_with("e-1790900001-")));
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), MAX_EVENTS);
        // Small times are zero-padded to the same shape.
        inbox.apply(&Op::Reply { thread: id, text: "x".into() }, 5, "tui");
        let last = &find(&inbox, "pr-1").unwrap().events.last().unwrap().id;
        assert!(last.starts_with("e-0000000005-"), "{last}");
    }

    #[test]
    fn events_are_bounded_per_thread_oldest_first() {
        let mut inbox = Inbox::default();
        let id = asking(&mut inbox);
        for i in 0..MAX_EVENTS + 3 {
            inbox.apply(&Op::Reply { thread: id, text: format!("r{i}") }, i as u64, "tui");
        }
        let t = find(&inbox, "pr-1").unwrap();
        assert_eq!(t.events.len(), MAX_EVENTS);
        assert_eq!(t.events[0].text.as_deref(), Some("r3"), "the oldest three went");
    }

    #[test]
    fn ack_is_idempotent_and_scoped_to_the_owner() {
        let mut inbox = Inbox::default();
        let id = asking(&mut inbox);
        inbox.put("other-id", "other", 100, ThreadPut { compose: Some(Compose::default()), ..put_body("o") });
        let other = find(&inbox, "o").unwrap().id;
        inbox.apply(&Op::Reply { thread: other, text: "theirs".into() }, 10, "tui");
        inbox.apply(&Op::Reply { thread: id, text: "one".into() }, 11, "tui");
        inbox.apply(&Op::Act { thread: id, action: "post".into() }, 11, "tui");

        let mine = inbox.events_for("web-id");
        assert_eq!(mine.iter().map(|(k, e)| (k.as_str(), e.kind)).collect::<Vec<_>>(), [
            ("pr-1", EventKind::Reply),
            ("pr-1", EventKind::Action),
        ]);
        let theirs = inbox.events_for("other-id");
        assert_eq!(theirs.len(), 1);

        // Another owner's id is not mine to ack.
        assert_eq!(inbox.ack("web-id", &[theirs[0].1.id.clone()]), 0);
        assert_eq!(inbox.events_for("other-id").len(), 1);
        let first = vec![mine[0].1.id.clone()];
        assert_eq!(inbox.ack("web-id", &first), 1);
        assert_eq!(inbox.ack("web-id", &first), 0, "acking again is harmless");
        assert_eq!(inbox.events_for("web-id").len(), 1);
    }

    /// Events are delivery state: a dispatcher's re-put (even one that
    /// changes the state back) leaves them and the feed alone.
    #[test]
    fn events_survive_a_put() {
        let mut inbox = Inbox::default();
        let id = asking(&mut inbox);
        inbox.apply(&Op::Act { thread: id, action: "fin".into() }, 200, "tui");
        let items = find(&inbox, "pr-1").unwrap().feed.len();
        let again = ThreadPut { status: Some("posting".into()), ..find(&inbox, "pr-1").unwrap().to_put() };
        let again = ThreadPut { state: State::NeedsYou, ..again };
        inbox.put("web-id", "web", 300, again);
        let t = find(&inbox, "pr-1").unwrap();
        assert_eq!(t.state, Some(State::NeedsYou), "the dispatcher owns the meaning");
        assert_eq!(t.events.len(), 1);
        assert_eq!(t.feed.len(), items + 2, "state + status markers appended, none lost");
        assert_eq!(feed(t)[items..], ["state done -> needs-you", "status running ci -> posting"]);
    }

    #[test]
    fn thread_ls_shape_round_trips_the_put() {
        let mut inbox = Inbox::default();
        asking(&mut inbox);
        inbox.put("web-id", "web", 100, ThreadPut { message: Some("hello".into()), ..put_body("gone") });
        inbox.put("other-id", "other", 100, put_body("theirs"));
        push(&mut inbox, "web", rec("note", Some("pr-1"), None), true);
        let mine: Vec<ThreadPut> = inbox.threads_for("web-id").iter().map(|t| t.to_put()).collect();
        assert_eq!(mine.len(), 2, "owner threads only, the owner's only");
        for put in &mine {
            let json = serde_json::to_string(put).unwrap();
            assert_eq!(thread::parse(&json).as_ref(), Ok(put), "{json}");
            assert!(!json.contains("null"), "absent fields are left out: {json}");
        }
        // v2 put compat: the header message and `reply` come back too.
        let json = serde_json::to_string(&mine[0]).unwrap();
        assert!(json.contains(r#""message":"hello""#), "{json}");
        let json = serde_json::to_string(&mine[1]).unwrap();
        assert!(json.contains(r#""compose":{}"#) && json.contains(r#""reply":{}"#), "{json}");
        // Re-putting what `ls` returned changes nothing.
        for put in mine {
            assert_eq!(inbox.put("web-id", "web", 999, put), PutOutcome::Unchanged);
        }
        inbox.archive_owner("web-id");
        assert!(inbox.threads_for("web-id").is_empty());
    }

    #[test]
    fn rfc3339_dates() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_709_208_000), "2024-02-29T12:00:00Z");
        assert_eq!(rfc3339(1_790_900_001), "2026-10-02T00:13:21Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_735_689_599), "2024-12-31T23:59:59Z");
    }

    /// A v1 file (no `version`, threads keyed by instance name, the key on the
    /// notes) imports with owners resolved through the state's names.
    #[test]
    fn imports_v1() {
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
        let mut inbox = Inbox::import_v2(v1, &names(&[("web", "web-abc1")])).unwrap();
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
        // What v1 became is a plain v3 store.
        assert_eq!(Inbox::from_json(&inbox.to_json().unwrap()).unwrap(), inbox);
    }
}
