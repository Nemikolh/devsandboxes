//! Forms (docs/inbox-redesign.md, *Forms*): a `form` block in an owner's
//! message is a list of questions the user answers and submits once.
//!
//! Three halves, all pure:
//!
//! - **Schema**: the store types ([`Form`], [`Question`]) and [`check`], what
//!   `thread send` runs on a form block's wire shape (`message.rs` parses it).
//! - **Lifecycle** ([`FormRecord`] on the message item): `open` →
//!   `submitted` (answers frozen with the message) or `withdrawn` (the owner
//!   withdrew the message, or re-sent it without the form). A half-filled
//!   **draft** lives in the record too, so it survives a dashboard restart
//!   and every client sees the same one. How a re-send moves the record is
//!   [`resend`].
//! - **Answers**: [`validate_draft`] and [`resolve_submission`] check a
//!   user's answers against the form, so the API can refuse a bad op with a
//!   proper code *before* applying it, and [`answers_json`] renders the
//!   complete answer set an owner receives.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::feed::Block;
use super::sanitize::sanitize;
use crate::devsbd::control::{valid_message_id, MAX_MESSAGE_ID};

/// Questions per form.
pub const MAX_QUESTIONS: usize = 30;
/// Options per choice question.
pub const MAX_OPTIONS: usize = 20;
/// Largest `max` a text question may set, in bytes.
pub const MAX_TEXT: usize = 8 * 1024;
/// A text question's `max` when it sets none.
pub const DEFAULT_TEXT_MAX: usize = 2 * 1024;
/// The submit button's label when the form sets none.
pub const DEFAULT_SUBMIT: &str = "Submit";
const MAX_TITLE: usize = 200;
const MAX_SUBMIT: usize = 40;
/// A question's or an option's label (inline markdown, one line).
const MAX_LABEL: usize = 200;
/// A question's `context`, an option's `description` (markdown).
const MAX_CONTEXT: usize = 4 * 1024;
const MAX_PLACEHOLDER: usize = 200;
/// A confirm question's `yes` / `no` labels.
const MAX_YES_NO: usize = 40;

/// A form block as stored, in the shape `thread send` takes (so `thread ls
/// --feed` hands an owner back what it sent). Defaults are filled in:
/// `submit`, each question's `required`, a text question's `max`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Form {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub submit: String,
    pub questions: Vec<Question>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    /// Unique in the form; what the answers are keyed by.
    pub id: String,
    /// Inline markdown, one line.
    pub label: String,
    /// Markdown shown above the input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    pub required: bool,
    #[serde(flatten)]
    pub kind: QuestionKind,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum QuestionKind {
    Choice {
        options: Vec<ChoiceOption>,
        #[serde(default)]
        multiple: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<ChoiceDefault>,
    },
    Text {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
        /// Prefilled and editable: how an owner offers a draft for editing.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<String>,
        #[serde(default)]
        multiline: bool,
        /// Longest answer, in bytes.
        max: usize,
    },
    Confirm {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        yes: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        no: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<bool>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChoiceOption {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// A choice question's `default`: an option id, or ids when `multiple`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ChoiceDefault {
    One(String),
    Many(Vec<String>),
}

/// One answer, typed by its question. Stored externally tagged
/// (`{"choice":"post"}`); owners and API clients see [`Answer::to_json`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Answer {
    /// A single-choice question: the option id.
    Choice(String),
    /// A `multiple` choice question: the option ids, in the order given.
    Choices(Vec<String>),
    Text(String),
    Confirm(bool),
}

impl Answer {
    /// The wire value: choice → id, multiple → `[ids]`, text → string,
    /// confirm → bool.
    pub fn to_json(&self) -> Value {
        match self {
            Answer::Choice(id) => Value::String(id.clone()),
            Answer::Choices(ids) => Value::Array(ids.iter().cloned().map(Value::String).collect()),
            Answer::Text(text) => Value::String(text.clone()),
            Answer::Confirm(b) => Value::Bool(*b),
        }
    }
}

/// A form's lifecycle and draft, kept on its message item by the host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormRecord {
    /// The form's id; kept when a re-send drops the block, so `thread ls
    /// --feed` can still say which form was withdrawn.
    pub id: String,
    pub state: FormState,
    /// The user's half-filled answers (open forms only).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub draft: BTreeMap<String, Answer>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum FormState {
    Open,
    /// Answered, complete (defaults filled in); `client` is the user's
    /// audit, never sent to the owner.
    Submitted { at: u64, answers: BTreeMap<String, Answer>, client: String },
    Withdrawn,
}

impl FormState {
    pub fn as_str(&self) -> &'static str {
        match self {
            FormState::Open => "open",
            FormState::Submitted { .. } => "submitted",
            FormState::Withdrawn => "withdrawn",
        }
    }
}

impl FormRecord {
    pub fn open(id: &str) -> FormRecord {
        FormRecord { id: id.to_string(), state: FormState::Open, draft: BTreeMap::new() }
    }

    pub fn is_open(&self) -> bool {
        self.state == FormState::Open
    }

    /// The owner withdrew the message: an open form is withdrawn with it
    /// (its draft goes); a submitted one stays submitted.
    pub fn withdraw(&mut self) {
        if self.is_open() {
            self.state = FormState::Withdrawn;
            self.draft.clear();
        }
    }
}

/// The form block in `blocks`, if any (there is at most one).
pub fn form_in(blocks: &[Block]) -> Option<&Form> {
    blocks.iter().find_map(|b| match b {
        Block::Form(f) => Some(f),
        _ => None,
    })
}

/// What a re-send of a message (same id) becomes ([`resend`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resent {
    /// The blocks to store.
    pub blocks: Vec<Block>,
    pub record: Option<FormRecord>,
    /// A form is open now that wasn't before: news, like a new message.
    pub opened: bool,
}

/// A re-send of a message holding `old_blocks` and `old` with `new` blocks:
///
/// - **Submitted: frozen.** The new blocks replace the others, but the form
///   block is the stored one (at the new form's position, else its old one):
///   the user's answers stay attached to the questions they answered. A
///   dispatcher re-asserting its original message is a no-op.
/// - **Open**, the new blocks carry a form: it replaces the old one, and the
///   draft keeps only the answers still valid for it (same question id,
///   compatible type, valid value).
/// - **Open**, no form any more: withdrawn.
/// - **Withdrawn or no form before**, a form now: a fresh open form
///   (`opened`).
pub fn resend(old_blocks: &[Block], old: Option<&FormRecord>, new: Vec<Block>) -> Resent {
    let new_form = form_in(&new).cloned();
    match (old, new_form) {
        (Some(r @ FormRecord { state: FormState::Submitted { .. }, .. }), _) => {
            let Some(kept) = old_blocks.iter().position(|b| matches!(b, Block::Form(_))) else {
                return Resent { blocks: new, record: Some(r.clone()), opened: false };
            };
            let at = new.iter().position(|b| matches!(b, Block::Form(_)));
            let mut blocks: Vec<Block> = new.into_iter().filter(|b| !matches!(b, Block::Form(_))).collect();
            let at = at.unwrap_or(kept).min(blocks.len());
            blocks.insert(at, old_blocks[kept].clone());
            Resent { blocks, record: Some(r.clone()), opened: false }
        }
        (Some(r), Some(form)) if r.is_open() => {
            let draft = r
                .draft
                .iter()
                .filter(|(q, a)| question(&form, q).is_some_and(|q| check_answer(q, a).is_ok()))
                .map(|(q, a)| (q.clone(), a.clone()))
                .collect();
            let record = FormRecord { id: form.id.clone(), state: FormState::Open, draft };
            Resent { blocks: new, record: Some(record), opened: false }
        }
        (Some(r), None) if r.is_open() => {
            let mut record = r.clone();
            record.withdraw();
            Resent { blocks: new, record: Some(record), opened: false }
        }
        (_, Some(form)) => Resent { blocks: new, record: Some(FormRecord::open(&form.id)), opened: true },
        (old, None) => Resent { blocks: new, record: old.cloned(), opened: false },
    }
}

fn question<'a>(form: &'a Form, id: &str) -> Option<&'a Question> {
    form.questions.iter().find(|q| q.id == id)
}

// ---- schema ---------------------------------------------------------------

/// The wire shape of a form block, minus `type` (`message.rs` dispatches on
/// it). Unknown fields are rejected, like every `thread send` field.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WireForm {
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    submit: Option<String>,
    questions: Vec<WireQuestion>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
enum WireQuestion {
    Choice {
        id: String,
        label: String,
        #[serde(default)]
        context: Option<String>,
        #[serde(default)]
        required: Option<bool>,
        options: Vec<WireOption>,
        #[serde(default)]
        multiple: bool,
        #[serde(default)]
        default: Option<ChoiceDefault>,
    },
    Text {
        id: String,
        label: String,
        #[serde(default)]
        context: Option<String>,
        #[serde(default)]
        required: Option<bool>,
        #[serde(default)]
        placeholder: Option<String>,
        #[serde(default)]
        default: Option<String>,
        #[serde(default)]
        multiline: bool,
        #[serde(default)]
        max: Option<usize>,
    },
    Confirm {
        id: String,
        label: String,
        #[serde(default)]
        context: Option<String>,
        #[serde(default)]
        required: Option<bool>,
        #[serde(default)]
        yes: Option<String>,
        #[serde(default)]
        no: Option<String>,
        #[serde(default)]
        default: Option<bool>,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireOption {
    id: String,
    label: String,
    #[serde(default)]
    description: Option<String>,
}

/// Length and control-character check, as for every `thread send` text.
fn text(field: &str, value: &str, max: usize) -> Result<(), String> {
    super::thread::check_text(field, value, max)
}

/// [`text`], and on one line, not blank.
fn one_line(field: &str, value: &str, max: usize) -> Result<(), String> {
    text(field, value, max)?;
    if value.contains(['\n', '\r']) {
        return Err(format!("`{field}` spans lines"));
    }
    if value.trim().is_empty() {
        return Err(format!("`{field}` is empty"));
    }
    Ok(())
}

fn id_rule(field: &str, id: &str) -> Result<(), String> {
    if valid_message_id(id) {
        return Ok(());
    }
    Err(format!("bad `{field}` `{id}`: lowercase letters, digits and `-`, 1-{MAX_MESSAGE_ID} chars"))
}

/// Check form block number `n` (1-based, for errors) and fill in its
/// defaults. `Err` is one line naming what to fix.
pub(super) fn check(n: usize, wire: WireForm) -> Result<Form, String> {
    let at = |field: &str| format!("blocks[{n}].{field}");
    id_rule(&at("id"), &wire.id)?;
    if let Some(title) = &wire.title {
        one_line(&at("title"), title, MAX_TITLE)?;
    }
    let submit = wire.submit.unwrap_or_else(|| DEFAULT_SUBMIT.to_string());
    one_line(&at("submit"), &submit, MAX_SUBMIT)?;
    if wire.questions.is_empty() {
        return Err(format!("block {n}: form has no questions"));
    }
    if wire.questions.len() > MAX_QUESTIONS {
        return Err(format!("block {n}: {} questions, at most {MAX_QUESTIONS} allowed", wire.questions.len()));
    }
    let mut seen = BTreeSet::new();
    let questions = wire
        .questions
        .into_iter()
        .map(|q| {
            let q = question_of(n, q)?;
            if !seen.insert(q.id.clone()) {
                return Err(format!("block {n}: question id `{}` repeats", q.id));
            }
            Ok(q)
        })
        .collect::<Result<_, String>>()?;
    Ok(Form { id: wire.id, title: wire.title, submit, questions })
}

fn question_of(n: usize, q: WireQuestion) -> Result<Question, String> {
    let (id, label, context, required, kind) = match q {
        WireQuestion::Choice { id, label, context, required, options, multiple, default } => {
            (id, label, context, required.unwrap_or(true), WireKind::Choice { options, multiple, default })
        }
        WireQuestion::Text { id, label, context, required, placeholder, default, multiline, max } => {
            (id, label, context, required.unwrap_or(false), WireKind::Text { placeholder, default, multiline, max })
        }
        WireQuestion::Confirm { id, label, context, required, yes, no, default } => {
            (id, label, context, required.unwrap_or(true), WireKind::Confirm { yes, no, default })
        }
    };
    id_rule(&format!("blocks[{n}] question id"), &id)?;
    let at = |field: &str| format!("blocks[{n}] `{id}` {field}");
    one_line(&at("label"), &label, MAX_LABEL)?;
    if let Some(context) = &context {
        text(&at("context"), context, MAX_CONTEXT)?;
    }
    let kind = match kind {
        WireKind::Choice { options, multiple, default } => {
            if options.is_empty() {
                return Err(format!("block {n}: `{id}` has no options"));
            }
            if options.len() > MAX_OPTIONS {
                return Err(format!("block {n}: `{id}` has {} options, at most {MAX_OPTIONS} allowed", options.len()));
            }
            let mut ids = BTreeSet::new();
            let options = options
                .into_iter()
                .map(|o| {
                    id_rule(&at("option id"), &o.id)?;
                    if !ids.insert(o.id.clone()) {
                        return Err(format!("block {n}: `{id}` option `{}` repeats", o.id));
                    }
                    one_line(&at(&format!("option `{}` label", o.id)), &o.label, MAX_LABEL)?;
                    if let Some(d) = &o.description {
                        text(&at(&format!("option `{}` description", o.id)), d, MAX_CONTEXT)?;
                    }
                    Ok(ChoiceOption { id: o.id, label: o.label, description: o.description })
                })
                .collect::<Result<Vec<_>, String>>()?;
            let known = |o: &String| ids.contains(o);
            match (&default, multiple) {
                (None, _) => {}
                (Some(ChoiceDefault::One(o)), false) if known(o) => {}
                (Some(ChoiceDefault::Many(os)), true) if os.iter().all(known) => {
                    if os.iter().collect::<BTreeSet<_>>().len() != os.len() {
                        return Err(format!("block {n}: `{id}` default repeats an option"));
                    }
                }
                (Some(ChoiceDefault::One(_)), true) => {
                    return Err(format!("block {n}: `{id}` is multiple: its default is a list of option ids"));
                }
                (Some(ChoiceDefault::Many(_)), false) => {
                    return Err(format!("block {n}: `{id}` is not multiple: its default is one option id"));
                }
                (Some(_), _) => return Err(format!("block {n}: `{id}` default names an unknown option")),
            }
            QuestionKind::Choice { options, multiple, default }
        }
        WireKind::Text { placeholder, default, multiline, max } => {
            let max = max.unwrap_or(DEFAULT_TEXT_MAX);
            if !(1..=MAX_TEXT).contains(&max) {
                return Err(format!("block {n}: `{id}` max {max}: 1-{MAX_TEXT} bytes"));
            }
            if let Some(p) = &placeholder {
                text(&at("placeholder"), p, MAX_PLACEHOLDER)?;
            }
            if let Some(d) = &default {
                text(&at("default"), d, MAX_TEXT)?;
                // Measured as stored: the sink expands tabs, and the default
                // must still be a valid answer once it has.
                if sanitize(d).len() > max {
                    return Err(format!("block {n}: `{id}` default is longer than its max ({max} bytes)"));
                }
                if !multiline && d.contains(['\n', '\r']) {
                    return Err(format!("block {n}: `{id}` default spans lines but the question isn't multiline"));
                }
            }
            QuestionKind::Text { placeholder, default, multiline, max }
        }
        WireKind::Confirm { yes, no, default } => {
            for (field, label) in [("yes", &yes), ("no", &no)] {
                if let Some(label) = label {
                    one_line(&at(field), label, MAX_YES_NO)?;
                }
            }
            QuestionKind::Confirm { yes, no, default }
        }
    };
    Ok(Question { id, label, context, required, kind })
}

/// [`WireQuestion`]'s type-specific half.
enum WireKind {
    Choice { options: Vec<WireOption>, multiple: bool, default: Option<ChoiceDefault> },
    Text { placeholder: Option<String>, default: Option<String>, multiline: bool, max: Option<usize> },
    Confirm { yes: Option<String>, no: Option<String>, default: Option<bool> },
}

/// `form` with every container-provided string [`sanitize`]d (the sink's
/// pass over a `thread send`).
pub fn sanitized(form: Form) -> Form {
    let opt = |s: Option<String>| s.map(|s| sanitize(&s));
    let questions = form
        .questions
        .into_iter()
        .map(|q| {
            let kind = match q.kind {
                QuestionKind::Choice { options, multiple, default } => QuestionKind::Choice {
                    options: options
                        .into_iter()
                        .map(|o| ChoiceOption { id: sanitize(&o.id), label: sanitize(&o.label), description: opt(o.description) })
                        .collect(),
                    multiple,
                    default: default.map(|d| match d {
                        ChoiceDefault::One(o) => ChoiceDefault::One(sanitize(&o)),
                        ChoiceDefault::Many(os) => ChoiceDefault::Many(os.iter().map(|o| sanitize(o)).collect()),
                    }),
                },
                QuestionKind::Text { placeholder, default, multiline, max } => {
                    QuestionKind::Text { placeholder: opt(placeholder), default: opt(default), multiline, max }
                }
                QuestionKind::Confirm { yes, no, default } => QuestionKind::Confirm { yes: opt(yes), no: opt(no), default },
            };
            Question { id: sanitize(&q.id), label: sanitize(&q.label), context: opt(q.context), required: q.required, kind }
        })
        .collect();
    Form { id: sanitize(&form.id), title: opt(form.title), submit: sanitize(&form.submit), questions }
}

// ---- answers --------------------------------------------------------------

/// Why a draft or a submission was refused; the API's error codes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FormError {
    /// The form isn't open (submitted or withdrawn): `closed-form`.
    Closed(String),
    /// An unknown question, a wrong type, a bad value, or a required
    /// question left unanswered: `invalid`.
    Invalid(String),
}

/// Whether `a` is a valid answer to `q`: the right type for it, known
/// option ids (no repeats), text within `max`.
pub fn check_answer(q: &Question, a: &Answer) -> Result<(), String> {
    let id = &q.id;
    match (&q.kind, a) {
        (QuestionKind::Choice { options, multiple: false, .. }, Answer::Choice(o)) => {
            if !options.iter().any(|x| &x.id == o) {
                return Err(format!("`{id}`: no option `{o}`"));
            }
        }
        (QuestionKind::Choice { options, multiple: true, .. }, Answer::Choices(os)) => {
            if let Some(o) = os.iter().find(|o| !options.iter().any(|x| &x.id == *o)) {
                return Err(format!("`{id}`: no option `{o}`"));
            }
            if os.iter().collect::<BTreeSet<_>>().len() != os.len() {
                return Err(format!("`{id}`: an option is picked twice"));
            }
        }
        (QuestionKind::Choice { multiple: true, .. }, Answer::Choice(_)) => {
            return Err(format!("`{id}` takes a list of option ids"));
        }
        (QuestionKind::Choice { multiple: false, .. }, Answer::Choices(_)) => {
            return Err(format!("`{id}` takes one option id"));
        }
        (QuestionKind::Text { max, .. }, Answer::Text(t)) => {
            if t.len() > *max {
                return Err(format!("`{id}`: longer than {max} bytes"));
            }
        }
        (QuestionKind::Confirm { .. }, Answer::Confirm(_)) => {}
        (kind, _) => return Err(format!("`{id}` is a {} question", kind_str(kind))),
    }
    Ok(())
}

fn kind_str(kind: &QuestionKind) -> &'static str {
    match kind {
        QuestionKind::Choice { .. } => "choice",
        QuestionKind::Text { .. } => "text",
        QuestionKind::Confirm { .. } => "confirm",
    }
}

/// `answers` checked against `form`, text [`sanitize`]d first (it is user
/// input headed for a terminal and an owner): every id a question, every
/// value valid ([`check_answer`]). One bad answer refuses them all.
fn checked(form: &Form, answers: &BTreeMap<String, Answer>) -> Result<BTreeMap<String, Answer>, FormError> {
    answers
        .iter()
        .map(|(id, a)| {
            let q = question(form, id).ok_or_else(|| FormError::Invalid(format!("form `{}` has no question `{id}`", form.id)))?;
            let a = match a {
                Answer::Text(t) => Answer::Text(sanitize(t)),
                other => other.clone(),
            };
            check_answer(q, &a).map_err(FormError::Invalid)?;
            Ok((id.clone(), a))
        })
        .collect()
}

fn require_open(form: &Form, record: &FormRecord) -> Result<(), FormError> {
    match record.is_open() {
        true => Ok(()),
        false => Err(FormError::Closed(format!("form `{}` is {}", form.id, record.state.as_str()))),
    }
}

/// A partial set of answers to save as the draft: checked and sanitized,
/// ready to merge over the stored draft. The form must be open.
pub fn validate_draft(
    form: &Form,
    record: &FormRecord,
    answers: &BTreeMap<String, Answer>,
) -> Result<BTreeMap<String, Answer>, FormError> {
    require_open(form, record)?;
    checked(form, answers)
}

/// The complete answers a submission of `answers` (partial) makes: each
/// question takes the given answer, else the draft's, else its default.
/// Unanswered optional text is `""` and an unanswered `multiple` choice
/// `[]`; an unanswered optional single choice or confirm stays absent
/// ([`answers_json`] shows it as `null`). Required questions must be
/// answered (text: not blank; multiple: at least one); `Invalid` lists the
/// ones that aren't. The form must be open.
pub fn resolve_submission(
    form: &Form,
    record: &FormRecord,
    answers: &BTreeMap<String, Answer>,
) -> Result<BTreeMap<String, Answer>, FormError> {
    require_open(form, record)?;
    let given = checked(form, answers)?;
    let mut out = BTreeMap::new();
    let mut missing = Vec::new();
    for q in &form.questions {
        let answer = given.get(&q.id).or_else(|| record.draft.get(&q.id)).cloned().or_else(|| default_of(q));
        // A draft from before a re-send was pruned against this form, but
        // check again: the store is the boundary.
        if let Some(a) = &answer {
            check_answer(q, a).map_err(FormError::Invalid)?;
        }
        let answered = match &answer {
            None => false,
            Some(Answer::Text(t)) => !t.trim().is_empty(),
            Some(Answer::Choices(os)) => !os.is_empty(),
            Some(_) => true,
        };
        if q.required && !answered {
            missing.push(q.id.as_str());
        }
        let filled = answer.or_else(|| match &q.kind {
            QuestionKind::Text { .. } => Some(Answer::Text(String::new())),
            QuestionKind::Choice { multiple: true, .. } => Some(Answer::Choices(Vec::new())),
            _ => None,
        });
        if let Some(a) = filled {
            out.insert(q.id.clone(), a);
        }
    }
    if !missing.is_empty() {
        return Err(FormError::Invalid(format!("required questions unanswered: {}", missing.join(", "))));
    }
    Ok(out)
}

/// A question's `default` as an answer.
pub fn default_of(q: &Question) -> Option<Answer> {
    match &q.kind {
        QuestionKind::Choice { default: Some(ChoiceDefault::One(o)), .. } => Some(Answer::Choice(o.clone())),
        QuestionKind::Choice { default: Some(ChoiceDefault::Many(os)), .. } => Some(Answer::Choices(os.clone())),
        QuestionKind::Text { default: Some(t), .. } => Some(Answer::Text(t.clone())),
        QuestionKind::Confirm { default: Some(b), .. } => Some(Answer::Confirm(*b)),
        _ => None,
    }
}

/// `answers` as the owner receives them: a JSON object with **every**
/// question of `form`, `null` for one left unanswered.
pub fn answers_json(form: &Form, answers: &BTreeMap<String, Answer>) -> Value {
    Value::Object(
        form.questions
            .iter()
            .map(|q| (q.id.clone(), answers.get(&q.id).map_or(Value::Null, Answer::to_json)))
            .collect(),
    )
}

/// `answers` (any subset) as a JSON object, for a draft.
pub fn partial_json(answers: &BTreeMap<String, Answer>) -> Value {
    Value::Object(answers.iter().map(|(id, a)| (id.clone(), a.to_json())).collect())
}

/// A client's JSON answers (an object keyed by question id) typed by
/// `form`'s questions: a string for a choice or a text question, an array
/// of strings for a `multiple` choice, a bool for a confirm. Values are only
/// typed here; [`validate_draft`] / [`resolve_submission`] check them.
pub fn answers_from_json(form: &Form, answers: &serde_json::Map<String, Value>) -> Result<BTreeMap<String, Answer>, String> {
    answers
        .iter()
        .map(|(id, v)| {
            let q = question(form, id).ok_or_else(|| format!("form `{}` has no question `{id}`", form.id))?;
            let a = match (&q.kind, v) {
                (QuestionKind::Choice { multiple: false, .. }, Value::String(s)) => Answer::Choice(s.clone()),
                (QuestionKind::Choice { multiple: true, .. }, Value::Array(items)) => Answer::Choices(
                    items
                        .iter()
                        .map(|i| i.as_str().map(str::to_string))
                        .collect::<Option<_>>()
                        .ok_or_else(|| format!("`{id}` takes a list of option ids"))?,
                ),
                (QuestionKind::Choice { multiple: true, .. }, _) => return Err(format!("`{id}` takes a list of option ids")),
                (QuestionKind::Choice { .. }, _) => return Err(format!("`{id}` takes one option id")),
                (QuestionKind::Text { .. }, Value::String(s)) => Answer::Text(s.clone()),
                (QuestionKind::Text { .. }, _) => return Err(format!("`{id}` takes a string")),
                (QuestionKind::Confirm { .. }, Value::Bool(b)) => Answer::Confirm(*b),
                (QuestionKind::Confirm { .. }, _) => return Err(format!("`{id}` takes true or false")),
            };
            Ok((id.clone(), a))
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::inbox::message::parse;

    /// The plan's example form, as `thread send` takes it.
    pub(crate) const EXAMPLE: &str = r##"{
        "type": "form",
        "id": "drafts",
        "title": "Replies to post",
        "submit": "Post replies",
        "questions": [
            {
                "id": "c-3726888733",
                "label": "greptile on `src/cost.ts:42`",
                "context": "> Consider batching these writes.\n\nNot changed: writes are already batched in `flush()`.",
                "type": "choice",
                "options": [
                    { "id": "post", "label": "Post the reply" },
                    { "id": "skip", "label": "Don't reply" }
                ],
                "default": "post"
            },
            {
                "id": "c-3726888733-text",
                "label": "Reply",
                "type": "text",
                "multiline": true,
                "default": "Already batched in `flush()` (src/cost.ts:88), so this would double-buffer.",
                "placeholder": "Reply to post on the comment"
            },
            {
                "id": "notes",
                "label": "Anything else for the agent?",
                "type": "text",
                "multiline": true,
                "required": false,
                "placeholder": "Leave empty to just post"
            }
        ]
    }"##;

    fn body(form: &str) -> String {
        format!(r#"{{"thread":"t","id":"m","blocks":[{form}]}}"#)
    }

    fn form(json: &str) -> Result<Form, String> {
        parse(&body(json)).map(|s| form_in(&s.blocks).cloned().expect("a form block"))
    }

    /// A form of `questions` (JSON, comma-joined).
    fn qs(questions: &str) -> String {
        format!(r#"{{"type":"form","id":"f","questions":[{questions}]}}"#)
    }

    pub(crate) fn example() -> Form {
        form(EXAMPLE).unwrap()
    }

    #[test]
    fn parses_the_plan_example_with_defaults_filled_in() {
        let f = example();
        assert_eq!((f.id.as_str(), f.title.as_deref(), f.submit.as_str()), ("drafts", Some("Replies to post"), "Post replies"));
        assert_eq!(f.questions.len(), 3);
        let [choice, reply, notes] = &f.questions[..] else { panic!() };
        assert!(choice.required, "choice: required by default");
        assert!(matches!(&choice.kind, QuestionKind::Choice { multiple: false, default: Some(ChoiceDefault::One(d)), options } if d == "post" && options.len() == 2));
        assert!(!reply.required, "text: optional by default");
        assert!(matches!(&reply.kind, QuestionKind::Text { multiline: true, max: DEFAULT_TEXT_MAX, default: Some(_), .. }));
        assert!(!notes.required);
        // `submit` defaults; confirm is required by default.
        let f = form(&qs(r#"{"id":"ok","label":"Merge?","type":"confirm","yes":"Merge","no":"Wait"}"#)).unwrap();
        assert_eq!(f.submit, DEFAULT_SUBMIT);
        assert!(f.questions[0].required);
        // The store shape round-trips, and is again what `thread send` takes.
        let json = serde_json::to_string(&Block::Form(example())).unwrap();
        assert_eq!(serde_json::from_str::<Block>(&json).unwrap(), Block::Form(example()));
        assert_eq!(form(&json).unwrap(), example());
    }

    #[test]
    fn rejects_unknown_fields_and_types() {
        for bad in [
            r#"{"type":"form","id":"f","questions":[],"style":"x"}"#.to_string(),
            qs(r#"{"id":"a","label":"A","type":"slider"}"#),
            qs(r#"{"id":"a","label":"A","type":"text","rows":3}"#),
            qs(r#"{"id":"a","label":"A","type":"confirm","options":[]}"#),
            qs(r#"{"id":"a","label":"A","type":"choice","options":[{"id":"x","label":"X","icon":"y"}]}"#),
            qs(r#"{"id":"a","label":"A"}"#),
        ] {
            assert!(form(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn rejects_every_invalid_field() {
        let err = |json: &str| form(json).unwrap_err();
        let text = |id: &str| format!(r#"{{"id":"{id}","label":"L","type":"text"}}"#);
        // Form.
        assert!(err(r#"{"type":"form","id":"F","questions":[]}"#).starts_with("bad `blocks[1].id` `F`"));
        assert_eq!(err(&qs("")), "block 1: form has no questions");
        let many: Vec<_> = (0..31).map(|i| text(&format!("q{i}"))).collect();
        assert_eq!(err(&qs(&many.join(","))), "block 1: 31 questions, at most 30 allowed");
        assert!(form(&qs(&many[..30].join(","))).is_ok());
        assert_eq!(err(&qs(&[text("a"), text("a")].join(","))), "block 1: question id `a` repeats");
        let titled = |t: &str| format!(r#"{{"type":"form","id":"f","title":"{t}","questions":[{}]}}"#, text("a"));
        assert!(err(&titled(&"t".repeat(201))).contains("longer than 200 bytes"));
        assert!(form(&titled(&"t".repeat(200))).is_ok());
        let submit = |s: &str| format!(r#"{{"type":"form","id":"f","submit":"{s}","questions":[{}]}}"#, text("a"));
        assert!(err(&submit(&"s".repeat(41))).contains("longer than 40 bytes"));
        assert!(err(&submit(" ")).contains("is empty"));
        // Common question fields.
        assert!(err(&qs(&text("A"))).starts_with("bad `blocks[1] question id` `A`"));
        assert!(err(&qs(r#"{"id":"a","label":"","type":"text"}"#)).contains("label` is empty"));
        assert!(err(&qs(r#"{"id":"a","label":"a\nb","type":"text"}"#)).contains("label` spans lines"));
        let label = format!(r#"{{"id":"a","label":"{}","type":"text"}}"#, "l".repeat(201));
        assert!(err(&qs(&label)).contains("longer than 200 bytes"));
        let context = |n: usize| format!(r#"{{"id":"a","label":"L","type":"text","context":"{}"}}"#, "c".repeat(n));
        assert!(err(&qs(&context(4097))).contains("longer than 4096 bytes"));
        assert!(form(&qs(&context(4096))).is_ok());
        assert!(err(&qs(r#"{"id":"a","label":"L\u001b","type":"text"}"#)).contains("control character"));
        // Choice.
        let choice = |rest: &str| qs(&format!(r#"{{"id":"c","label":"C","type":"choice",{rest}}}"#));
        assert_eq!(err(&choice(r#""options":[]"#)), "block 1: `c` has no options");
        let opts: Vec<_> = (0..21).map(|i| format!(r#"{{"id":"o{i}","label":"O"}}"#)).collect();
        assert_eq!(err(&choice(&format!(r#""options":[{}]"#, opts.join(",")))), "block 1: `c` has 21 options, at most 20 allowed");
        assert!(form(&choice(&format!(r#""options":[{}]"#, opts[..20].join(",")))).is_ok());
        let ab = r#""options":[{"id":"a","label":"A"},{"id":"b","label":"B","description":"more"}]"#;
        assert_eq!(err(&choice(r#""options":[{"id":"a","label":"A"},{"id":"a","label":"B"}]"#)), "block 1: `c` option `a` repeats");
        assert!(err(&choice(&format!(r#"{ab},"default":"z""#))).contains("unknown option"));
        assert!(err(&choice(&format!(r#"{ab},"default":["a"]"#))).contains("is not multiple"));
        assert!(err(&choice(&format!(r#"{ab},"multiple":true,"default":"a""#))).contains("is multiple"));
        assert!(err(&choice(&format!(r#"{ab},"multiple":true,"default":["a","z"]"#))).contains("unknown option"));
        assert!(err(&choice(&format!(r#"{ab},"multiple":true,"default":["a","a"]"#))).contains("repeats an option"));
        assert!(form(&choice(&format!(r#"{ab},"default":"b""#))).is_ok());
        assert!(form(&choice(&format!(r#"{ab},"multiple":true,"default":["b","a"]"#))).is_ok());
        assert!(form(&choice(&format!(r#"{ab},"multiple":true,"default":[]"#))).is_ok());
        // Text.
        let t = |rest: &str| qs(&format!(r#"{{"id":"t","label":"T","type":"text",{rest}}}"#));
        assert!(err(&t(r#""max":0"#)).contains("max 0"));
        assert!(err(&t(r#""max":8193"#)).contains("max 8193"));
        assert!(form(&t(r#""max":8192"#)).is_ok());
        assert!(err(&t(r#""max":3,"default":"four""#)).contains("longer than its max"));
        assert!(form(&t(r#""max":4,"default":"four""#)).is_ok());
        // A tab expands when stored: measured after.
        assert!(err(&t(r#""max":4,"default":"\tx""#)).contains("longer than its max"));
        assert!(err(&t(r#""default":"a\nb""#)).contains("isn't multiline"));
        assert!(form(&t(r#""multiline":true,"default":"a\nb""#)).is_ok());
        let long_default = format!(r#""default":"{}""#, "d".repeat(DEFAULT_TEXT_MAX + 1));
        assert!(err(&t(&long_default)).contains("longer than its max"), "the default max applies");
        // Confirm.
        let c = |rest: &str| qs(&format!(r#"{{"id":"y","label":"Y","type":"confirm",{rest}}}"#));
        assert!(err(&c(&format!(r#""yes":"{}""#, "y".repeat(41)))).contains("longer than 40 bytes"));
        assert!(err(&c(r#""default":"yes""#)).contains("invalid type"));
        assert!(form(&c(r#""default":false"#)).is_ok());
    }

    fn answers(pairs: &[(&str, Answer)]) -> BTreeMap<String, Answer> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    fn text(s: &str) -> Answer {
        Answer::Text(s.into())
    }

    /// A form with one of each, plus a multiple choice: `pick` (required
    /// single choice, no default), `tags` (optional multiple), `note`
    /// (required text, max 5), `sure` (optional confirm).
    pub(crate) fn mixed() -> Form {
        form(&qs(r#"
            {"id":"pick","label":"P","type":"choice","options":[{"id":"a","label":"A"},{"id":"b","label":"B"}]},
            {"id":"tags","label":"T","type":"choice","multiple":true,"required":false,"options":[{"id":"x","label":"X"},{"id":"y","label":"Y"}]},
            {"id":"note","label":"N","type":"text","required":true,"max":5},
            {"id":"sure","label":"S","type":"confirm","required":false}
        "#))
        .unwrap()
    }

    #[test]
    fn drafts_check_each_answer_and_sanitize_text() {
        let f = mixed();
        let open = FormRecord::open("f");
        let got = validate_draft(&f, &open, &answers(&[("pick", Answer::Choice("a".into())), ("note", text("a\x1bb"))])).unwrap();
        assert_eq!(got, answers(&[("pick", Answer::Choice("a".into())), ("note", text("ab"))]));
        let invalid = |a: &[(&str, Answer)]| match validate_draft(&f, &open, &answers(a)) {
            Err(FormError::Invalid(why)) => why,
            other => panic!("{other:?}"),
        };
        assert_eq!(invalid(&[("nope", text("x"))]), "form `f` has no question `nope`");
        assert_eq!(invalid(&[("pick", text("a"))]), "`pick` is a choice question");
        assert_eq!(invalid(&[("pick", Answer::Choice("z".into()))]), "`pick`: no option `z`");
        assert_eq!(invalid(&[("tags", Answer::Choice("x".into()))]), "`tags` takes a list of option ids");
        assert_eq!(invalid(&[("pick", Answer::Choices(vec!["a".into()]))]), "`pick` takes one option id");
        assert_eq!(invalid(&[("tags", Answer::Choices(vec!["x".into(), "x".into()]))]), "`tags`: an option is picked twice");
        assert_eq!(invalid(&[("note", text("toolong"))]), "`note`: longer than 5 bytes");
        assert_eq!(invalid(&[("sure", text("yes"))]), "`sure` is a confirm question");
        // One bad answer refuses the lot.
        assert!(validate_draft(&f, &open, &answers(&[("pick", Answer::Choice("a".into())), ("nope", text("x"))])).is_err());
        // Only an open form takes drafts.
        let mut gone = open.clone();
        gone.withdraw();
        assert_eq!(validate_draft(&f, &gone, &BTreeMap::new()), Err(FormError::Closed("form `f` is withdrawn".into())));
    }

    #[test]
    fn a_submission_merges_given_over_draft_over_defaults() {
        let f = example();
        let mut open = FormRecord::open("drafts");
        // Nothing given: defaults, and the optional text filled in empty.
        let got = resolve_submission(&f, &open, &BTreeMap::new()).unwrap();
        let reply = "Already batched in `flush()` (src/cost.ts:88), so this would double-buffer.";
        assert_eq!(
            got,
            answers(&[("c-3726888733", Answer::Choice("post".into())), ("c-3726888733-text", text(reply)), ("notes", text(""))])
        );
        assert_eq!(
            answers_json(&f, &got),
            serde_json::json!({"c-3726888733": "post", "c-3726888733-text": reply, "notes": ""})
        );
        // The draft beats the default; what's given beats the draft.
        open.draft = answers(&[("c-3726888733", Answer::Choice("skip".into())), ("notes", text("draft"))]);
        let got = resolve_submission(&f, &open, &answers(&[("notes", text("given"))])).unwrap();
        assert_eq!(got["c-3726888733"], Answer::Choice("skip".into()));
        assert_eq!(got["notes"], text("given"));
    }

    #[test]
    fn a_submission_needs_every_required_answer_and_valid_values() {
        let f = mixed();
        let open = FormRecord::open("f");
        let resolve = |a: &[(&str, Answer)]| resolve_submission(&f, &open, &answers(a));
        assert_eq!(resolve(&[]), Err(FormError::Invalid("required questions unanswered: pick, note".into())));
        // Blank text doesn't count as an answer.
        assert_eq!(
            resolve(&[("pick", Answer::Choice("a".into())), ("note", text("  "))]),
            Err(FormError::Invalid("required questions unanswered: note".into()))
        );
        let got = resolve(&[("pick", Answer::Choice("b".into())), ("note", text("hi"))]).unwrap();
        // Unanswered optional ones: `[]` for multiple, absent (null) otherwise.
        assert_eq!(got, answers(&[("pick", Answer::Choice("b".into())), ("tags", Answer::Choices(vec![])), ("note", text("hi"))]));
        assert_eq!(answers_json(&f, &got), serde_json::json!({"pick": "b", "tags": [], "note": "hi", "sure": null}));
        // Bad values are invalid, like in a draft.
        assert!(matches!(resolve(&[("pick", Answer::Choice("z".into())), ("note", text("hi"))]), Err(FormError::Invalid(_))));
        assert!(matches!(resolve(&[("pick", Answer::Choice("a".into())), ("note", text("123456"))]), Err(FormError::Invalid(_))));
        assert!(matches!(resolve(&[("tags", Answer::Choice("x".into()))]), Err(FormError::Invalid(_))));
        // A required multiple needs one pick.
        let f = form(&qs(r#"{"id":"m","label":"M","type":"choice","multiple":true,"options":[{"id":"x","label":"X"}]}"#)).unwrap();
        let open = FormRecord::open("f");
        assert!(matches!(resolve_submission(&f, &open, &answers(&[("m", Answer::Choices(vec![]))])), Err(FormError::Invalid(_))));
        assert!(resolve_submission(&f, &open, &answers(&[("m", Answer::Choices(vec!["x".into()]))])).is_ok());
        // Closed forms take nothing.
        let done = FormRecord { state: FormState::Submitted { at: 1, answers: BTreeMap::new(), client: "tui".into() }, ..open };
        assert_eq!(resolve_submission(&f, &done, &BTreeMap::new()), Err(FormError::Closed("form `f` is submitted".into())));
    }

    #[test]
    fn json_answers_are_typed_by_their_question() {
        let f = mixed();
        let obj = |v: Value| v.as_object().unwrap().clone();
        let got = answers_from_json(&f, &obj(serde_json::json!({"pick": "a", "tags": ["x"], "note": "n", "sure": true}))).unwrap();
        assert_eq!(
            got,
            answers(&[
                ("pick", Answer::Choice("a".into())),
                ("tags", Answer::Choices(vec!["x".into()])),
                ("note", text("n")),
                ("sure", Answer::Confirm(true)),
            ])
        );
        assert_eq!(partial_json(&got), serde_json::json!({"pick": "a", "tags": ["x"], "note": "n", "sure": true}));
        for (bad, why) in [
            (serde_json::json!({"nope": "a"}), "form `f` has no question `nope`"),
            (serde_json::json!({"pick": ["a"]}), "`pick` takes one option id"),
            (serde_json::json!({"tags": "x"}), "`tags` takes a list of option ids"),
            (serde_json::json!({"tags": [1]}), "`tags` takes a list of option ids"),
            (serde_json::json!({"note": 3}), "`note` takes a string"),
            (serde_json::json!({"sure": "yes"}), "`sure` takes true or false"),
        ] {
            assert_eq!(answers_from_json(&f, &obj(bad)).unwrap_err(), why);
        }
    }

    fn md(text: &str) -> Block {
        Block::Markdown { text: text.into() }
    }

    #[test]
    fn a_resend_while_open_keeps_the_draft_answers_that_still_fit() {
        let old = vec![md("a"), Block::Form(mixed())];
        let mut open = FormRecord::open("f");
        open.draft = answers(&[
            ("pick", Answer::Choice("b".into())),
            ("tags", Answer::Choices(vec!["y".into()])),
            ("note", text("hey")),
            ("sure", Answer::Confirm(true)),
        ]);
        // Same form: the draft stays whole.
        let same = resend(&old, Some(&open), old.clone());
        assert_eq!(same, Resent { blocks: old.clone(), record: Some(open.clone()), opened: false });
        // Changed: `pick` lost option b, `tags` became a text question,
        // `note` shrank below its draft, `sure` is gone, `new` is new.
        let changed = form(&qs(r#"
            {"id":"pick","label":"P","type":"choice","options":[{"id":"a","label":"A"}]},
            {"id":"tags","label":"T","type":"text"},
            {"id":"note","label":"N","type":"text","max":10},
            {"id":"new","label":"N","type":"confirm"}
        "#))
        .unwrap();
        let r = resend(&old, Some(&open), vec![Block::Form(changed.clone())]);
        assert_eq!(r.blocks, [Block::Form(changed)]);
        assert!(!r.opened);
        let record = r.record.unwrap();
        assert!(record.is_open());
        assert_eq!(record.draft, answers(&[("note", text("hey"))]));
        // Dropping the form block withdraws it, keeping its id.
        let r = resend(&old, Some(&open), vec![md("a")]);
        assert_eq!(r.record, Some(FormRecord { id: "f".into(), state: FormState::Withdrawn, draft: BTreeMap::new() }));
        assert!(!r.opened);
    }

    #[test]
    fn a_submitted_form_is_frozen() {
        let old = vec![md("a"), Block::Form(mixed())];
        let done = FormRecord {
            id: "f".into(),
            state: FormState::Submitted { at: 5, answers: answers(&[("note", text("hi"))]), client: "tui".into() },
            draft: BTreeMap::new(),
        };
        // The original again: no change at all.
        assert_eq!(resend(&old, Some(&done), old.clone()).blocks, old);
        // Other blocks change; the stored form stays, at the new form's place.
        let r = resend(&old, Some(&done), vec![Block::Form(example()), md("b"), md("c")]);
        assert_eq!(r.blocks, [Block::Form(mixed()), md("b"), md("c")]);
        assert_eq!(r.record.as_ref(), Some(&done));
        // No form in the re-send: the stored one stays at its old place.
        let r = resend(&old, Some(&done), vec![md("b")]);
        assert_eq!(r.blocks, [md("b"), Block::Form(mixed())]);
        let r = resend(&old, Some(&done), vec![md("b"), md("c"), md("d")]);
        assert_eq!(r.blocks, [md("b"), Block::Form(mixed()), md("c"), md("d")]);
        assert!(!r.opened);
    }

    #[test]
    fn a_form_where_there_was_none_opens() {
        let r = resend(&[md("a")], None, vec![md("a"), Block::Form(mixed())]);
        assert_eq!((r.record, r.opened), (Some(FormRecord::open("f")), true));
        let gone = FormRecord { id: "f".into(), state: FormState::Withdrawn, draft: BTreeMap::new() };
        let r = resend(&[md("a")], Some(&gone), vec![Block::Form(mixed())]);
        assert_eq!((r.record, r.opened), (Some(FormRecord::open("f")), true), "a withdrawn form comes back fresh");
        // Still no form: nothing to open, a withdrawn record stays one.
        let r = resend(&[md("a")], Some(&gone), vec![md("b")]);
        assert_eq!((r.record, r.opened), (Some(gone), false));
        let r = resend(&[md("a")], None, vec![md("b")]);
        assert_eq!((r.record, r.opened), (None, false));
    }

    #[test]
    fn records_round_trip() {
        let mut open = FormRecord::open("f");
        open.draft = answers(&[("pick", Answer::Choice("a".into())), ("sure", Answer::Confirm(false))]);
        let done = FormRecord {
            id: "f".into(),
            state: FormState::Submitted { at: 9, answers: answers(&[("tags", Answer::Choices(vec!["x".into()]))]), client: "cli".into() },
            draft: BTreeMap::new(),
        };
        for r in [open, done, FormRecord { state: FormState::Withdrawn, ..FormRecord::open("g") }] {
            let json = serde_json::to_string(&r).unwrap();
            assert_eq!(serde_json::from_str::<FormRecord>(&json).unwrap(), r, "{json}");
        }
    }

    #[test]
    fn sanitizing_reaches_every_text() {
        let f = form(&qs(r#"
            {"id":"c","label":"C\t","context":"x\r\ny","type":"choice","options":[{"id":"a","label":"A\t","description":"d\r"}]},
            {"id":"t","label":"T","type":"text","placeholder":"p\t","default":"d\t"},
            {"id":"y","label":"Y","type":"confirm","yes":"Y\t","no":"N\t"}
        "#))
        .unwrap();
        let json = serde_json::to_string(&sanitized(f)).unwrap();
        assert!(!json.contains("\\t") && !json.contains("\\r"), "{json}");
    }
}
