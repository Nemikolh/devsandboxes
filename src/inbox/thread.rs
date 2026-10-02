//! The `devsbd thread put` body: the schema the host checks, and nothing the
//! helper knows about (docs/inbox-threads.md, *Decisions*: the helper only
//! checks JSON syntax, so everything here runs on the host).
//!
//! The body comes from inside a container and ends up driving dashboard
//! actions, so every field is bounded and every reference is checked: paths
//! stay inside the instance workspace, URLs are http(s) only, and the `child`
//! an action targets must be a key a dispatcher could actually have ensured.
//! A reject is one line, because that line becomes an `error` record in the
//! sender's own Inbox — it is the only feedback the author gets.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::is_url;
use crate::devsbd::control::{valid_key, MAX_KEY};

/// Numbered `1`-`9` in the thread pane, so nine is the ceiling.
pub const MAX_ACTIONS: usize = 9;

/// Caps on the free-text fields. Generous for their purpose and far under
/// `notify::MAX_RECORD`, so a runaway dispatcher can't fill the store.
const MAX_TITLE: usize = 200;
const MAX_STATUS: usize = 60;
const MAX_MESSAGE: usize = 4000;
const MAX_LINK: usize = 2000;
const MAX_LABEL: usize = 60;
const MAX_PATH: usize = 400;
const MAX_PLACEHOLDER: usize = 100;

/// A fixed set: it drives the Inbox views and the unread badge.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    NeedsYou,
    #[default]
    Active,
    Done,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::NeedsYou => "needs-you",
            State::Active => "active",
            State::Done => "done",
        }
    }
}

/// One `devsbd thread put` body.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadPut {
    pub key: String,
    pub title: String,
    #[serde(default)]
    pub link: Option<String>,
    pub state: State,
    #[serde(default)]
    pub status: Option<String>,
    /// Key of one of the sender's dispatcher children: the instance the
    /// thread is about, and what its host actions target.
    #[serde(default)]
    pub child: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub actions: Vec<Action>,
    #[serde(default)]
    pub reply: Option<Reply>,
}

/// A button on the thread. Field order is load-bearing for TOML: `host` is a
/// table and must be serialized after every scalar.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Action {
    pub id: String,
    pub label: String,
    /// Also enqueue an event for the owner, even with a `host` verb.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub notify: bool,
    /// Built-in: set the thread (and its child) done, and enqueue an event.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub done: bool,
    /// Runs in the dashboard with no dispatcher round trip; absent means the
    /// action is only an event for the owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<HostVerb>,
}

/// The fixed host-action table (docs/inbox-threads.md, *Actions*). Externally
/// tagged, so the body is exactly one verb object — two keys don't deserialize.
/// There is no arbitrary host command, ever.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HostVerb {
    Vscode(Vscode),
    Terminal(NoArgs),
    Logs(NoArgs),
    Forward(Forward),
    Open(Open),
    Rm(NoArgs),
}

/// A verb that takes no arguments; still written `{}` so every verb has the
/// same shape.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoArgs {}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Vscode {
    /// Relative to the instance's workspace folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub col: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Forward {
    /// Checked as 1-65535 rather than typed `u16`, so 0 and 70000 get the
    /// same one-line reason as every other field.
    pub port: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Open {
    pub url: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
}

/// Parse and check one put body. `Err` is a single line naming the field to
/// fix: it is shown to the sender as an `error` record.
pub fn parse(body: &str) -> Result<ThreadPut, String> {
    let put: ThreadPut = serde_json::from_str(body).map_err(|e| e.to_string())?;
    validate(&put)?;
    Ok(put)
}

fn validate(put: &ThreadPut) -> Result<(), String> {
    check_key("key", &put.key)?;
    check_text("title", &put.title, MAX_TITLE)?;
    if put.title.trim().is_empty() {
        return Err("`title` is empty".into());
    }
    if let Some(link) = &put.link {
        check_text("link", link, MAX_LINK)?;
        check_url("link", link)?;
    }
    if let Some(status) = &put.status {
        check_text("status", status, MAX_STATUS)?;
    }
    if let Some(child) = &put.child {
        check_key("child", child)?;
    }
    if let Some(message) = &put.message {
        check_text("message", message, MAX_MESSAGE)?;
    }
    if put.actions.len() > MAX_ACTIONS {
        return Err(format!("{} actions, at most {MAX_ACTIONS} allowed", put.actions.len()));
    }
    let mut seen = BTreeSet::new();
    for action in &put.actions {
        check_action(action)?;
        if !seen.insert(action.id.as_str()) {
            return Err(format!("duplicate action id `{}`", action.id));
        }
    }
    if let Some(placeholder) = put.reply.as_ref().and_then(|r| r.placeholder.as_ref()) {
        check_text("reply.placeholder", placeholder, MAX_PLACEHOLDER)?;
    }
    Ok(())
}

fn check_action(action: &Action) -> Result<(), String> {
    if !valid_action_id(&action.id) {
        return Err(format!(
            "bad action id `{}`: lowercase letters, digits and `-`, 1-{MAX_KEY} chars",
            action.id
        ));
    }
    check_text(&format!("action `{}` label", action.id), &action.label, MAX_LABEL)?;
    if action.label.trim().is_empty() {
        return Err(format!("action `{}` has an empty label", action.id));
    }
    match &action.host {
        Some(HostVerb::Vscode(v)) => {
            if let Some(path) = &v.path {
                check_workspace_path(path)?;
            }
        }
        Some(HostVerb::Forward(f)) => {
            if !(1..=65535).contains(&f.port) {
                return Err(format!("bad forward port {}: 1-65535", f.port));
            }
        }
        Some(HostVerb::Open(o)) => {
            check_text("open.url", &o.url, MAX_LINK)?;
            check_url("open.url", &o.url)?;
        }
        _ => {}
    }
    Ok(())
}

/// `[a-z0-9-]{1,40}`: the action id is a stable handle the dispatcher gets
/// back in its event, never a path or a name, so a leading `-` is harmless.
fn valid_action_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_KEY
        && id.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

fn check_key(field: &str, key: &str) -> Result<(), String> {
    if valid_key(key) {
        return Ok(());
    }
    Err(format!(
        "bad `{field}` `{key}`: lowercase letters, digits and `-`, starting with a letter or \
         digit, at most {MAX_KEY} chars"
    ))
}

fn check_text(field: &str, value: &str, max: usize) -> Result<(), String> {
    if value.len() > max {
        return Err(format!("`{field}` is longer than {max} bytes"));
    }
    // Control characters other than newline would corrupt a terminal row.
    if value.chars().any(|c| c.is_control() && c != '\n') {
        return Err(format!("`{field}` contains a control character"));
    }
    Ok(())
}

fn check_url(field: &str, url: &str) -> Result<(), String> {
    if is_url(url) {
        return Ok(());
    }
    Err(format!("`{field}` is not an http(s) URL: {url}"))
}

/// A path the `vscode` action may open: relative to the instance workspace,
/// with no component that could climb out of it. Normalization isn't enough —
/// an absolute path or a `..` is refused outright, so nothing downstream has
/// to re-derive where the workspace ends.
fn check_workspace_path(path: &str) -> Result<(), String> {
    check_text("vscode.path", path, MAX_PATH)?;
    if path.is_empty() {
        return Err("`vscode.path` is empty".into());
    }
    if path.starts_with('/') || path.starts_with('~') {
        return Err(format!("`vscode.path` must be relative to the workspace: {path}"));
    }
    for component in path.split('/') {
        if component.is_empty() {
            return Err(format!("`vscode.path` has an empty component: {path}"));
        }
        if component == ".." {
            return Err(format!("`vscode.path` must stay inside the workspace: {path}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The plan's example body (docs/inbox-threads.md, *Threads*).
    const EXAMPLE: &str = r##"{
      "key": "pr-6900",
      "title": "#6900 feat/agent-run-cost-event",
      "link": "https://github.com/stackblitz/bolt/pull/6900",
      "state": "needs-you",
      "status": "review draft replies",
      "child": "pr-6900",
      "message": "4 Greptile comments reviewed.",
      "actions": [
        { "id": "open-draft", "label": "Open draft", "host": { "vscode": { "path": ".dispatcher/pr-6900-replies.md", "line": 1 } } },
        { "id": "post", "label": "Post replies" },
        { "id": "done", "label": "Done", "done": true }
      ],
      "reply": { "placeholder": "Instructions for the next run" }
    }"##;

    #[test]
    fn parses_the_documented_body() {
        let put = parse(EXAMPLE).unwrap();
        assert_eq!(put.key, "pr-6900");
        assert_eq!(put.state, State::NeedsYou);
        assert_eq!(put.child.as_deref(), Some("pr-6900"));
        assert_eq!(put.actions.len(), 3);
        assert_eq!(
            put.actions[0].host,
            Some(HostVerb::Vscode(Vscode {
                path: Some(".dispatcher/pr-6900-replies.md".into()),
                line: Some(1),
                col: None
            }))
        );
        assert_eq!(put.actions[1].host, None, "no `host` = a dispatcher action");
        assert!(put.actions[2].done);
        assert_eq!(put.reply.unwrap().placeholder.as_deref(), Some("Instructions for the next run"));
    }

    #[test]
    fn minimal_body_and_every_host_verb() {
        let put = parse(r#"{"key":"k","title":"t","state":"active"}"#).unwrap();
        assert_eq!(put, ThreadPut { key: "k".into(), title: "t".into(), state: State::Active, ..Default::default() });
        for verb in [
            r#"{"vscode":{}}"#,
            r#"{"vscode":{"path":"a/b.rs","line":3,"col":7}}"#,
            r#"{"terminal":{}}"#,
            r#"{"logs":{}}"#,
            r#"{"forward":{"port":65535}}"#,
            r#"{"open":{"url":"https://x/y"}}"#,
            r#"{"rm":{}}"#,
        ] {
            let body = format!(
                r#"{{"key":"k","title":"t","state":"done","actions":[{{"id":"a","label":"A","host":{verb}}}]}}"#
            );
            assert!(parse(&body).is_ok(), "{verb}: {:?}", parse(&body));
        }
    }

    #[test]
    fn rejects_unknown_fields_and_bad_shapes() {
        let bad = [
            r#"{"key":"k","title":"t","state":"active","nope":1}"#,
            r#"{"key":"k","title":"t"}"#,                              // no state
            r#"{"key":"k","state":"active"}"#,                         // no title
            r#"{"title":"t","state":"active"}"#,                       // no key
            r#"{"key":"k","title":"t","state":"later"}"#,              // unknown state
            r#"{"key":"k","title":"t","state":"active","reply":{"x":1}}"#,
            // Exactly one verb per action, with its own fields only.
            r#"{"key":"k","title":"t","state":"active","actions":[{"id":"a","label":"A","host":{"terminal":{},"logs":{}}}]}"#,
            r#"{"key":"k","title":"t","state":"active","actions":[{"id":"a","label":"A","host":{"shell":{}}}]}"#,
            r#"{"key":"k","title":"t","state":"active","actions":[{"id":"a","label":"A","host":{"vscode":{"file":"x"}}}]}"#,
            r#"{"key":"k","title":"t","state":"active","actions":[{"id":"a"}]}"#,
        ];
        for body in bad {
            assert!(parse(body).is_err(), "{body} parsed");
        }
    }

    /// Each rule gets its own body, so a reason can't be reached by accident.
    #[test]
    fn rejects_every_invalid_field() {
        let put = |extra: &str| format!(r#"{{"key":"k","title":"t","state":"active"{extra}}}"#);
        let cases: &[(String, &str)] = &[
            (r#"{"key":"Bad","title":"t","state":"active"}"#.into(), "bad `key`"),
            (put(r#","child":"Bad""#), "bad `child`"),
            (r#"{"key":"k","title":"  ","state":"active"}"#.into(), "`title` is empty"),
            (format!(r#"{{"key":"k","title":"{}","state":"active"}}"#, "x".repeat(MAX_TITLE + 1)), "`title` is longer"),
            (put(&format!(r#","status":"{}""#, "x".repeat(MAX_STATUS + 1))), "`status` is longer"),
            (put(&format!(r#","message":"{}""#, "x".repeat(MAX_MESSAGE + 1))), "`message` is longer"),
            (put(r#","message":"a\u0007b""#), "control character"),
            (put(r#","link":"ftp://x""#), "`link` is not an http(s) URL"),
            (put(r#","link":"/etc/passwd""#), "`link` is not an http(s) URL"),
            (put(r#","reply":{"placeholder":"PPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPP"}"#), "`reply.placeholder` is longer"),
            (put(r#","actions":[{"id":"a","label":"A"},{"id":"a","label":"B"}]"#), "duplicate action id"),
            (put(r#","actions":[{"id":"Bad","label":"A"}]"#), "bad action id"),
            (put(r#","actions":[{"id":"","label":"A"}]"#), "bad action id"),
            (put(r#","actions":[{"id":"a","label":" "}]"#), "empty label"),
            (put(r#","actions":[{"id":"a","label":"A","host":{"forward":{"port":0}}}]"#), "bad forward port"),
            (put(r#","actions":[{"id":"a","label":"A","host":{"forward":{"port":65536}}}]"#), "bad forward port"),
            (put(r#","actions":[{"id":"a","label":"A","host":{"open":{"url":"file:///etc"}}}]"#), "`open.url` is not an http(s) URL"),
            (put(r#","actions":[{"id":"a","label":"A","host":{"vscode":{"path":"/etc/passwd"}}}]"#), "must be relative"),
            (put(r#","actions":[{"id":"a","label":"A","host":{"vscode":{"path":"~/x"}}}]"#), "must be relative"),
            (put(r#","actions":[{"id":"a","label":"A","host":{"vscode":{"path":"a/../../etc"}}}]"#), "stay inside the workspace"),
            (put(r#","actions":[{"id":"a","label":"A","host":{"vscode":{"path":"a//b"}}}]"#), "empty component"),
            (put(r#","actions":[{"id":"a","label":"A","host":{"vscode":{"path":""}}}]"#), "`vscode.path` is empty"),
        ];
        for (body, reason) in cases {
            let err = parse(body).unwrap_err();
            assert!(err.contains(reason), "{body}\nwanted {reason:?}, got {err:?}");
        }
        // Ten actions is one too many.
        let actions: Vec<String> =
            (0..10).map(|i| format!(r#"{{"id":"a{i}","label":"A"}}"#)).collect();
        let err = parse(&put(&format!(r#","actions":[{}]"#, actions.join(",")))).unwrap_err();
        assert!(err.contains("at most 9 allowed"), "{err}");
        assert!(parse(&put(&format!(r#","actions":[{}]"#, actions[..9].join(",")))).is_ok());
    }

    #[test]
    fn rejects_malformed_json_with_one_line() {
        let err = parse("{").unwrap_err();
        assert!(!err.contains('\n'), "{err}");
    }
}
