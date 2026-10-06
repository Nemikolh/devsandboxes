//! Inbox forms (docs/inbox-redesign.md, *Forms* and *TUI Inbox*): the open
//! forms pinned under the thread pane's header, their focus zone
//! ([`InboxFocus::Form`]) and keys, and the read-only answers of a submitted
//! one in the feed.
//!
//! What a form says is pure ([`form_lines`], [`answer_rows`]), from the form,
//! its stored record and the cursor. The answers shown are the stored draft
//! over the defaults ([`value_of`]): a pick or a finished text edit is an
//! [`Op::SaveDraft`] of that one answer, which [`App::request_inbox`] applies
//! to the local copy at once (it enqueues no event), so the next frame shows
//! it without waiting for the store. Only the cursor ([`FormEdit`]) is view
//! state here.

use std::collections::BTreeMap;
use std::ops::Range;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::inbox::form::{self, Form, FormError, Question, QuestionKind};
use crate::inbox::{Answer, FormRecord, ItemKind, Kind, Op, State, Thread};
use crate::tui::textarea::TextArea;
use crate::tui::ui;

use super::inbox::{edit_text, InboxFocus, PaneLine, Tone};
use super::App;

/// The form zone's cursor: which pinned form and question has the keys, the
/// option under the cursor, the text edit in progress, the Confirm step.
pub struct FormEdit {
    pub thread: u64,
    /// The message carrying the form (forms are keyed by it).
    pub message: String,
    /// Index into the form's questions.
    pub question: usize,
    /// Index into the focused question's options (confirm: 0 yes, 1 no).
    pub option: usize,
    /// `e` on a text question: the text being edited, in the composer box.
    pub editing: Option<TextArea>,
    /// `enter` was pressed once: the summary line shows, a second `enter`
    /// submits.
    pub confirming: bool,
}

/// An open form of a thread, as pinned.
pub struct Pinned<'a> {
    pub message: &'a str,
    pub form: &'a Form,
    pub record: &'a FormRecord,
}

/// Where in the pinned forms a screen row is: form index (into
/// [`pinned_forms`]) and question, when the row belongs to one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FormSpot {
    pub form: usize,
    pub question: Option<usize>,
}

/// `t`'s open forms, newest message first: what the pane pins. A withdrawn
/// message's form is withdrawn with it, so it never shows here.
pub fn pinned_forms(t: &Thread) -> Vec<Pinned<'_>> {
    if t.kind != Kind::Thread {
        return Vec::new();
    }
    t.feed
        .iter()
        .rev()
        .filter_map(|i| match &i.kind {
            ItemKind::Message { id, blocks, withdrawn: false, form: Some(record), .. } if record.is_open() => {
                form::form_in(blocks).map(|form| Pinned { message: id, form, record })
            }
            _ => None,
        })
        .collect()
}

/// A question's current answer: the draft's, else its default.
pub fn value_of(q: &Question, record: &FormRecord) -> Option<Answer> {
    record.draft.get(&q.id).cloned().or_else(|| form::default_of(q))
}

/// Whether `a` counts as an answer to a required question
/// (`form::resolve_submission`'s rule: text not blank, a multiple choice at
/// least one pick).
fn answered(a: Option<&Answer>) -> bool {
    match a {
        None => false,
        Some(Answer::Text(t)) => !t.trim().is_empty(),
        Some(Answer::Choices(os)) => !os.is_empty(),
        Some(_) => true,
    }
}

fn missing(q: &Question, record: &FormRecord) -> bool {
    q.required && !answered(value_of(q, record).as_ref())
}

/// The confirm step's outcome over the stored draft.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Confirm {
    /// The complete answers a submit sends (defaults filled in).
    Ready(BTreeMap<String, Answer>),
    /// Labels of the required questions left unanswered.
    Missing(Vec<String>),
    Closed,
    /// A stored answer no longer fits (shouldn't happen: the store prunes).
    Invalid(String),
}

pub fn confirm(form: &Form, record: &FormRecord) -> Confirm {
    match form::resolve_submission(form, record, &BTreeMap::new()) {
        Ok(answers) => Confirm::Ready(answers),
        Err(FormError::Closed(_)) => Confirm::Closed,
        Err(FormError::Invalid(why)) => {
            let labels: Vec<String> =
                form.questions.iter().filter(|q| missing(q, record)).map(|q| q.label.clone()).collect();
            if labels.is_empty() { Confirm::Invalid(why) } else { Confirm::Missing(labels) }
        }
    }
}

/// How many questions `answers` really answers (an empty optional text is
/// sent, but isn't an answer): the summary's count, and the folded
/// submission's in the feed.
pub(super) fn answer_count(answers: &BTreeMap<String, Answer>) -> usize {
    answers.values().filter(|a| answered(Some(a))).count()
}

/// Options the cursor moves over: a choice's, a confirm's yes/no, none for
/// text.
fn option_count(q: &Question) -> usize {
    match &q.kind {
        QuestionKind::Choice { options, .. } => options.len(),
        QuestionKind::Confirm { .. } => 2,
        QuestionKind::Text { .. } => 0,
    }
}

/// Where the cursor starts on `q`: on its current single answer, else the
/// first option.
fn option_cursor(q: &Question, value: Option<&Answer>) -> usize {
    match (&q.kind, value) {
        (QuestionKind::Choice { options, .. }, Some(Answer::Choice(id))) => {
            options.iter().position(|o| &o.id == id).unwrap_or(0)
        }
        (QuestionKind::Confirm { .. }, Some(Answer::Confirm(false))) => 1,
        _ => 0,
    }
}

/// `space` on option `option` of `q` answered `value`: a single choice
/// picks it, a multiple one toggles it (kept in the options' order), a
/// confirm picks yes (0) or no. `None` on a text question.
pub fn pick(q: &Question, value: Option<&Answer>, option: usize) -> Option<Answer> {
    match &q.kind {
        QuestionKind::Choice { options, multiple: false, .. } => options.get(option).map(|o| Answer::Choice(o.id.clone())),
        QuestionKind::Choice { options, multiple: true, .. } => {
            let o = options.get(option)?;
            let mut picked = match value {
                Some(Answer::Choices(ids)) => ids.clone(),
                _ => Vec::new(),
            };
            match picked.iter().position(|x| *x == o.id) {
                Some(i) => {
                    picked.remove(i);
                }
                None => picked.push(o.id.clone()),
            }
            Some(Answer::Choices(options.iter().filter(|x| picked.contains(&x.id)).map(|x| x.id.clone()).collect()))
        }
        QuestionKind::Confirm { .. } => Some(Answer::Confirm(option == 0)),
        QuestionKind::Text { .. } => None,
    }
}

fn yes_no(q: &Question) -> (&str, &str) {
    match &q.kind {
        QuestionKind::Confirm { yes, no, .. } => (yes.as_deref().unwrap_or("Yes"), no.as_deref().unwrap_or("No")),
        _ => ("Yes", "No"),
    }
}

/// An answer as the user reads it: option labels, the confirm's labels, the
/// text; `–` for none.
pub fn answer_text(q: &Question, a: Option<&Answer>) -> String {
    let label = |id: &str| match &q.kind {
        QuestionKind::Choice { options, .. } => {
            options.iter().find(|o| o.id == id).map_or(id.to_string(), |o| o.label.clone())
        }
        _ => id.to_string(),
    };
    match a {
        None => "–".into(),
        Some(Answer::Choice(id)) => label(id),
        Some(Answer::Choices(ids)) if ids.is_empty() => "none".into(),
        Some(Answer::Choices(ids)) => ids.iter().map(|id| label(id)).collect::<Vec<_>>().join(", "),
        Some(Answer::Text(t)) if t.trim().is_empty() => "–".into(),
        Some(Answer::Text(t)) => t.clone(),
        Some(Answer::Confirm(b)) => {
            let (yes, no) = yes_no(q);
            if *b { yes.into() } else { no.into() }
        }
    }
}

/// A submitted form's answers, read-only, as `label: value` rows; a
/// multi-line text question's answer is its own markdown line under the
/// label.
pub fn answer_rows(form: &Form, answers: &BTreeMap<String, Answer>) -> Vec<PaneLine> {
    let mut out = Vec::new();
    for q in &form.questions {
        let a = answers.get(&q.id);
        let multiline = matches!(q.kind, QuestionKind::Text { multiline: true, .. });
        match a {
            Some(Answer::Text(t)) if multiline && !t.trim().is_empty() => {
                out.push(vec![(Tone::Text, q.label.clone()), (Tone::Dim, ":".into())]);
                out.push(vec![(Tone::Markdown, t.clone())]);
            }
            _ => out.push(vec![
                (Tone::Text, q.label.clone()),
                (Tone::Dim, ": ".into()),
                (Tone::Plain, answer_text(q, a)),
            ]),
        }
    }
    out
}

/// The cursor, for [`form_lines`], when the form has the keys.
pub struct FormCursor<'a> {
    pub question: usize,
    pub option: usize,
    /// The text being edited, shown live in place of the stored answer.
    pub editing: Option<&'a str>,
    pub confirming: bool,
}

/// A pinned form's lines (the box's content) and the line range of each
/// question, so the renderer can scroll the box to the focused one.
pub struct FormLines {
    pub lines: Vec<PaneLine>,
    pub questions: Vec<Range<usize>>,
}

/// Text answers show at most this many lines in the box.
const TEXT_LINES: usize = 3;
const INDENT: &str = "    ";

/// A pinned form as lines `width` columns wide (text answers are cut to it;
/// the rest is wrapped by the renderer). Per question: `› i/N label`, a `*`
/// while it's required and unanswered, the focused question's `context`
/// (only the focused one's: the box stays short, and the context is what
/// the user reads while answering), then its widget: `(•) A   ( ) B` (one
/// pick), `[x] A  [ ] B` (several), `(•) Yes  ( ) No`, a text answer's first
/// lines or its placeholder; the option under the cursor highlighted, its
/// `description` under the widget. Then the Confirm summary while
/// `confirming`, and the key hints.
pub fn form_lines(form: &Form, record: &FormRecord, cursor: Option<&FormCursor>, width: usize) -> FormLines {
    let mut lines: Vec<PaneLine> = Vec::new();
    let mut questions = Vec::new();
    let n = form.questions.len();
    let room = width.saturating_sub(INDENT.len());
    for (i, q) in form.questions.iter().enumerate() {
        let start = lines.len();
        let at = cursor.filter(|c| c.question == i);
        let value = value_of(q, record);
        let mut head = vec![
            if at.is_some() { (Tone::Bold, "› ".to_string()) } else { (Tone::Plain, "  ".to_string()) },
            (Tone::Dim, format!("{}/{n} ", i + 1)),
            (Tone::Text, q.label.clone()),
        ];
        if missing(q, record) {
            head.push((Tone::State(State::NeedsYou), " *".into()));
        }
        lines.push(head);
        if let Some(context) = q.context.as_deref().filter(|c| at.is_some() && !c.trim().is_empty()) {
            lines.push(vec![(Tone::Markdown, context.to_string())]);
        }
        let option = at.map(|c| c.option);
        match &q.kind {
            QuestionKind::Choice { options, multiple, .. } => {
                let picked = |id: &str| match &value {
                    Some(Answer::Choice(o)) => o == id,
                    Some(Answer::Choices(os)) => os.iter().any(|o| o == id),
                    _ => false,
                };
                let marks = |on: bool| match (multiple, on) {
                    (false, true) => "(•) ",
                    (false, false) => "( ) ",
                    (true, true) => "[x] ",
                    (true, false) => "[ ] ",
                };
                let labels: Vec<(bool, &str)> = options.iter().map(|o| (picked(&o.id), o.label.as_str())).collect();
                lines.push(widget(&labels, option, marks));
                if let Some(desc) = option.and_then(|o| options.get(o)).and_then(|o| o.description.as_deref()) {
                    lines.push(vec![(Tone::Dim, format!("{INDENT}{desc}"))]);
                }
            }
            QuestionKind::Confirm { .. } => {
                let (yes, no) = yes_no(q);
                let v = match &value {
                    Some(Answer::Confirm(b)) => Some(*b),
                    _ => None,
                };
                let labels = [(v == Some(true), yes), (v == Some(false), no)];
                lines.push(widget(&labels, option, |on| if on { "(•) " } else { "( ) " }));
            }
            QuestionKind::Text { placeholder, .. } => {
                let editing = at.and_then(|c| c.editing);
                let text = match (editing, &value) {
                    (Some(t), _) => t.to_string(),
                    (None, Some(Answer::Text(t))) => t.clone(),
                    _ => String::new(),
                };
                let mark = if editing.is_some() { "✎ " } else { "" };
                if text.is_empty() {
                    let hint = placeholder.as_deref().unwrap_or("(empty)");
                    lines.push(vec![(Tone::Plain, format!("{INDENT}{mark}")), (Tone::Dim, ui::truncate(hint, room))]);
                } else {
                    let all: Vec<&str> = text.lines().collect();
                    for (j, l) in all.iter().take(TEXT_LINES).enumerate() {
                        let mark = if j == 0 { mark } else { "" };
                        lines.push(vec![(Tone::Plain, format!("{INDENT}{}", ui::truncate(&format!("{mark}{l}"), room)))]);
                    }
                    if all.len() > TEXT_LINES {
                        lines.push(vec![(Tone::Dim, format!("{INDENT}… {} more lines", all.len() - TEXT_LINES))]);
                    }
                }
            }
        }
        questions.push(start..lines.len());
    }
    let hint = match cursor {
        Some(c) if c.editing.is_some() => "[Enter] keep  [Esc] keep  [Alt-Enter] newline",
        Some(c) if c.confirming => match confirm(form, record) {
            Confirm::Ready(answers) => {
                let n = answer_count(&answers);
                let s = if n == 1 { "" } else { "s" };
                lines.push(vec![(Tone::Bold, format!("{}: submit {n} answer{s}?", form.submit))]);
                "[Enter] yes  [Esc] no"
            }
            Confirm::Missing(labels) => {
                lines.push(vec![(Tone::State(State::NeedsYou), format!("missing: {}", labels.join(", ")))]);
                "[Esc] back"
            }
            Confirm::Closed => {
                lines.push(vec![(Tone::Dim, "form is no longer open".to_string())]);
                "[Esc] back"
            }
            Confirm::Invalid(why) => {
                lines.push(vec![(Tone::State(State::NeedsYou), why)]);
                "[Esc] back"
            }
        },
        _ => "[Tab] next  [Space] pick  [e] edit  [Enter] Confirm",
    };
    lines.push(vec![(Tone::Dim, hint.to_string())]);
    FormLines { lines, questions }
}

/// One widget line: each option as its mark and label, three spaces apart;
/// the one under the cursor highlighted as a whole.
fn widget(labels: &[(bool, &str)], cursor: Option<usize>, mark: impl Fn(bool) -> &'static str) -> PaneLine {
    let mut out = vec![(Tone::Plain, INDENT.to_string())];
    for (j, (on, label)) in labels.iter().enumerate() {
        if j > 0 {
            out.push((Tone::Plain, "   ".into()));
        }
        if cursor == Some(j) {
            out.push((Tone::Cursor, format!("{}{label}", mark(*on))));
        } else {
            out.push((Tone::Plain, mark(*on).to_string()));
            out.push((Tone::Text, label.to_string()));
        }
    }
    out
}

/// A message's form in the feed: an open one is pinned above (one dim
/// line here), a submitted one shows its answers read-only, a withdrawn one
/// a dim `form withdrawn`.
pub fn feed_form_lines(f: &Form, record: Option<&FormRecord>, utc_offset: i64) -> Vec<PaneLine> {
    let title = f.title.as_deref().unwrap_or("Form");
    match record.map(|r| &r.state) {
        None | Some(crate::inbox::FormState::Open) => {
            vec![vec![(Tone::Dim, format!("form: {title} · open, pinned above"))]]
        }
        Some(crate::inbox::FormState::Submitted { at, answers, .. }) => {
            let mut out = vec![vec![(Tone::Dim, format!("form: {title} · submitted {}", super::inbox::stamp(*at, utc_offset)))]];
            out.extend(answer_rows(f, answers));
            out
        }
        Some(crate::inbox::FormState::Withdrawn) => vec![vec![(Tone::Dim, "form withdrawn".to_string())]],
    }
}

impl App {
    /// The form the cursor is on, while it's still pinned: its thread (the
    /// shown one; looked up by id so an edit is still saved while the
    /// selection moves off it) and its index in [`pinned_forms`].
    fn form_target(&self) -> Option<(Thread, usize)> {
        let e = self.inbox.form.as_ref()?;
        let t = self.inbox.threads().iter().find(|t| t.id == e.thread)?;
        let i = pinned_forms(t).iter().position(|p| p.message == e.message)?;
        Some((t.clone(), i))
    }

    /// Focus the selected thread's pinned forms at `spot` (default: the
    /// newest form's first question). Whether the thread has an open form
    /// (an archived one's is refused with a status line, focus unchanged).
    pub(super) fn focus_inbox_form(&mut self, spot: Option<FormSpot>) -> bool {
        let Some(t) = self.selected_inbox_owned() else { return false };
        let pinned = pinned_forms(&t);
        if pinned.is_empty() {
            return false;
        }
        if self.refuse_user_op(&t) {
            return true;
        }
        let spot = spot.unwrap_or(FormSpot { form: 0, question: Some(0) });
        let fi = spot.form.min(pinned.len() - 1);
        let current = self
            .inbox
            .form
            .as_ref()
            .filter(|e| e.thread == t.id && e.message == pinned[fi].message)
            .map(|e| (e.question, e.editing.is_some()));
        let qi = spot.question.or(current.map(|(q, _)| q)).unwrap_or(0);
        if current == Some((qi, true)) {
            // A click on the question being edited: keep typing.
            self.inbox.focus = InboxFocus::Form;
            return true;
        }
        self.close_form_edit();
        self.set_form_cursor(&t, fi, qi);
        self.inbox.focus = InboxFocus::Form;
        true
    }

    /// Put the cursor on question `qi` of pinned form `fi`, its option
    /// cursor on the current answer.
    fn set_form_cursor(&mut self, t: &Thread, fi: usize, qi: usize) {
        let pinned = pinned_forms(t);
        let Some(p) = pinned.get(fi) else { return };
        let qi = qi.min(p.form.questions.len().saturating_sub(1));
        let option = p.form.questions.get(qi).map_or(0, |q| option_cursor(q, value_of(q, p.record).as_ref()));
        self.inbox.form = Some(FormEdit {
            thread: t.id,
            message: p.message.to_string(),
            question: qi,
            option,
            editing: None,
            confirming: false,
        });
        self.inbox.forms_follow.set(true);
    }

    /// Leave the form zone for the thread, keeping a text edit in progress.
    fn leave_form(&mut self) {
        self.close_form_edit();
        self.inbox.focus = InboxFocus::Thread;
    }

    /// Drop the form cursor; a text edit in progress is saved first (`esc`
    /// keeps an edit, so leaving any other way must too), unless it no longer
    /// fits the question.
    pub(super) fn close_form_edit(&mut self) {
        if self.inbox.form.as_ref().is_some_and(|e| e.editing.is_some()) {
            let _ = self.commit_text_edit();
        }
        self.inbox.form = None;
    }

    /// After a reload: a cursor on a form that's gone (submitted elsewhere,
    /// withdrawn) leaves the zone, with a status line.
    pub(super) fn check_form_edit(&mut self) {
        if self.inbox.form.is_some() && self.form_target().is_none() {
            self.inbox.form = None;
            if self.inbox.focus == InboxFocus::Form {
                self.inbox.focus = InboxFocus::Thread;
                self.status = Some("form is no longer open".into());
            }
        }
    }

    /// Queue `answer` to question `q` of the cursor's form as a draft, when
    /// it changes anything.
    fn save_answer(&mut self, t: &Thread, fi: usize, q: &Question, answer: Answer) {
        let pinned = pinned_forms(t);
        let Some(p) = pinned.get(fi) else { return };
        if value_of(q, p.record).as_ref() == Some(&answer) {
            return;
        }
        let op = Op::SaveDraft { thread: t.id, message: p.message.to_string(), answers: [(q.id.clone(), answer)].into() };
        self.request_inbox(op);
    }

    /// Save the text edit in progress and close it; `Err` (the edit stays
    /// open) when the text doesn't fit the question.
    fn commit_text_edit(&mut self) -> Result<(), String> {
        let Some((t, fi)) = self.form_target() else {
            self.inbox.form = None;
            return Ok(());
        };
        let Some(e) = self.inbox.form.as_ref() else { return Ok(()) };
        let Some(text) = e.editing.as_ref().map(|a| a.input().to_string()) else { return Ok(()) };
        let pinned = pinned_forms(&t);
        let Some(q) = pinned[fi].form.questions.get(e.question).cloned() else { return Ok(()) };
        let answer = Answer::Text(text);
        form::check_answer(&q, &answer)?;
        if let Some(e) = self.inbox.form.as_mut() {
            e.editing = None;
        }
        self.save_answer(&t, fi, &q, answer);
        Ok(())
    }

    /// Keys while the form zone has focus (docs/inbox-redesign.md, *Form
    /// editing*): `tab`/`S-tab` step through every pinned form's questions,
    /// `↑`/`↓` (and `←`/`→`) the options, `space` picks (a text question:
    /// edits), `e` edits a text question in the composer box, `enter` is
    /// Confirm then submit, `esc` cancels Confirm or goes back to the
    /// thread, `r`/`i` the reply input. `q`, `:` and `?` stay reachable.
    pub(super) fn on_key_form(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some((t, fi)) = self.form_target() else {
            self.inbox.form = None;
            self.inbox.focus = InboxFocus::Thread;
            self.status = Some("form is no longer open".into());
            return;
        };
        if self.inbox.form.as_ref().is_some_and(|e| e.editing.is_some()) {
            self.on_key_form_text(key);
            return;
        }
        self.status = None;
        let pinned = pinned_forms(&t);
        let p = &pinned[fi];
        let Some(e) = self.inbox.form.as_ref() else { return };
        let Some(q) = p.form.questions.get(e.question).cloned() else { return };
        let (option, confirming) = (e.option, e.confirming);
        let value = value_of(&q, p.record);
        let (form, record) = (p.form.clone(), p.record.clone());
        drop(pinned);
        match key.code {
            KeyCode::Esc if confirming => self.form_edit().confirming = false,
            KeyCode::Esc => self.leave_form(),
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('c') if ctrl => self.should_quit = true,
            KeyCode::Char(':') => self.open_prompt(),
            KeyCode::Char('?') => self.open_help(),
            KeyCode::Tab => self.step_question(&t, 1),
            KeyCode::BackTab => self.step_question(&t, -1),
            KeyCode::Up | KeyCode::Left => {
                let e = self.form_edit();
                e.option = option.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Right => {
                let max = option_count(&q).saturating_sub(1);
                self.form_edit().option = (option + 1).min(max);
            }
            KeyCode::Char(' ') | KeyCode::Char('e') if matches!(q.kind, QuestionKind::Text { .. }) => {
                let text = match &value {
                    Some(Answer::Text(t)) => t.clone(),
                    _ => String::new(),
                };
                let e = self.form_edit();
                e.editing = Some(TextArea::with_text(&text));
                e.confirming = false;
            }
            KeyCode::Char('e') => self.status = Some("e edits a text question".into()),
            KeyCode::Char(' ') => {
                if let Some(answer) = pick(&q, value.as_ref(), option) {
                    self.form_edit().confirming = false;
                    self.save_answer(&t, fi, &q, answer);
                }
            }
            KeyCode::Enter if !confirming => self.form_edit().confirming = true,
            KeyCode::Enter => match confirm(&form, &record) {
                Confirm::Ready(answers) => {
                    let message = self.form_edit().message.clone();
                    self.request_inbox(Op::Submit { thread: t.id, message, answers });
                    self.inbox.form = None;
                    self.inbox.focus = InboxFocus::Thread;
                    self.status = Some(format!("submitted to {}", t.owner_name));
                }
                Confirm::Closed => {
                    self.inbox.form = None;
                    self.inbox.focus = InboxFocus::Thread;
                    self.status = Some("form is no longer open".into());
                }
                // The summary already names what's missing.
                Confirm::Missing(_) | Confirm::Invalid(_) => {}
            },
            KeyCode::Char('r' | 'i') => {
                self.close_form_edit();
                self.inbox.focus = InboxFocus::Thread;
                self.open_reply(&t);
            }
            _ => {}
        }
    }

    fn form_edit(&mut self) -> &mut FormEdit {
        self.inbox.form.as_mut().expect("the form zone has a cursor")
    }

    /// `tab`/`S-tab`: the next/previous question over all pinned forms,
    /// newest form first, wrapping.
    fn step_question(&mut self, t: &Thread, step: isize) {
        let pinned = pinned_forms(t);
        let all: Vec<(usize, usize)> =
            pinned.iter().enumerate().flat_map(|(f, p)| (0..p.form.questions.len()).map(move |q| (f, q))).collect();
        let Some(e) = self.inbox.form.as_ref() else { return };
        let Some(fi) = pinned.iter().position(|p| p.message == e.message) else { return };
        let Some(pos) = all.iter().position(|&x| x == (fi, e.question)) else { return };
        let (f, q) = all[(pos as isize + step).rem_euclid(all.len() as isize) as usize];
        self.set_form_cursor(t, f, q);
    }

    /// Keys while a text question is being edited: `enter` and `esc` both
    /// keep the edit (saved as the draft; refused with a status line when
    /// it's too long), `alt-enter` / `shift-enter` insert a newline on a
    /// multi-line question; the rest edits as in the reply input.
    fn on_key_form_text(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let newline = key.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::SHIFT);
        let multiline = self.editing_question().is_some_and(|q| matches!(q.kind, QuestionKind::Text { multiline: true, .. }));
        match key.code {
            KeyCode::Enter if newline => {
                if multiline {
                    if let Some(a) = self.form_edit().editing.as_mut() {
                        a.newline();
                    }
                }
            }
            KeyCode::Enter | KeyCode::Esc => self.finish_text_edit(),
            KeyCode::Char('c') if ctrl => self.finish_text_edit(),
            _ => {
                if let Some(a) = self.form_edit().editing.as_mut() {
                    edit_text(a, key);
                }
            }
        }
    }

    fn finish_text_edit(&mut self) {
        match self.commit_text_edit() {
            Ok(()) => self.status = None,
            Err(why) => self.status = Some(why),
        }
    }

    /// The text question being edited, for the composer box's title and
    /// placeholder.
    pub fn editing_question(&self) -> Option<Question> {
        let e = self.inbox.form.as_ref().filter(|e| e.editing.is_some())?;
        let (t, fi) = self.form_target()?;
        pinned_forms(&t)[fi].form.questions.get(e.question).cloned()
    }

    /// The text edit in progress, while the form zone has the keys.
    pub fn form_text_edit(&self) -> Option<&TextArea> {
        self.inbox.form.as_ref().filter(|_| self.inbox.focus == InboxFocus::Form)?.editing.as_ref()
    }

    /// The cursor to draw on pinned form `message` of thread `thread`.
    pub fn form_cursor(&self, thread: u64, message: &str) -> Option<FormCursor<'_>> {
        let e = self.inbox.form.as_ref().filter(|e| {
            self.inbox.focus == InboxFocus::Form && e.thread == thread && e.message == message
        })?;
        Some(FormCursor {
            question: e.question,
            option: e.option,
            editing: e.editing.as_ref().map(TextArea::input),
            confirming: e.confirming,
        })
    }
}

/// First row of the forms box to show so the focused question's rows
/// (`focus`) are in view in `height` rows, moving `prev` as little as
/// possible; only when `follow` (the cursor moved), so the wheel can scroll
/// away from it. Clamped to the content either way.
pub fn forms_offset(prev: usize, focus: Option<Range<usize>>, total: usize, height: usize, follow: bool) -> usize {
    let mut off = prev.min(total.saturating_sub(height));
    if let (true, Some(r)) = (follow, focus) {
        if r.start < off {
            off = r.start;
        } else if r.end > off + height {
            off = (r.end - height).min(r.start);
        }
    }
    off
}

/// A pinned form's title: the form's, else `Form`.
pub fn form_title(f: &Form) -> &str {
    f.title.as_deref().filter(|t| !t.trim().is_empty()).unwrap_or("Form")
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use crate::inbox::form::{ChoiceDefault, ChoiceOption};
    use crate::inbox::{feed, Block, Compose, FeedItem, FormState, Inbox};

    fn form_of<'a>(t: &'a Thread, id: &str) -> Option<(&'a Form, &'a FormRecord)> {
        feed::form_of(&t.feed, id)
    }

    fn q(id: &str, label: &str, required: bool, kind: QuestionKind) -> Question {
        Question { id: id.into(), label: label.into(), context: None, required, kind }
    }

    fn opt(id: &str, label: &str) -> ChoiceOption {
        ChoiceOption { id: id.into(), label: label.into(), description: None }
    }

    fn choice(multiple: bool, default: Option<ChoiceDefault>) -> QuestionKind {
        QuestionKind::Choice { options: vec![opt("post", "Post the reply"), opt("skip", "Don't reply")], multiple, default }
    }

    fn text(default: Option<&str>, multiline: bool) -> QuestionKind {
        QuestionKind::Text { placeholder: Some("Reply to post".into()), default: default.map(str::to_string), multiline, max: 20 }
    }

    /// The plan's example, plus a confirm: 1 choice (default post), 2 text
    /// (default, multiline), 3 optional text, 4 required confirm, no default.
    fn drafts() -> Form {
        let mut first = q("c-1", "greptile on `src/cost.ts:42`", true, choice(false, Some(ChoiceDefault::One("post".into()))));
        first.context = Some("> Consider batching these writes.".into());
        Form {
            id: "drafts".into(),
            title: Some("Replies to post".into()),
            submit: "Post replies".into(),
            questions: vec![
                first,
                q("c-1-text", "Reply", false, text(Some("Already batched"), true)),
                q("notes", "Anything else?", false, text(None, true)),
                q("merge", "Merge after?", true, QuestionKind::Confirm { yes: Some("Merge".into()), no: None, default: None }),
            ],
        }
    }

    fn texts(lines: &[PaneLine]) -> Vec<String> {
        lines.iter().map(|l| l.iter().map(|(_, s)| s.as_str()).collect()).collect()
    }

    fn cursor(question: usize, option: usize) -> FormCursor<'static> {
        FormCursor { question, option, editing: None, confirming: false }
    }

    #[test]
    fn form_lines_render_each_widget_and_the_required_marker() {
        let form = drafts();
        let record = FormRecord::open("drafts");
        let out = form_lines(&form, &record, None, 40);
        assert_eq!(texts(&out.lines), [
            "  1/4 greptile on `src/cost.ts:42`",
            "    (•) Post the reply   ( ) Don't reply",
            "  2/4 Reply",
            "    Already batched",
            "  3/4 Anything else?",
            "    Reply to post",
            "  4/4 Merge after? *",
            "    ( ) Merge   ( ) No",
            "[Tab] next  [Space] pick  [e] edit  [Enter] Confirm",
        ]);
        assert_eq!(out.questions, [0..2, 2..4, 4..6, 6..8]);
        // The placeholder is dim, the marker yellow, labels inline markdown.
        assert_eq!(out.lines[5][1], (Tone::Dim, "Reply to post".into()));
        assert_eq!(out.lines[6][3], (Tone::State(State::NeedsYou), " *".into()));
        assert_eq!(out.lines[0][2].0, Tone::Text);

        // Several picks: checkboxes; a draft answer beats the default.
        let mut multi = drafts();
        multi.questions[0].kind = choice(true, None);
        let mut record = FormRecord::open("drafts");
        record.draft.insert("c-1".into(), Answer::Choices(vec!["skip".into()]));
        record.draft.insert("merge".into(), Answer::Confirm(false));
        let out = form_lines(&multi, &record, None, 40);
        assert_eq!(texts(&out.lines)[1], "    [ ] Post the reply   [x] Don't reply");
        assert_eq!(texts(&out.lines)[6], "  4/4 Merge after?", "answered: no marker");
        assert_eq!(texts(&out.lines)[7], "    ( ) Merge   (•) No");
    }

    #[test]
    fn the_focused_question_shows_its_context_and_highlights_the_option() {
        let form = drafts();
        let record = FormRecord::open("drafts");
        let out = form_lines(&form, &record, Some(&cursor(0, 1)), 40);
        let t = texts(&out.lines);
        assert_eq!(t[0], "› 1/4 greptile on `src/cost.ts:42`");
        assert_eq!(out.lines[1], [(Tone::Markdown, "> Consider batching these writes.".to_string())]);
        assert_eq!(t[2], "    (•) Post the reply   ( ) Don't reply");
        assert_eq!(out.lines[2].last(), Some(&(Tone::Cursor, "( ) Don't reply".to_string())));
        assert_eq!(out.questions[0], 0..3);
        // Unfocused, the context is left out.
        let out = form_lines(&form, &record, Some(&cursor(1, 0)), 40);
        assert!(!texts(&out.lines).iter().any(|l| l.contains("Consider")));
    }

    #[test]
    fn text_answers_are_cut_and_multi_line_ones_show_their_first_lines() {
        let form = drafts();
        let mut record = FormRecord::open("drafts");
        record.draft.insert("c-1-text".into(), Answer::Text("a much longer reply than fits here".into()));
        record.draft.insert("notes".into(), Answer::Text("1\n2\n3\n4\n5".into()));
        let t = texts(&form_lines(&form, &record, None, 20).lines);
        assert_eq!(t[3], "    a much longer r…");
        assert_eq!(t[5..9], ["    1", "    2", "    3", "    … 2 more lines"]);
        // Being edited: the live text, marked.
        let c = FormCursor { editing: Some("typed"), ..cursor(1, 0) };
        let t = texts(&form_lines(&form, &record, Some(&c), 20).lines);
        assert_eq!(t[3], "    ✎ typed");
        assert_eq!(t.last().unwrap(), "[Enter] keep  [Esc] keep  [Alt-Enter] newline");
    }

    #[test]
    fn confirming_shows_the_summary_or_what_is_missing() {
        let form = drafts();
        let mut record = FormRecord::open("drafts");
        let c = FormCursor { confirming: true, ..cursor(0, 0) };
        let t = texts(&form_lines(&form, &record, Some(&c), 40).lines);
        assert_eq!(t[t.len() - 2..], ["missing: Merge after?", "[Esc] back"]);
        record.draft.insert("merge".into(), Answer::Confirm(true));
        let t = texts(&form_lines(&form, &record, Some(&c), 40).lines);
        // The empty optional text is sent, but isn't counted.
        assert_eq!(t[t.len() - 2..], ["Post replies: submit 3 answers?", "[Enter] yes  [Esc] no"]);
        record.state = FormState::Withdrawn;
        assert_eq!(confirm(&form, &record), Confirm::Closed);
    }

    #[test]
    fn submitted_answers_render_read_only() {
        let form = drafts();
        let answers: BTreeMap<String, Answer> = [
            ("c-1".to_string(), Answer::Choice("skip".into())),
            ("c-1-text".to_string(), Answer::Text("line one\n\nline two".into())),
            ("notes".to_string(), Answer::Text(String::new())),
            ("merge".to_string(), Answer::Confirm(true)),
        ]
        .into();
        let rows = answer_rows(&form, &answers);
        assert_eq!(texts(&rows), [
            "greptile on `src/cost.ts:42`: Don't reply",
            "Reply:",
            "line one\n\nline two",
            "Anything else?: –",
            "Merge after?: Merge",
        ]);
        assert_eq!(rows[2], [(Tone::Markdown, "line one\n\nline two".to_string())]);
        let mut multi = q("m", "Which", true, choice(true, None));
        assert_eq!(answer_text(&multi, Some(&Answer::Choices(vec!["post".into(), "skip".into()]))), "Post the reply, Don't reply");
        assert_eq!(answer_text(&multi, Some(&Answer::Choices(vec![]))), "none");
        multi.kind = QuestionKind::Confirm { yes: None, no: None, default: None };
        assert_eq!(answer_text(&multi, Some(&Answer::Confirm(false))), "No");
    }

    #[test]
    fn pick_picks_toggles_and_confirms() {
        let single = q("s", "S", true, choice(false, None));
        assert_eq!(pick(&single, None, 1), Some(Answer::Choice("skip".into())));
        let multi = q("m", "M", true, choice(true, None));
        let a = pick(&multi, None, 1).unwrap();
        assert_eq!(a, Answer::Choices(vec!["skip".into()]));
        let b = pick(&multi, Some(&a), 0).unwrap();
        assert_eq!(b, Answer::Choices(vec!["post".into(), "skip".into()]), "options' order");
        assert_eq!(pick(&multi, Some(&b), 1), Some(Answer::Choices(vec!["post".into()])));
        let yn = q("c", "C", true, QuestionKind::Confirm { yes: None, no: None, default: None });
        assert_eq!(pick(&yn, None, 1), Some(Answer::Confirm(false)));
        assert_eq!(pick(&q("t", "T", false, text(None, false)), None, 0), None);
    }

    #[test]
    fn forms_offset_follows_the_focus_only_when_asked() {
        // 20 rows in 5: a focus below the window pulls it down to show it.
        assert_eq!(forms_offset(0, Some(8..10), 20, 5, true), 5);
        assert_eq!(forms_offset(9, Some(2..4), 20, 5, true), 2);
        // Taller than the window: its start shows.
        assert_eq!(forms_offset(0, Some(8..16), 20, 5, true), 8);
        // Not following (the wheel): stays, clamped.
        assert_eq!(forms_offset(3, Some(8..10), 20, 5, false), 3);
        assert_eq!(forms_offset(30, None, 20, 5, false), 15);
    }

    /// `asks` from `d` taking replies, with message `run-1` carrying
    /// [`drafts`] (older) and `run-2` a one-question form (newer), its
    /// thread focused and read.
    fn form_app() -> (App, u64) {
        let mut app = new_app();
        let mut inbox = Inbox::default();
        let put = crate::inbox::ThreadPut {
            key: "asks".into(),
            title: "asks".into(),
            state: State::NeedsYou,
            compose: Some(Compose::default()),
            ..Default::default()
        };
        inbox.put("d-id", "d", 10, put);
        let t = &mut inbox.threads[0];
        let id = t.id;
        let small = Form {
            id: "ok".into(),
            title: None,
            submit: "Submit".into(),
            questions: vec![q("go", "Go?", true, QuestionKind::Confirm { yes: None, no: None, default: Some(true) })],
        };
        for (seq, (msg, form)) in [("run-1", drafts()), ("run-2", small)].into_iter().enumerate() {
            let record = FormRecord::open(&form.id);
            let blocks = vec![Block::Markdown { text: format!("{msg} text") }, Block::Form(form)];
            let kind = ItemKind::Message { id: msg.into(), blocks, edited: false, withdrawn: false, form: Some(record) };
            t.feed.push(FeedItem { seq: 100 + seq as u64, at: 20 + seq as u64, kind });
        }
        app.set_inbox(inbox);
        app.on_key(key(KeyCode::Char('4')));
        app.on_key(key(KeyCode::Enter));
        flush(&mut app);
        (app, id)
    }

    fn flush(app: &mut App) -> Vec<Op> {
        let ops = app.take_pending_inbox();
        let mut inbox = app.inbox.content_clone();
        ops.iter().for_each(|op| inbox.apply(op, 50, "tui"));
        app.set_inbox(inbox);
        ops
    }

    fn at(app: &App) -> (String, usize, usize) {
        let e = app.inbox.form.as_ref().expect("a form cursor");
        (e.message.clone(), e.question, e.option)
    }

    fn record<'a>(app: &'a App, msg: &str) -> &'a FormRecord {
        form_of(app.selected_inbox_thread().unwrap(), msg).unwrap().1
    }

    #[test]
    fn pinned_forms_are_the_open_ones_newest_first() {
        let (mut app, _) = form_app();
        let t = app.selected_inbox_thread().unwrap();
        assert_eq!(pinned_forms(t).iter().map(|p| p.message).collect::<Vec<_>>(), ["run-2", "run-1"]);
        let mut inbox = app.inbox.content_clone();
        feed::form_record_mut(&mut inbox.threads[0].feed, "run-2").unwrap().withdraw();
        app.set_inbox(inbox);
        let t = app.selected_inbox_thread().unwrap();
        assert_eq!(pinned_forms(t).iter().map(|p| p.message).collect::<Vec<_>>(), ["run-1"]);
    }

    #[test]
    fn tab_in_the_thread_enters_the_form_and_tab_cycles_its_questions() {
        let (mut app, _) = form_app();
        assert_eq!(app.inbox.focus, InboxFocus::Thread);
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.inbox.focus, InboxFocus::Form);
        // The newest form first; its confirm defaults to yes.
        assert_eq!(at(&app), ("run-2".into(), 0, 0));
        app.on_key(key(KeyCode::Tab));
        assert_eq!(at(&app), ("run-1".into(), 0, 0), "on to the older form");
        for want in [1, 2, 3] {
            app.on_key(key(KeyCode::Tab));
            assert_eq!(at(&app).1, want);
        }
        app.on_key(key(KeyCode::Tab));
        assert_eq!(at(&app), ("run-2".into(), 0, 0), "wraps");
        app.on_key(key(KeyCode::BackTab));
        assert_eq!(at(&app), ("run-1".into(), 3, 0));
        // Esc: back to the thread; Tab there again starts at the top.
        app.on_key(key(KeyCode::Esc));
        assert_eq!((app.inbox.focus, app.inbox.form.is_none()), (InboxFocus::Thread, true));
        // `r` from the form: the composer.
        app.on_key(key(KeyCode::Tab));
        app.on_key(key(KeyCode::Char('r')));
        assert_eq!(app.inbox.focus, InboxFocus::Input);
        assert_eq!(app.take_pending_inbox(), []);
    }

    #[test]
    fn without_an_open_form_tab_goes_to_the_composer() {
        let (mut app, _) = form_app();
        let mut inbox = app.inbox.content_clone();
        for m in ["run-1", "run-2"] {
            feed::form_record_mut(&mut inbox.threads[0].feed, m).unwrap().withdraw();
        }
        app.set_inbox(inbox);
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.inbox.focus, InboxFocus::Input);
    }

    #[test]
    fn space_picks_and_saves_the_draft() {
        let (mut app, id) = form_app();
        app.on_key(key(KeyCode::Tab));
        app.on_key(key(KeyCode::Tab));
        // Single choice: move to `skip`, pick it.
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Down));
        assert_eq!(at(&app).2, 1, "clamped to the last option");
        app.on_key(key(KeyCode::Char(' ')));
        let save = |q: &str, a: Answer| Op::SaveDraft { thread: id, message: "run-1".into(), answers: [(q.to_string(), a)].into() };
        // Applied at once, before the store reload.
        assert_eq!(record(&app, "run-1").draft.get("c-1"), Some(&Answer::Choice("skip".into())));
        assert_eq!(flush(&mut app), [save("c-1", Answer::Choice("skip".into()))]);
        // Re-picking the same is no change.
        app.on_key(key(KeyCode::Char(' ')));
        assert_eq!(flush(&mut app), []);
        // Confirm: `no` with a custom-label yes.
        for _ in 0..3 {
            app.on_key(key(KeyCode::Tab));
        }
        app.on_key(key(KeyCode::Right));
        app.on_key(key(KeyCode::Char(' ')));
        assert_eq!(flush(&mut app), [save("merge", Answer::Confirm(false))]);
        // `e` on a choice is only a hint.
        app.on_key(key(KeyCode::Tab));
        app.on_key(key(KeyCode::Tab));
        app.on_key(key(KeyCode::Char('e')));
        assert_eq!(app.status.as_deref(), Some("e edits a text question"));
    }

    #[test]
    fn space_toggles_a_multiple_choice() {
        let (mut app, id) = form_app();
        let mut inbox = app.inbox.content_clone();
        for item in &mut inbox.threads[0].feed {
            if let ItemKind::Message { id, blocks, .. } = &mut item.kind {
                if let ("run-1", Block::Form(f)) = (id.as_str(), &mut blocks[1]) {
                    f.questions[0].kind = choice(true, None);
                }
            }
        }
        app.set_inbox(inbox);
        app.on_key(key(KeyCode::Tab));
        app.on_key(key(KeyCode::Tab));
        app.on_key(key(KeyCode::Char(' ')));
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Char(' ')));
        app.on_key(key(KeyCode::Up));
        app.on_key(key(KeyCode::Char(' ')));
        let ops = flush(&mut app);
        let picks: Vec<&Answer> = ops
            .iter()
            .map(|op| match op {
                Op::SaveDraft { thread, answers, .. } if *thread == id => &answers["c-1"],
                op => panic!("{op:?}"),
            })
            .collect();
        assert_eq!(picks, [
            &Answer::Choices(vec!["post".into()]),
            &Answer::Choices(vec!["post".into(), "skip".into()]),
            &Answer::Choices(vec!["skip".into()]),
        ]);
    }

    #[test]
    fn e_edits_a_text_question_and_enter_or_esc_keeps_it() {
        let (mut app, id) = form_app();
        app.on_key(key(KeyCode::Tab));
        app.on_key(key(KeyCode::Tab));
        app.on_key(key(KeyCode::Tab));
        assert_eq!(at(&app), ("run-1".into(), 1, 0));
        app.on_key(key(KeyCode::Char('e')));
        assert_eq!(app.form_text_edit().map(TextArea::input), Some("Already batched"), "opened on the default");
        // Keys are text now: `q` doesn't quit, tab doesn't move.
        for c in " q".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
        app.on_key(key(KeyCode::Char('x')));
        assert!(!app.should_quit);
        assert_eq!(flush(&mut app), [], "no save per keystroke");
        app.on_key(key(KeyCode::Enter));
        assert!(app.form_text_edit().is_none());
        assert_eq!(app.inbox.focus, InboxFocus::Form, "still in the form");
        let text = Answer::Text("Already batched q\nx".into());
        assert_eq!(flush(&mut app), [Op::SaveDraft {
            thread: id,
            message: "run-1".into(),
            answers: [("c-1-text".to_string(), text.clone())].into()
        }]);
        assert_eq!(record(&app, "run-1").draft["c-1-text"], text);

        // Esc keeps too; the edit reopens on the saved text.
        app.on_key(key(KeyCode::Char('e')));
        assert_eq!(app.form_text_edit().map(TextArea::input), Some("Already batched q\nx"));
        app.on_key(key(KeyCode::Backspace));
        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.inbox.focus, InboxFocus::Form);
        assert_eq!(record(&app, "run-1").draft["c-1-text"], Answer::Text("Already batched q\n".into()));
        flush(&mut app);

        // Too long (max 20): refused, the edit stays open.
        app.on_key(key(KeyCode::Char('e')));
        for _ in 0..5 {
            app.on_key(key(KeyCode::Char('y')));
        }
        app.on_key(key(KeyCode::Enter));
        assert!(app.form_text_edit().is_some());
        assert_eq!(app.status.as_deref(), Some("`c-1-text`: longer than 20 bytes"));
        // Leaving another way still keeps a fitting edit.
        app.on_key(key(KeyCode::Backspace));
        app.on_key(key(KeyCode::Backspace));
        app.on_key(key(KeyCode::Backspace));
        app.close_form_edit();
        assert_eq!(record(&app, "run-1").draft["c-1-text"], Answer::Text("Already batched q\nyy".into()));
    }

    #[test]
    fn enter_confirms_then_submits_with_the_defaults_resolved() {
        let (mut app, id) = form_app();
        app.on_key(key(KeyCode::Tab));
        app.on_key(key(KeyCode::Tab));
        // `merge` is required and unanswered: the summary says so, a second
        // enter does nothing.
        app.on_key(key(KeyCode::Enter));
        assert!(app.inbox.form.as_ref().unwrap().confirming);
        app.on_key(key(KeyCode::Enter));
        assert_eq!((app.inbox.focus, app.take_pending_inbox()), (InboxFocus::Form, vec![]));
        let t = app.selected_inbox_thread().unwrap();
        let (form, rec) = form_of(t, "run-1").unwrap();
        assert_eq!(confirm(form, rec), Confirm::Missing(vec!["Merge after?".into()]));
        // Esc cancels the confirm step only.
        app.on_key(key(KeyCode::Esc));
        assert!(!app.inbox.form.as_ref().unwrap().confirming);
        assert_eq!(app.inbox.focus, InboxFocus::Form);
        // Answer it, confirm, submit.
        app.on_key(key(KeyCode::BackTab));
        app.on_key(key(KeyCode::BackTab));
        assert_eq!(at(&app).1, 3);
        app.on_key(key(KeyCode::Char(' ')));
        app.on_key(key(KeyCode::Enter));
        app.on_key(key(KeyCode::Enter));
        let ops = app.take_pending_inbox();
        let answers: BTreeMap<String, Answer> = [
            ("c-1".to_string(), Answer::Choice("post".into())),
            ("c-1-text".to_string(), Answer::Text("Already batched".into())),
            ("notes".to_string(), Answer::Text(String::new())),
            ("merge".to_string(), Answer::Confirm(true)),
        ]
        .into();
        assert_eq!(ops.last(), Some(&Op::Submit { thread: id, message: "run-1".into(), answers }));
        assert_eq!(app.inbox.focus, InboxFocus::Thread);
        assert_eq!(app.status.as_deref(), Some("submitted to d"));
        // The store applies it: the form leaves the pins, the feed shows the
        // answers under the message and a folded submission.
        let mut inbox = app.inbox.content_clone();
        ops.iter().for_each(|op| inbox.apply(op, 60, "tui"));
        app.set_inbox(inbox);
        let t = app.selected_inbox_thread().unwrap();
        assert_eq!(pinned_forms(t).len(), 1);
        let feed: Vec<String> = texts(&super::super::pane_feed(t, 0));
        assert!(feed.iter().any(|l| l.ends_with("  you answered 3 questions")), "{feed:#?}");
        assert!(feed.iter().any(|l| l == "Merge after?: Merge"), "{feed:#?}");
        assert!(feed.iter().any(|l| l.starts_with("form: Replies to post · submitted ")), "{feed:#?}");
        assert!(feed.iter().any(|l| l == "form: Form · open, pinned above"), "{feed:#?}");
    }

    #[test]
    fn a_form_closed_elsewhere_leaves_the_zone() {
        let (mut app, _) = form_app();
        app.on_key(key(KeyCode::Tab));
        let mut inbox = app.inbox.content_clone();
        feed::form_record_mut(&mut inbox.threads[0].feed, "run-2").unwrap().withdraw();
        app.set_inbox(inbox);
        assert_eq!(app.inbox.focus, InboxFocus::Thread);
        assert_eq!(app.status.as_deref(), Some("form is no longer open"));
    }

    #[test]
    fn archived_threads_refuse_the_form() {
        let (mut app, _) = form_app();
        let mut inbox = app.inbox.content_clone();
        inbox.threads[0].archived = true;
        app.set_inbox(inbox);
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.inbox.focus, InboxFocus::Thread);
        assert!(app.status.as_deref().is_some_and(|s| s.starts_with("archived")));
    }
}
