//! `devsandbox inbox …`: the Inbox from a shell (docs/inbox-cli.md).
//!
//! Reads (`ls`, `show`) go straight to the store, so they work without the
//! daemon and never start it. Mutations go to a daemon already running
//! (no lazy start), so subscribers and the owner's follower wake at once;
//! when none answers they apply the same `Op`s locally through the same
//! checks (`inbox::wire::decide_*`), so the events and errors are the same
//! either way. Output shapes are the API's views (`inbox::wire`).

use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use serde_json::Value;

use crate::commands::status::Envelope;
use crate::inbox::wire::{self, Addr, ApiError, BlockView, FeedItemView, QuestionView, ThreadDetail, ThreadSummary};
use crate::inbox::{Inbox, Op, View, ops, store};
use crate::render::table;
use crate::state::State;

/// A `<thread>` argument: `<owner>/<key>` (owner = instance name or id) or
/// the store id `ls` shows (the only handle a notify thread has).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadArg {
    Id(u64),
    Key { owner: String, key: String },
}

impl ThreadArg {
    pub fn parse(s: &str) -> Result<Self, String> {
        if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
            return s.parse().map(ThreadArg::Id).map_err(|e| format!("bad thread id `{s}`: {e}"));
        }
        match s.split_once('/') {
            Some((owner, key)) if !owner.is_empty() && !key.is_empty() => {
                Ok(ThreadArg::Key { owner: owner.into(), key: key.into() })
            }
            _ => Err(format!("expected <owner>/<key> or a thread id (see `devsandbox inbox ls`), got `{s}`")),
        }
    }
}

/// The store id of the thread `arg` names. An owner is resolved through
/// `state` like every instance argument (to its `instance_id`); when that
/// finds no thread (no such instance, or a removed one whose threads are
/// archived), the API's own lookup by id or last-known name decides.
fn locate(inbox: &Inbox, state: Option<&State>, arg: &ThreadArg) -> Result<u64, ApiError> {
    let (owner, key) = match arg {
        ThreadArg::Id(id) => return wire::resolve(inbox, &Addr::id(*id)).map(|t| t.id),
        ThreadArg::Key { owner, key } => (owner, key),
    };
    let by = |owner: &str| Addr { owner: Some(owner.into()), key: Some(key.clone()), ..Addr::default() };
    let instance_id = state.and_then(|s| {
        let k = super::resolve_instance_noninteractive(s, owner).ok()?;
        s.instances.get(&k).map(|i| i.instance_id.clone())
    });
    if let Some(id) = instance_id
        && let Ok(t) = wire::resolve(inbox, &by(&id))
    {
        return Ok(t.id);
    }
    wire::resolve(inbox, &by(owner)).map(|t| t.id)
}

/// An API error as the CLI reports it, the same from either path.
fn api_error(code: &str, message: &str) -> anyhow::Error {
    anyhow!("{message} ({code})")
}

fn store_path() -> Result<std::path::PathBuf> {
    store::path().context("cannot locate the Inbox store")
}

/// The saved state for owner names; unreadable state only loses the
/// name -> id step (the archived-name fallback still works).
fn state() -> Option<State> {
    State::load().ok()
}

// ---- reads ----------------------------------------------------------------

pub fn ls(view: &str, json: bool) -> Result<()> {
    let view = View::parse(view).ok_or_else(|| anyhow!("unknown view `{view}`"))?;
    let rows = wire::summaries(&ops::load(&store_path()?)?, view);
    if json {
        println!("{}", serde_json::to_string_pretty(&Envelope::new(rows)).context("serialize inbox ls")?);
        return Ok(());
    }
    print!("{}", ls_table(&rows, now(), crate::tui::local_utc_offset()));
    Ok(())
}

pub fn show(arg: &ThreadArg, json: bool) -> Result<()> {
    let inbox = ops::load(&store_path()?)?;
    let id = locate(&inbox, state().as_ref(), arg).map_err(|e| api_error(e.code, &e.message))?;
    let t = inbox.threads.iter().find(|t| t.id == id).expect("located in this inbox");
    let detail = ThreadDetail::of(t);
    if json {
        println!("{}", serde_json::to_string_pretty(&Envelope::new(detail)).context("serialize inbox show")?);
        return Ok(());
    }
    print!("{}", render_detail(&detail, now(), crate::tui::local_utc_offset()));
    Ok(())
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

const LS_HEADERS: [&str; 6] = ["ID", "STATE", "OWNER", "KEY", "TITLE", "AGE"];
/// Longest TITLE cell, in chars.
const TITLE_WIDTH: usize = 50;

/// What the dashboard's chip says: the state (`needs you`, `active`,
/// `done`), a notify thread's level; `(archived)` once its owner is gone.
fn chip(t: &ThreadSummary) -> String {
    let base = match t.state {
        Some("needs-you") => "needs you",
        Some(s) => s,
        None => t.level.unwrap_or("-"),
    };
    if t.archived { format!("{base} (archived)") } else { base.to_string() }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

fn ls_table(rows: &[ThreadSummary], now: u64, utc_offset: i64) -> String {
    let cells: Vec<[String; 6]> = rows
        .iter()
        .map(|t| {
            [
                t.id.to_string(),
                chip(t),
                t.owner_name.clone(),
                t.key.clone().unwrap_or_else(|| "-".into()),
                truncate(&t.title, TITLE_WIDTH),
                crate::tui::short_age(t.changed_at, now, utc_offset),
            ]
        })
        .collect();
    table(&LS_HEADERS, &cells)
}

/// The thread as `show` prints it: the header (title, state, address,
/// child, link, actions, how to reply), then the feed newest first, like
/// the dashboard's pane.
fn render_detail(d: &ThreadDetail, now: u64, utc_offset: i64) -> String {
    let s = &d.summary;
    let mut out = String::new();
    let line = |out: &mut String, text: &str| {
        out.push_str(text);
        out.push('\n');
    };
    line(&mut out, &s.title);
    let state = match &s.status {
        Some(status) => format!("{} · {status}", chip(s)),
        None => chip(s),
    };
    line(&mut out, &state);
    let addr = match &s.key {
        Some(key) => format!("{}/{key}  (thread {})", s.owner_name, s.id),
        None => format!("{}  (thread {})", s.owner_name, s.id),
    };
    line(&mut out, &addr);
    if let Some(child) = &d.child {
        line(&mut out, &format!("child: {child}"));
    }
    if let Some(link) = &d.link {
        line(&mut out, &format!("link: {link}"));
    }
    if !d.actions.is_empty() {
        let actions: Vec<String> = d
            .actions
            .iter()
            .map(|a| {
                let mut tags = Vec::new();
                if a.done {
                    tags.push("sets done");
                }
                if !a.sends_event {
                    tags.push("dashboard only");
                }
                let tags = if tags.is_empty() { String::new() } else { format!(" ({})", tags.join(", ")) };
                format!("[{}] {}{tags}", a.id, a.label)
            })
            .collect();
        line(&mut out, &format!("actions: {}", actions.join("  ")));
    }
    if let Some(c) = &d.compose {
        let hint = c.hint.as_deref().or(c.placeholder.as_deref());
        let how = format!("devsandbox inbox reply {} <text>", s.id);
        line(&mut out, &match hint {
            Some(hint) => format!("reply: {hint}  ({how})"),
            None => format!("reply: {how}"),
        });
    }
    let age = |at: u64| crate::tui::short_age(at, now, utc_offset);
    for n in &d.notes {
        out.push('\n');
        line(&mut out, &format!("{}  {}", age(n.at), n.level));
        indent(&mut out, &n.msg, 4);
        if let Some(link) = &n.link {
            indent(&mut out, &format!("link: {link}"), 4);
        }
    }
    for item in d.feed.iter().rev() {
        out.push('\n');
        render_item(&mut out, item, s, &age);
    }
    out
}

fn indent(out: &mut String, text: &str, by: usize) {
    for l in text.lines() {
        out.extend(std::iter::repeat_n(' ', by));
        out.push_str(l);
        out.push('\n');
    }
}

fn render_item(out: &mut String, item: &FeedItemView, s: &ThreadSummary, age: &dyn Fn(u64) -> String) {
    match item {
        FeedItemView::Message { at, id, blocks, edited, withdrawn, .. } => {
            let mut head = format!("{}  {}  message {id}", age(*at), s.owner_name);
            if *edited {
                head.push_str(" (edited)");
            }
            if *withdrawn {
                head.push_str(" (withdrawn)");
                out.push_str(&head);
                out.push('\n');
                return;
            }
            out.push_str(&head);
            out.push('\n');
            for b in blocks {
                render_block(out, b, s.id, id);
            }
        }
        FeedItemView::Reply { at, text, .. } => {
            out.push_str(&format!("{}  you\n", age(*at)));
            indent(out, text, 4);
        }
        FeedItemView::Action { at, action, label, .. } => {
            out.push_str(&format!("{}  you · {label} [{action}]\n", age(*at)));
        }
        FeedItemView::Submission { at, message, form, answers, .. } => {
            out.push_str(&format!("{}  you · submitted form {form} of message {message}\n", age(*at)));
            if let Value::Object(answers) = answers {
                for (q, v) in answers {
                    indent(out, &format!("{q}: {}", value_text(v)), 4);
                }
            }
        }
        FeedItemView::Marker { at, marker, from, to, .. } => {
            let what = match *marker {
                "done" => "marked done".to_string(),
                "reopen" => "reopened".to_string(),
                other => {
                    let side = |v: &Option<String>| v.clone().unwrap_or_else(|| "-".into());
                    format!("{other} {} → {}", side(from), side(to))
                }
            };
            out.push_str(&format!("{}  · {what}\n", age(*at)));
        }
    }
}

fn render_block(out: &mut String, b: &BlockView, thread: u64, message: &str) {
    match b {
        BlockView::Markdown { text } => indent(out, text, 4),
        BlockView::Fields { items } => {
            for f in items {
                indent(out, &format!("{}: {}", f.label, f.value), 4);
            }
        }
        BlockView::Form { id, title, submit, questions, state, draft, answers } => {
            let name = title.as_deref().unwrap_or(submit);
            indent(out, &format!("form {id}: {name} ({state})"), 4);
            for q in questions {
                render_question(out, q, draft, answers.as_ref());
            }
            if *state == "open" {
                indent(out, &format!("{submit}: devsandbox inbox submit {thread} {message} --json '{{…}}'"), 6);
            }
        }
    }
}

fn render_question(out: &mut String, q: &QuestionView, draft: &Value, answers: Option<&Value>) {
    let (id, label, context, required, default, detail) = match q {
        QuestionView::Choice { id, label, context, required, options, multiple, default } => {
            let opts: Vec<String> = options.iter().map(|o| format!("{} = {}", o.id, o.label)).collect();
            let which = if *multiple { "any of" } else { "one of" };
            (id, label, context, *required, default.clone(), Some(format!("{which}: {}", opts.join(", "))))
        }
        QuestionView::Text { id, label, context, required, default, .. } => {
            (id, label, context, *required, default.clone().map(Value::String), None)
        }
        QuestionView::Confirm { id, label, context, required, yes, no, default } => {
            let detail = match (yes, no) {
                (None, None) => "true | false".to_string(),
                (yes, no) => format!("true = {}, false = {}", yes.as_deref().unwrap_or("yes"), no.as_deref().unwrap_or("no")),
            };
            (id, label, context, *required, default.map(Value::Bool), Some(detail))
        }
    };
    let req = if required { " (required)" } else { "" };
    indent(out, &format!("{id}: {label}{req}"), 6);
    if let Some(context) = context {
        indent(out, &format!("> {}", context.replace('\n', "\n> ")), 8);
    }
    if let Some(detail) = detail {
        indent(out, &detail, 8);
    }
    let value = match answers {
        Some(a) => Some(("answer", a.get(id).cloned().unwrap_or(Value::Null))),
        None => draft.get(id).map(|v| ("draft", v.clone())).or(default.map(|v| ("default", v))),
    };
    if let Some((what, v)) = value {
        indent(out, &format!("{what}: {}", value_text(&v)), 8);
    }
}

/// An answer as text: strings as is, arrays comma-joined, `-` for none.
fn value_text(v: &Value) -> String {
    match v {
        Value::Null => "-".into(),
        Value::String(s) => s.clone(),
        Value::Array(items) => items.iter().map(value_text).collect::<Vec<_>>().join(", "),
        other => other.to_string(),
    }
}

// ---- mutations ------------------------------------------------------------

/// A user op on a thread: one API method, one `wire::decide_*`.
#[derive(Debug, Clone, PartialEq)]
pub enum Mutation {
    Reply(String),
    Act(String),
    Submit { message: String, answers: serde_json::Map<String, Value> },
    Done,
    Reopen,
}

impl Mutation {
    #[cfg_attr(not(unix), allow(dead_code))]
    fn method(&self) -> &'static str {
        match self {
            Mutation::Reply(_) => "inbox.thread.reply",
            Mutation::Act(_) => "inbox.thread.act",
            Mutation::Submit { .. } => "inbox.form.submit",
            Mutation::Done => "inbox.thread.done",
            Mutation::Reopen => "inbox.thread.reopen",
        }
    }

    #[cfg_attr(not(unix), allow(dead_code))]
    fn params(&self, thread: u64) -> Value {
        let mut p = serde_json::json!({ "thread": thread });
        match self {
            Mutation::Reply(text) => p["text"] = text.as_str().into(),
            Mutation::Act(action) => p["action"] = action.as_str().into(),
            Mutation::Submit { message, answers } => {
                p["message"] = message.as_str().into();
                p["answers"] = Value::Object(answers.clone());
            }
            Mutation::Done | Mutation::Reopen => {}
        }
        p
    }

    fn decide(&self, inbox: &Inbox, addr: &Addr) -> Result<Vec<Op>, ApiError> {
        match self {
            Mutation::Reply(text) => wire::decide_reply(inbox, addr, text),
            Mutation::Act(action) => wire::decide_act(inbox, addr, action),
            Mutation::Submit { message, answers } => wire::decide_submit(inbox, addr, message, answers),
            Mutation::Done => wire::decide_done(inbox, addr),
            Mutation::Reopen => wire::decide_reopen(inbox, addr),
        }
    }

    /// The one line on stderr after it landed.
    fn confirmation(&self, thread: u64) -> String {
        match self {
            Mutation::Reply(_) => format!("replied to thread {thread}"),
            Mutation::Act(action) => format!("sent action `{action}` to thread {thread}"),
            Mutation::Submit { message, .. } => format!("submitted the form of message `{message}` on thread {thread}"),
            Mutation::Done => format!("marked thread {thread} done"),
            Mutation::Reopen => format!("reopened thread {thread}"),
        }
    }
}

pub fn mutate(arg: &ThreadArg, m: Mutation) -> Result<()> {
    let path = store_path()?;
    let id = locate(&ops::load(&path)?, state().as_ref(), arg).map_err(|e| api_error(e.code, &e.message))?;
    #[cfg(unix)]
    if let Some(answer) = via_daemon(id, &m)? {
        answer?;
        eprintln!("{}", m.confirmation(id));
        return Ok(());
    }
    apply_local(&path, id, &m)?.map_err(|e| api_error(e.code, &e.message))?;
    eprintln!("{}", m.confirmation(id));
    Ok(())
}

/// The local fallback: the same check and `Op`s the API handler applies,
/// in one locked store write, recorded as the `cli` client's.
fn apply_local(path: &Path, thread: u64, m: &Mutation) -> Result<Result<(), ApiError>> {
    ops::apply_if(path, "cli", |inbox| m.decide(inbox, &Addr::id(thread)))
}

/// `m` through a daemon that's already running; `None` when none answers
/// (or it's handing off), so the caller applies it locally. Once the
/// request went out, a lost connection is an error, never a local retry:
/// the daemon may have applied it.
#[cfg(unix)]
fn via_daemon(thread: u64, m: &Mutation) -> Result<Option<Result<()>>> {
    use crate::serve::{client, endpoint, proto::Version};
    let Ok(dir) = endpoint::socket_dir() else { return Ok(None) };
    let Ok(Some(mut conn)) = client::connect_running(&dir, "cli", &Version::current(), client::START_TIMEOUT) else {
        return Ok(None);
    };
    let r = conn.call(m.method(), m.params(thread))?;
    Ok(Some(match r.error {
        Some(e) => Err(api_error(&e.code, &e.message)),
        None => Ok(()),
    }))
}

/// `reply`'s text: `-` reads all of stdin (one trailing newline dropped).
pub fn reply_text(text: &str) -> Result<String> {
    if text != "-" {
        return Ok(text.to_string());
    }
    let mut s = String::new();
    std::io::stdin().read_to_string(&mut s).context("cannot read the reply from stdin")?;
    Ok(strip_newline(s))
}

fn strip_newline(mut s: String) -> String {
    if s.ends_with('\n') {
        s.pop();
        if s.ends_with('\r') {
            s.pop();
        }
    }
    s
}

/// `submit`'s answers: the `--json` text, else all of stdin.
pub fn submit_answers(json: Option<&str>) -> Result<serde_json::Map<String, Value>> {
    let text = match json {
        Some(j) => j.to_string(),
        None => {
            let mut s = String::new();
            std::io::stdin().read_to_string(&mut s).context("cannot read the answers from stdin")?;
            s
        }
    };
    parse_answers(&text)
}

/// A JSON object of answers by question id; blank input is no answers
/// (the draft and the defaults fill in).
fn parse_answers(text: &str) -> Result<serde_json::Map<String, Value>> {
    if text.trim().is_empty() {
        return Ok(Default::default());
    }
    match serde_json::from_str(text).context("answers aren't valid JSON")? {
        Value::Object(map) => Ok(map),
        other => anyhow::bail!("answers must be a JSON object of question id -> answer, got `{other}`"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devsbd::notify::{Level, Record};
    use crate::inbox::{Action, Block, Compose, MessageSend, State as TState, ThreadPut, form};
    use serde_json::json;

    fn store_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("devsandbox-inbox-cli-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("inbox.json")
    }

    fn put(path: &Path, owner: &str, name: &str, key: &str) {
        let put = ThreadPut {
            key: key.into(),
            title: format!("{key} title"),
            state: TState::NeedsYou,
            status: Some("waiting".into()),
            compose: Some(Compose { placeholder: None, hint: Some("instructions".into()) }),
            actions: vec![
                Action { id: "go".into(), label: "Go".into(), ..Action::default() },
                Action { id: "fin".into(), label: "Finish".into(), done: true, ..Action::default() },
            ],
            ..ThreadPut::default()
        };
        store::update_at(path, |i| i.put(owner, name, 10, put)).unwrap();
    }

    fn state(instances: &[(&str, &str)]) -> State {
        let toml: String = instances
            .iter()
            .map(|(name, id)| {
                format!(
                    "[instance.{name}]\nsandbox = \"web\"\ninstance_id = \"{id}\"\ncontainer = \"c\"\nfolder = \"/f\"\nworkspace = \"/w\"\ncreated_unix = 0\n"
                )
            })
            .collect();
        toml::from_str(&toml).unwrap()
    }

    #[test]
    fn thread_args_parse() {
        assert_eq!(ThreadArg::parse("12"), Ok(ThreadArg::Id(12)));
        assert_eq!(ThreadArg::parse("disp/pr-1"), Ok(ThreadArg::Key { owner: "disp".into(), key: "pr-1".into() }));
        for bad in ["", "disp", "/pr-1", "disp/", "-3", "99999999999999999999999"] {
            assert!(ThreadArg::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn owners_resolve_by_state_then_last_known_name() {
        let path = store_path("locate");
        put(&path, "d-id", "d", "pr-1");
        put(&path, "old-id", "gone", "pr-2");
        store::update_at(&path, |i| i.archive_owner("old-id")).unwrap();
        let inbox = ops::load(&path).unwrap();
        let id_of = |owner: &str, key: &str| ops::thread_id(&inbox, owner, key).unwrap();
        let key = |owner: &str, key: &str| ThreadArg::Key { owner: owner.into(), key: key.into() };
        // The instance's name resolves through state to its id (here a
        // renamed instance: name `renamed`, id `d-id`).
        let st = state(&[("renamed", "d-id")]);
        assert_eq!(locate(&inbox, Some(&st), &key("renamed", "pr-1")), Ok(id_of("d-id", "pr-1")));
        // A removed owner isn't in state: its last-known name still finds
        // the archived thread; so does the raw id, and no state at all.
        assert_eq!(locate(&inbox, Some(&st), &key("gone", "pr-2")), Ok(id_of("old-id", "pr-2")));
        assert_eq!(locate(&inbox, None, &key("d", "pr-1")), Ok(id_of("d-id", "pr-1")));
        assert_eq!(locate(&inbox, None, &key("old-id", "pr-2")), Ok(id_of("old-id", "pr-2")));
        let id = id_of("d-id", "pr-1");
        assert_eq!(locate(&inbox, None, &ThreadArg::Id(id)), Ok(id));
        assert_eq!(locate(&inbox, Some(&st), &key("renamed", "nope")).unwrap_err().code, "not-found");
        assert_eq!(locate(&inbox, None, &ThreadArg::Id(999)).unwrap_err().code, "not-found");
    }

    /// The CLI's local path and the API handler decide alike: the same
    /// `Op`s, the same error codes and messages.
    #[cfg(unix)]
    #[test]
    fn local_and_api_decisions_agree() {
        let path = store_path("agree");
        put(&path, "d-id", "d", "pr-1");
        let send = MessageSend { thread: "pr-1".into(), id: "run-1".into(), blocks: vec![Block::Form(form::tests::mixed())] };
        store::update_at(&path, |i| i.send("d-id", 11, send)).unwrap();
        let inbox = ops::load(&path).unwrap();
        let id = ops::thread_id(&inbox, "d-id", "pr-1").unwrap();
        let answers = |v: Value| v.as_object().unwrap().clone();
        let cases = [
            Mutation::Reply("hi".into()),
            Mutation::Reply("  ".into()),
            Mutation::Act("go".into()),
            Mutation::Act("nope".into()),
            Mutation::Done,
            Mutation::Reopen,
            Mutation::Submit { message: "run-1".into(), answers: answers(json!({"note": "ok"})) },
            Mutation::Submit { message: "run-1".into(), answers: answers(json!({})) },
            Mutation::Submit { message: "run-1".into(), answers: answers(json!({"pick": "z", "note": "ok"})) },
            Mutation::Submit { message: "nope".into(), answers: answers(json!({})) },
        ];
        for m in &cases {
            let cli = m.decide(&inbox, &Addr::id(id));
            // What the API handler decides, read back as the ops it applied:
            // run it on a copy of the store and compare the events + errors.
            let copy = store_path(&format!("agree-copy-{}", m.method()));
            std::fs::create_dir_all(copy.parent().unwrap()).unwrap();
            std::fs::copy(&path, &copy).unwrap();
            let api = crate::serve::api::call(
                m.method(),
                m.params(id),
                &crate::serve::api::Ctx { inbox: &copy, client: "cli", bridges: None, forwards: None },
            );
            match (&cli, &api) {
                (Ok(_), Ok(_)) => {
                    let local = store_path(&format!("agree-local-{}", m.method()));
                    std::fs::create_dir_all(local.parent().unwrap()).unwrap();
                    std::fs::copy(&path, &local).unwrap();
                    apply_local(&local, id, m).unwrap().unwrap();
                    let events = |p: &Path| -> Vec<Value> {
                        ops::events(p, "d-id").unwrap().into_iter().map(|(_, e)| strip_id(serde_json::to_value(e).unwrap())).collect()
                    };
                    assert_eq!(events(&local), events(&copy), "{m:?}");
                }
                (Err(c), Err(a)) => assert_eq!(c, a, "{m:?}"),
                _ => panic!("{m:?}: cli {cli:?} vs api {api:?}"),
            }
        }
        assert!(matches!(Mutation::Reply("hi".into()).decide(&inbox, &Addr::id(id)).unwrap()[..], [Op::Reply { .. }]));
    }

    /// An event without its store-assigned id and timestamps.
    fn strip_id(mut v: Value) -> Value {
        if let Some(o) = v.as_object_mut() {
            for k in ["id", "at", "seq"] {
                o.remove(k);
            }
        }
        v
    }

    #[test]
    fn local_fallback_end_to_end() {
        let path = store_path("e2e");
        put(&path, "d-id", "d", "pr-1");
        let send = MessageSend { thread: "pr-1".into(), id: "run-1".into(), blocks: vec![Block::Form(form::tests::mixed())] };
        store::update_at(&path, |i| i.send("d-id", 11, send)).unwrap();
        let arg = ThreadArg::Key { owner: "d".into(), key: "pr-1".into() };
        let id = locate(&ops::load(&path).unwrap(), None, &arg).unwrap();

        apply_local(&path, id, &Mutation::Reply("looks good".into())).unwrap().unwrap();
        apply_local(&path, id, &Mutation::Act("go".into())).unwrap().unwrap();
        let answers = parse_answers(r#"{"pick": "b", "note": "hi"}"#).unwrap();
        apply_local(&path, id, &Mutation::Submit { message: "run-1".into(), answers }).unwrap().unwrap();
        apply_local(&path, id, &Mutation::Done).unwrap().unwrap();
        let kinds: Vec<String> = ops::events(&path, "d-id").unwrap().into_iter().map(|(_, e)| e.kind.as_str().to_string()).collect();
        assert_eq!(kinds, ["reply", "action", "submit", "done"]);
        let events = ops::events(&path, "d-id").unwrap();
        assert_eq!(events[2].1.answers, Some(json!({"pick": "b", "tags": [], "note": "hi", "sure": null})));

        // Missing required answers name them; the form is closed after one submit.
        let again = apply_local(&path, id, &Mutation::Submit { message: "run-1".into(), answers: Default::default() }).unwrap();
        assert_eq!(again.unwrap_err().code, "closed-form");
        assert_eq!(apply_local(&path, id, &Mutation::Reply(String::new())).unwrap().unwrap_err().code, "invalid");
        // The feed records the CLI as the client.
        assert!(ops::load(&path).unwrap().to_json().unwrap().contains("\"cli\""));
    }

    #[test]
    fn missing_required_answers_are_named() {
        let path = store_path("missing");
        put(&path, "d-id", "d", "pr-1");
        let send = MessageSend { thread: "pr-1".into(), id: "run-1".into(), blocks: vec![Block::Form(form::tests::mixed())] };
        store::update_at(&path, |i| i.send("d-id", 11, send)).unwrap();
        let id = ops::thread_id(&ops::load(&path).unwrap(), "d-id", "pr-1").unwrap();
        let e = apply_local(&path, id, &Mutation::Submit { message: "run-1".into(), answers: parse_answers("").unwrap() })
            .unwrap()
            .unwrap_err();
        assert_eq!((e.code, e.message.as_str()), ("invalid", "required questions unanswered: pick, note"));
    }

    #[test]
    fn answers_and_reply_text_parse() {
        assert_eq!(parse_answers("  \n").unwrap(), serde_json::Map::new());
        assert_eq!(parse_answers(r#"{"a": ["x"], "b": true}"#).unwrap().len(), 2);
        assert!(parse_answers("[1]").unwrap_err().to_string().contains("JSON object"));
        assert!(parse_answers("{").is_err());
        assert_eq!(strip_newline("hi\n".into()), "hi");
        assert_eq!(strip_newline("hi\r\n".into()), "hi");
        assert_eq!(strip_newline("a\n\n".into()), "a\n");
        assert_eq!(reply_text("plain").unwrap(), "plain");
    }

    #[test]
    fn params_carry_the_thread_id() {
        let m = Mutation::Submit { message: "run-1".into(), answers: parse_answers(r#"{"a":"x"}"#).unwrap() };
        assert_eq!(m.params(7), json!({"thread": 7, "message": "run-1", "answers": {"a": "x"}}));
        assert_eq!(Mutation::Done.params(7), json!({"thread": 7}));
        assert_eq!(Mutation::Reply("t".into()).params(7), json!({"thread": 7, "text": "t"}));
        assert_eq!(Mutation::Act("go".into()).params(7), json!({"thread": 7, "action": "go"}));
    }

    #[test]
    fn ls_renders_a_table_and_json_an_envelope() {
        let path = store_path("ls");
        put(&path, "d-id", "d", "pr-1");
        let record = Record { level: Level::Warn, key: None, link: None, msg: "a very long notification ".repeat(4), at: 5 };
        store::update_at(&path, |i| i.push("w-id".into(), "w".into(), record, true)).unwrap();
        let rows = wire::summaries(&ops::load(&path).unwrap(), View::All);
        let out = ls_table(&rows, 10 + 120, 0);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3, "{out}");
        assert!(lines[0].starts_with("ID  STATE"), "{out}");
        let thread = lines.iter().find(|l| l.contains("pr-1")).unwrap();
        assert!(thread.contains("needs you") && thread.contains("pr-1 title") && thread.ends_with("2m"), "{thread}");
        let note = lines.iter().find(|l| l.contains("warn")).unwrap();
        assert!(note.contains("…") && note.contains(" w "), "{note}");

        let json = serde_json::to_value(Envelope::new(rows)).unwrap();
        assert_eq!(json["schema"], 1);
        assert_eq!(json["data"][0]["key"], "pr-1", "newest change first");
        assert_eq!(json["data"][0]["state"], "needs-you");
        assert_eq!(json["data"][1]["kind"], "notify");
    }

    #[test]
    fn show_renders_header_then_feed_newest_first() {
        let path = store_path("show");
        put(&path, "d-id", "d", "pr-1");
        let blocks = vec![
            Block::Markdown { text: "Addressed **3** comments.\nOne left.".into() },
            Block::Fields { items: vec![crate::inbox::feed::Field { label: "Head".into(), value: "36b13d41".into() }] },
            Block::Form(form::tests::mixed()),
        ];
        store::update_at(&path, |i| i.send("d-id", 11, MessageSend { thread: "pr-1".into(), id: "run-1".into(), blocks })).unwrap();
        let id = ops::thread_id(&ops::load(&path).unwrap(), "d-id", "pr-1").unwrap();
        apply_local(&path, id, &Mutation::Reply("ship it".into())).unwrap().unwrap();
        let inbox = ops::load(&path).unwrap();
        let detail = ThreadDetail::of(inbox.threads.iter().find(|t| t.id == id).unwrap());
        let out = render_detail(&detail, 10_000, 0);
        let want_head = format!(
            "pr-1 title\nneeds you · waiting\nd/pr-1  (thread {id})\nactions: [go] Go  [fin] Finish (sets done)\nreply: instructions  (devsandbox inbox reply {id} <text>)\n"
        );
        assert!(out.starts_with(&want_head), "{out}");
        let reply = out.find("you\n    ship it").expect(&out);
        let message = out.find("d  message run-1").expect(&out);
        assert!(reply < message, "newest first: {out}");
        for want in [
            "    Addressed **3** comments.\n    One left.\n",
            "    Head: 36b13d41\n",
            "(open)\n",
            "      note: ",
            " (required)\n",
            &format!("devsandbox inbox submit {id} run-1 --json"),
        ] {
            assert!(out.contains(want), "missing {want:?} in\n{out}");
        }

        let json = serde_json::to_value(Envelope::new(detail)).unwrap();
        assert_eq!(json["schema"], 1);
        assert_eq!(json["data"]["id"], id);
        assert_eq!(json["data"]["feed"][1]["type"], "reply");
    }

    #[test]
    fn markers_and_submissions_are_one_line_each() {
        let mut out = String::new();
        let s = ThreadSummary {
            id: 1,
            owner: "o".into(),
            owner_name: "o".into(),
            key: Some("k".into()),
            kind: "thread",
            state: Some("active"),
            status: None,
            title: "t".into(),
            level: None,
            unread: false,
            archived: true,
            needs_you: false,
            changed_at: 0,
        };
        let age = |_| "5m".to_string();
        render_item(&mut out, &FeedItemView::Marker { seq: 1, at: 0, marker: "state", from: Some("active".into()), to: Some("done".into()) }, &s, &age);
        render_item(&mut out, &FeedItemView::Marker { seq: 2, at: 0, marker: "reopen", from: None, to: None }, &s, &age);
        render_item(&mut out, &FeedItemView::Action { seq: 3, at: 0, action: "go".into(), label: "Go".into() }, &s, &age);
        let answers = json!({"pick": "a", "tags": ["x", "y"], "sure": null});
        render_item(&mut out, &FeedItemView::Submission { seq: 4, at: 0, message: "m".into(), form: "f".into(), answers }, &s, &age);
        assert_eq!(
            out,
            "5m  · state active → done\n5m  · reopened\n5m  you · Go [go]\n5m  you · submitted form f of message m\n    pick: a\n    sure: -\n    tags: x, y\n"
        );
        assert_eq!(chip(&s), "active (archived)");
    }
}
