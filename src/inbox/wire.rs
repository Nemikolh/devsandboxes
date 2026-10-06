//! The API's wire views and user-op decisions, platform-neutral so the CLI
//! (`commands::inbox`) shares them on every OS while the daemon that serves
//! them (`serve::api`, which re-exports all of this) is unix-only.
//!
//! Views ([`ThreadSummary`], [`ThreadDetail`]) are the explicit wire shape,
//! never the store model, whose JSON is internal (and carries what the wire
//! must not, e.g. the client behind each user item). The `decide_*` fns are
//! the user ops' checks: run under the store lock by `ops::apply_if`, they
//! return the `Op`s to apply or the API error, so the daemon and the CLI's
//! local fallback give identical events and errors.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::form::{self, ChoiceDefault, FormError, QuestionKind};
use super::thread::HostVerb;
use super::{Block, FeedItem, FormRecord, Inbox, ItemKind, Kind, Marker, Op, Thread, feed, ops};

/// An error answer: a stable machine `code` (docs/api.md, *Errors*) and a
/// human message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub code: &'static str,
    pub message: String,
}

impl ApiError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new("not-found", message)
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new("invalid", message)
    }

    pub fn denied(message: impl Into<String>) -> Self {
        Self::new("denied", message)
    }

    /// A form that was already submitted or withdrawn.
    pub fn closed_form(message: impl Into<String>) -> Self {
        Self::new("closed-form", message)
    }

    /// A forward's host port couldn't be bound (in use, privileged, an
    /// address not on this host).
    #[cfg_attr(not(unix), allow(dead_code))] // only the daemon (unix) uses it
    pub fn bind_failed(message: impl Into<String>) -> Self {
        Self::new("bind-failed", message)
    }

    #[cfg_attr(not(unix), allow(dead_code))] // only the daemon (unix) uses it
    pub fn internal(e: anyhow::Error) -> Self {
        Self::new("internal", format!("{e:#}"))
    }

    #[cfg_attr(not(unix), allow(dead_code))] // only the daemon (unix) uses it
    pub fn unknown_method(method: &str) -> Self {
        Self::new("unknown-method", format!("unknown method `{method}`"))
    }
}

// ---- wire views -----------------------------------------------------------

/// One row of `inbox.threads.list`, and the head of `inbox.thread.get`.
/// Carries both addresses: `id`, and `owner` (instance id) / `owner_name` +
/// `key`.
#[derive(Debug, Serialize)]
pub struct ThreadSummary {
    /// Store id: stable for the thread's life, what `{"thread": id}` takes.
    pub id: u64,
    /// The owner's `instance_id`.
    pub owner: String,
    /// The owner's instance name when it last wrote (display; kept after
    /// `devsandbox rm`).
    pub owner_name: String,
    pub key: Option<String>,
    /// `notify` (`devsbd notify` records) or `thread` (`devsbd thread put`).
    pub kind: &'static str,
    /// `needs-you` | `active` | `done`; `null` on a notify thread.
    pub state: Option<&'static str>,
    pub status: Option<String>,
    /// A dispatcher thread's title, or a notify thread's newest message,
    /// first line only (what the dashboard's list shows).
    pub title: String,
    /// A notify thread's newest record level (`info` | `warn` | `error`).
    pub level: Option<&'static str>,
    pub unread: bool,
    pub archived: bool,
    /// Whether it's in the `needs-you` view (what the badges count).
    pub needs_you: bool,
    /// Unix seconds of the last change.
    pub changed_at: u64,
}

/// `inbox.thread.get`: the summary plus everything the pane shows.
#[derive(Debug, Serialize)]
pub struct ThreadDetail {
    #[serde(flatten)]
    pub summary: ThreadSummary,
    pub link: Option<String>,
    /// Key of the dispatcher child the thread is about.
    pub child: Option<String>,
    /// Present when the thread takes free-text replies.
    pub compose: Option<ComposeView>,
    pub actions: Vec<ActionView>,
    /// An owner thread's feed, oldest first (first-insert order).
    pub feed: Vec<FeedItemView>,
    /// A notify thread's records, newest first (the first is the head).
    pub notes: Vec<NoteView>,
    /// Events the owner hasn't acked yet.
    pub events_pending: usize,
}

#[derive(Debug, Serialize)]
pub struct ComposeView {
    pub placeholder: Option<String>,
    /// What sending does now, one line under the box.
    pub hint: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ActionView {
    pub id: String,
    pub label: String,
    /// Pressing it sets the thread done.
    pub done: bool,
    /// The host verb it runs (`vscode`, `terminal`, `logs`, `forward`,
    /// `open`, `rm`), or `null` for a dispatcher-only action.
    pub host: Option<&'static str>,
    /// Pressing it sends the owner an event: what `inbox.thread.act` does.
    /// `false` means host-only, which the API refuses.
    pub sends_event: bool,
}

/// One feed item, tagged by `type`. `seq` is the arrival order across the
/// whole Inbox, `at` unix seconds of first insert. The client that made a
/// user item is the store's, not the wire's.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum FeedItemView {
    /// From the owner.
    Message { seq: u64, at: u64, id: String, blocks: Vec<BlockView>, edited: bool, withdrawn: bool },
    Reply { seq: u64, at: u64, text: String },
    Action { seq: u64, at: u64, action: String, label: String },
    /// The user submitted form `form` of message `message`: every question's
    /// answer (`null`: an optional one left unanswered).
    Submission { seq: u64, at: u64, message: String, form: String, answers: Value },
    /// `marker`: `done` | `reopen` (the user's; no `from`/`to`), `state` |
    /// `status` (the owner's change, `from` -> `to`).
    Marker { seq: u64, at: u64, marker: &'static str, from: Option<String>, to: Option<String> },
}

/// One block of a message, tagged by `type`.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum BlockView {
    Markdown { text: String },
    /// A compact key/value list, in order.
    Fields { items: Vec<FieldView> },
    /// Questions for the user. `state` is `open` | `submitted` |
    /// `withdrawn`; `draft` the saved partial answers of an open form;
    /// `answers` every question's answer once submitted, else `null`.
    Form {
        id: String,
        title: Option<String>,
        submit: String,
        questions: Vec<QuestionView>,
        state: &'static str,
        draft: Value,
        answers: Option<Value>,
    },
}

/// One question of a form block, tagged by `type`.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum QuestionView {
    /// `default`: an option id, ids when `multiple`, or `null`.
    Choice {
        id: String,
        label: String,
        context: Option<String>,
        required: bool,
        options: Vec<OptionView>,
        multiple: bool,
        default: Option<Value>,
    },
    Text {
        id: String,
        label: String,
        context: Option<String>,
        required: bool,
        placeholder: Option<String>,
        default: Option<String>,
        multiline: bool,
        /// Longest answer, in bytes.
        max: usize,
    },
    Confirm {
        id: String,
        label: String,
        context: Option<String>,
        required: bool,
        yes: Option<String>,
        no: Option<String>,
        default: Option<bool>,
    },
}

/// One option of a choice question.
#[derive(Debug, Serialize)]
pub struct OptionView {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
}

impl BlockView {
    /// `record`: the message's form record (a form block always has one).
    pub(crate) fn of(b: &Block, record: Option<&FormRecord>) -> Self {
        match b {
            Block::Markdown { text } => BlockView::Markdown { text: text.clone() },
            Block::Fields { items } => BlockView::Fields {
                items: items.iter().map(|f| FieldView { label: f.label.clone(), value: f.value.clone() }).collect(),
            },
            Block::Form(f) => {
                let (state, draft, answers) = match record.map(|r| (&r.state, &r.draft)) {
                    Some((form::FormState::Submitted { answers, .. }, _)) => {
                        ("submitted", json!({}), Some(form::answers_json(f, answers)))
                    }
                    Some((state, draft)) => (state.as_str(), form::partial_json(draft), None),
                    None => ("open", json!({}), None),
                };
                BlockView::Form {
                    id: f.id.clone(),
                    title: f.title.clone(),
                    submit: f.submit.clone(),
                    questions: f.questions.iter().map(QuestionView::of).collect(),
                    state,
                    draft,
                    answers,
                }
            }
        }
    }
}

impl QuestionView {
    pub(crate) fn of(q: &form::Question) -> Self {
        let (id, label, context, required) = (q.id.clone(), q.label.clone(), q.context.clone(), q.required);
        match &q.kind {
            QuestionKind::Choice { options, multiple, default } => QuestionView::Choice {
                id,
                label,
                context,
                required,
                options: options
                    .iter()
                    .map(|o| OptionView { id: o.id.clone(), label: o.label.clone(), description: o.description.clone() })
                    .collect(),
                multiple: *multiple,
                default: default.as_ref().map(|d| match d {
                    ChoiceDefault::One(o) => json!(o),
                    ChoiceDefault::Many(os) => json!(os),
                }),
            },
            QuestionKind::Text { placeholder, default, multiline, max } => QuestionView::Text {
                id,
                label,
                context,
                required,
                placeholder: placeholder.clone(),
                default: default.clone(),
                multiline: *multiline,
                max: *max,
            },
            QuestionKind::Confirm { yes, no, default } => {
                QuestionView::Confirm { id, label, context, required, yes: yes.clone(), no: no.clone(), default: *default }
            }
        }
    }
}

/// One row of a `fields` block.
#[derive(Debug, Serialize)]
pub struct FieldView {
    pub label: String,
    pub value: String,
}

impl FeedItemView {
    /// `item` of `thread_feed` (a submission's answers are typed by its
    /// message's form).
    pub fn of(item: &FeedItem, thread_feed: &[FeedItem]) -> Self {
        let (seq, at) = (item.seq, item.at);
        match &item.kind {
            ItemKind::Message { id, blocks, edited, withdrawn, form } => FeedItemView::Message {
                seq,
                at,
                id: id.clone(),
                blocks: blocks.iter().map(|b| BlockView::of(b, form.as_ref())).collect(),
                edited: *edited,
                withdrawn: *withdrawn,
            },
            ItemKind::Submission { message, form: form_id, answers, .. } => FeedItemView::Submission {
                seq,
                at,
                message: message.clone(),
                form: form_id.clone(),
                // Every question, as the owner's event had them, while the
                // message still holds the form (it always does: submitted
                // forms are frozen).
                answers: match feed::form_of(thread_feed, message) {
                    Some((f, _)) if &f.id == form_id => form::answers_json(f, answers),
                    _ => form::partial_json(answers),
                },
            },
            ItemKind::Reply { text, .. } => FeedItemView::Reply { seq, at, text: text.clone() },
            ItemKind::Action { action, label, .. } => {
                FeedItemView::Action { seq, at, action: action.clone(), label: label.clone() }
            }
            ItemKind::Marker(m) => {
                let (marker, from, to) = match m {
                    Marker::Done { .. } => ("done", None, None),
                    Marker::Reopen { .. } => ("reopen", None, None),
                    Marker::State { from, to } => ("state", Some(from.as_str().to_string()), Some(to.as_str().to_string())),
                    Marker::Status { from, to } => ("status", from.clone(), to.clone()),
                };
                FeedItemView::Marker { seq, at, marker, from, to }
            }
        }
    }
}

#[derive(Debug, Serialize)]
pub struct NoteView {
    pub id: u64,
    pub level: &'static str,
    pub msg: String,
    pub link: Option<String>,
    pub at: u64,
}

fn kind_str(kind: Kind) -> &'static str {
    match kind {
        Kind::Notify => "notify",
        Kind::Thread => "thread",
    }
}

fn host_verb_str(verb: &HostVerb) -> &'static str {
    match verb {
        HostVerb::Vscode(_) => "vscode",
        HostVerb::Terminal(_) => "terminal",
        HostVerb::Logs(_) => "logs",
        HostVerb::Forward(_) => "forward",
        HostVerb::Open(_) => "open",
        HostVerb::Rm(_) => "rm",
    }
}

impl ThreadSummary {
    pub fn of(t: &Thread) -> Self {
        let title = match t.kind {
            Kind::Thread => t.title.clone(),
            Kind::Notify => t.head().map_or("", |r| r.msg.lines().next().unwrap_or("")).to_string(),
        };
        Self {
            id: t.id,
            owner: t.owner.clone(),
            owner_name: t.owner_name.clone(),
            key: t.key.clone(),
            kind: kind_str(t.kind),
            state: t.state.map(|s| s.as_str()),
            status: t.status.clone(),
            title,
            level: t.head().map(|r| r.level.as_str()),
            unread: t.unread,
            archived: t.archived,
            needs_you: t.needs_you(),
            changed_at: t.changed_at(),
        }
    }
}

impl ThreadDetail {
    pub fn of(t: &Thread) -> Self {
        Self {
            summary: ThreadSummary::of(t),
            link: t.link.clone().or_else(|| t.head().and_then(|r| r.link.clone())),
            child: t.child.clone(),
            compose: t
                .compose
                .as_ref()
                .map(|c| ComposeView { placeholder: c.placeholder.clone(), hint: c.hint.clone() }),
            actions: t
                .actions
                .iter()
                .map(|a| ActionView {
                    id: a.id.clone(),
                    label: a.label.clone(),
                    done: a.done,
                    host: a.host.as_ref().map(host_verb_str),
                    sends_event: a.enqueues_event(),
                })
                .collect(),
            feed: t.feed.iter().map(|i| FeedItemView::of(i, &t.feed)).collect(),
            notes: t
                .notes
                .iter()
                .map(|n| NoteView {
                    id: n.id,
                    level: n.record.level.as_str(),
                    msg: n.record.msg.clone(),
                    link: n.record.link.clone(),
                    at: n.record.at,
                })
                .collect(),
            events_pending: t.events.len(),
        }
    }
}

/// The threads in `view`, last change first, like the dashboard's list:
/// `inbox.threads.list` and `devsandbox inbox ls`.
pub fn summaries(inbox: &Inbox, view: super::View) -> Vec<ThreadSummary> {
    let mut rows: Vec<&Thread> = inbox.threads.iter().filter(|t| view.shows(t)).collect();
    // Stable: ties keep the store's order (newest arrival first).
    rows.sort_by_key(|t| std::cmp::Reverse(t.changed_at()));
    rows.into_iter().map(ThreadSummary::of).collect()
}

// ---- addressing -----------------------------------------------------------

/// How every `inbox.thread.*` names its thread: `{"thread": id}`, or
/// `{"owner": <instance name or id>, "key": …}` for a dispatcher thread.
#[derive(Debug, Default, Deserialize)]
pub struct Addr {
    #[serde(default)]
    pub thread: Option<u64>,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub key: Option<String>,
}

impl Addr {
    /// Thread `id` by its store id.
    pub fn id(id: u64) -> Self {
        Self { thread: Some(id), ..Self::default() }
    }

    #[cfg_attr(not(unix), allow(dead_code))] // only the daemon (unix) uses it
    pub fn is_empty(&self) -> bool {
        self.thread.is_none() && self.owner.is_none() && self.key.is_none()
    }
}

/// The thread `addr` names. `owner` matches the instance id first, then the
/// instance name (`owner_name`); a name can have named several instances
/// over time (removed ones keep theirs on archived threads), so a live
/// thread wins, then the most recently changed.
pub fn resolve<'a>(inbox: &'a Inbox, addr: &Addr) -> Result<&'a Thread, ApiError> {
    let id = match (addr.thread, &addr.owner, &addr.key) {
        (Some(id), None, None) => Some(id),
        (None, Some(owner), Some(key)) => ops::thread_id(inbox, owner, key).or_else(|| {
            inbox
                .threads
                .iter()
                .filter(|t| t.kind == Kind::Thread && t.owner_name == *owner && t.key.as_deref() == Some(key))
                .min_by_key(|t| (t.archived, std::cmp::Reverse(t.changed_at())))
                .map(|t| t.id)
        }),
        _ => return Err(ApiError::invalid("name the thread by `thread` (its id), or by `owner` and `key`")),
    };
    let found = id.and_then(|id| inbox.threads.iter().find(|t| t.id == id));
    found.ok_or_else(|| match (addr.thread, &addr.owner, &addr.key) {
        (Some(id), ..) => ApiError::not_found(format!("no thread {id}")),
        (_, Some(owner), Some(key)) => ApiError::not_found(format!("no thread `{key}` from `{owner}`")),
        _ => ApiError::not_found("no such thread"),
    })
}

/// The target of a user op (act, reply, done, reopen): a live dispatcher
/// thread. A notify record has no owner to answer it; an archived thread's
/// owner is gone, so it's read-only.
pub fn user_target<'a>(inbox: &'a Inbox, addr: &Addr) -> Result<&'a Thread, ApiError> {
    let t = resolve(inbox, addr)?;
    if t.kind != Kind::Thread {
        return Err(ApiError::invalid(format!("thread {} is a notification, not an owner thread", t.id)));
    }
    if t.archived {
        return Err(ApiError::denied(format!("thread {} is archived: its owner was removed", t.id)));
    }
    Ok(t)
}
// ---- user-op decisions ----------------------------------------------------

/// `inbox.thread.done`.
pub fn decide_done(inbox: &Inbox, addr: &Addr) -> Result<Vec<Op>, ApiError> {
    Ok(vec![Op::MarkDone(user_target(inbox, addr)?.id)])
}

/// `inbox.thread.reopen`.
pub fn decide_reopen(inbox: &Inbox, addr: &Addr) -> Result<Vec<Op>, ApiError> {
    Ok(vec![Op::Reopen(user_target(inbox, addr)?.id)])
}

/// `inbox.thread.act`: a dispatcher action's event half. Host verbs run in
/// the client that shows the button (the dashboard); a host-only action is
/// refused rather than pretend it ran.
pub fn decide_act(inbox: &Inbox, addr: &Addr, action: &str) -> Result<Vec<Op>, ApiError> {
    let t = user_target(inbox, addr)?;
    let Some(a) = t.actions.iter().find(|a| a.id == action) else {
        return Err(ApiError::not_found(format!("thread {} has no action `{action}`", t.id)));
    };
    if !a.enqueues_event() {
        return Err(ApiError::invalid(format!(
            "action `{}` only runs on the host ({}); the API doesn't run host verbs",
            a.id,
            a.host.as_ref().map_or("?", host_verb_str)
        )));
    }
    Ok(vec![Op::Act { thread: t.id, action: a.id.clone() }])
}

/// `inbox.thread.reply`: free text to a thread that takes replies.
pub fn decide_reply(inbox: &Inbox, addr: &Addr, text: &str) -> Result<Vec<Op>, ApiError> {
    if text.trim().is_empty() {
        return Err(ApiError::invalid("empty reply"));
    }
    let t = user_target(inbox, addr)?;
    if t.compose.is_none() {
        return Err(ApiError::denied(format!("thread {} takes no replies", t.id)));
    }
    Ok(vec![Op::Reply { thread: t.id, text: text.to_string() }])
}

/// The form of `message` on `addr`'s thread, and `answers` typed by its
/// questions: `not-found` (thread, message, no form), `invalid` (unknown
/// question, wrong type).
fn form_target<'a>(
    inbox: &'a Inbox,
    addr: &Addr,
    message: &str,
    answers: &serde_json::Map<String, Value>,
) -> Result<(&'a Thread, &'a form::Form, &'a FormRecord, std::collections::BTreeMap<String, form::Answer>), ApiError> {
    let t = user_target(inbox, addr)?;
    let (f, record) = feed::form_of(&t.feed, message)
        .ok_or_else(|| ApiError::not_found(format!("thread {} has no message `{message}` with a form", t.id)))?;
    let answers = form::answers_from_json(f, answers).map_err(ApiError::invalid)?;
    Ok((t, f, record, answers))
}

fn form_error(e: FormError) -> ApiError {
    match e {
        FormError::Closed(why) => ApiError::closed_form(why),
        FormError::Invalid(why) => ApiError::invalid(why),
    }
}

/// `inbox.form.saveDraft`: a partial set of answers as the form's draft (no
/// event), checked first so a bad one is refused rather than dropped.
#[cfg_attr(not(unix), allow(dead_code))] // only the daemon (unix) uses it
pub fn decide_save_draft(
    inbox: &Inbox,
    addr: &Addr,
    message: &str,
    answers: &serde_json::Map<String, Value>,
) -> Result<Vec<Op>, ApiError> {
    let (t, f, record, answers) = form_target(inbox, addr, message, answers)?;
    form::validate_draft(f, record, &answers).map_err(form_error)?;
    Ok(vec![Op::SaveDraft { thread: t.id, message: message.to_string(), answers }])
}

/// `inbox.form.submit`: any subset of answers (the draft and the defaults
/// fill in the rest); the owner gets one `submit` event with every answer.
pub fn decide_submit(
    inbox: &Inbox,
    addr: &Addr,
    message: &str,
    answers: &serde_json::Map<String, Value>,
) -> Result<Vec<Op>, ApiError> {
    let (t, f, record, answers) = form_target(inbox, addr, message, answers)?;
    form::resolve_submission(f, record, &answers).map_err(form_error)?;
    Ok(vec![Op::Submit { thread: t.id, message: message.to_string(), answers }])
}
