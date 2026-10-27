//! The identity prompt (docs/architecture/daemon/verbs.md, *The identity
//! prompt*): before `run`/`start`/`rebuild` is sent, the target sandbox's
//! un-acked identity findings (`config.findings`, whatever `call` reaches:
//! the daemon, a machine through it, or [`local_call`] with no daemon) are
//! listed, and the user picks `[u]pdate` (`config.migrate`), `[c]ontinue as
//! is` (`config.ack`; a root user in a TOML config is kept by `config.migrate
//! {keep}`, which writes `identity = "image"`), or `[s]kip this time`. One
//! prompt covers every root of an `--all`. Nothing here blocks a verb: a
//! root that can't be read is left out (the verb reports its own errors),
//! and a failed update is a warning, but for a file changed meanwhile.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Call, CliVerb, Fed, Reply, Ui, reply, routed};
use crate::serve::api;
use crate::serve::machines::cli::found_of;
use crate::serve::machines::{Where, parse_name};
use crate::serve::proto::Response;

pub(super) const QUESTION: &str = "[u]pdate / [c]ontinue as is (don't ask again) / [s]kip this time: ";

/// One config root to ask about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Ask {
    pub root: PathBuf,
    /// Where `root` is (`None`: here); the calls go there.
    pub machine: Option<String>,
    pub scope: Scope,
}

/// Which of a root's findings count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Scope {
    /// Those applying to one of these sandboxes.
    Sandboxes(Vec<String>),
    /// Those applying to any sandbox (`--all`; an unused template's aren't
    /// a verb's business).
    Used,
}

/// `config.findings`' answer, as far as the prompt reads it.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Findings {
    file: PathBuf,
    format: String,
    file_hash: String,
    findings: Vec<Finding>,
}

#[derive(Debug, Clone, Deserialize)]
struct Finding {
    id: String,
    table: String,
    kind: String,
    key: String,
    value: String,
    edit: Option<String>,
    sandboxes: Vec<String>,
    acked: bool,
}

impl Finding {
    /// As the dashboard's Detail reads it (`snapshot::identity_notes`).
    fn line(&self) -> String {
        match &self.edit {
            Some(edit) => edit.clone(),
            None => format!("{}: {} = \"{}\" is under /root (fix by hand)", self.table, self.key, self.value),
        }
    }

    fn counts(&self, scope: &Scope) -> bool {
        !self.acked
            && match scope {
                Scope::Sandboxes(names) => self.sandboxes.iter().any(|s| names.contains(s)),
                Scope::Used => !self.sandboxes.is_empty(),
            }
    }
}

/// One root's findings to ask about.
#[derive(Debug)]
struct Group {
    root: PathBuf,
    machine: Option<String>,
    file: PathBuf,
    yaml: bool,
    file_hash: String,
    findings: Vec<Finding>,
}

impl Group {
    /// The file as messages name it: a machine's as `m:<path>`.
    fn label(&self) -> String {
        match &self.machine {
            Some(m) => format!("{m}:{}", self.file.display()),
            None => self.file.display().to_string(),
        }
    }

    fn params(&self, params: Value) -> Value {
        let mut params = params;
        params["root"] = json!(self.root);
        routed(params, self.machine.as_deref())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
    Update,
    Keep,
    Skip,
}

/// An answer to [`QUESTION`]; `None`: ask again. An empty one (Enter, or
/// stdin's end) is a skip.
fn choice(answer: &str) -> Option<Choice> {
    match answer.trim().to_ascii_lowercase().as_str() {
        "u" | "update" => Some(Choice::Update),
        "c" | "continue" => Some(Choice::Keep),
        "s" | "skip" | "" => Some(Choice::Skip),
        _ => None,
    }
}

/// Ask about `asks`' findings, on a TTY only, and do what was answered.
/// `Err` only for a file changed while asking: the user re-runs.
pub(super) fn prompt(asks: Vec<Ask>, call: &mut Call, ui: &mut Ui) -> Result<()> {
    if !ui.tty {
        return Ok(());
    }
    let groups = gather(asks, call);
    if groups.is_empty() {
        return Ok(());
    }
    (ui.say)(&listing(&groups));
    let choice = loop {
        if let Some(c) = choice(&(ui.ask)(QUESTION)?) {
            break c;
        }
    };
    for group in &groups {
        match choice {
            Choice::Skip => {}
            Choice::Update => update(group, call, ui)?,
            Choice::Keep => keep(group, call, ui)?,
        }
    }
    Ok(())
}

/// Each root's un-acked findings in scope; a root whose findings can't be
/// had (an older daemon, a bad config, an unreachable machine) is left out.
fn gather(asks: Vec<Ask>, call: &mut Call) -> Vec<Group> {
    let mut groups = Vec::new();
    for ask in asks {
        let params = routed(json!({ "root": ask.root }), ask.machine.as_deref());
        let Ok(Reply::Ok(v)) = call("config.findings", params).map(reply) else { continue };
        let Ok(found) = serde_json::from_value::<Findings>(v) else { continue };
        let findings: Vec<Finding> = found.findings.into_iter().filter(|f| f.counts(&ask.scope)).collect();
        if findings.is_empty() {
            continue;
        }
        groups.push(Group {
            root: ask.root,
            machine: ask.machine,
            file: found.file,
            yaml: found.format == "yaml",
            file_hash: found.file_hash,
            findings,
        });
    }
    groups
}

/// What the prompt shows above [`QUESTION`].
fn listing(groups: &[Group]) -> String {
    let mut out = String::new();
    for g in groups {
        out.push_str(&format!(
            "{} assumes root in the container, which breaks under identity = \"host\":\n",
            g.label()
        ));
        for f in &g.findings {
            out.push_str(&format!("  {}\n", f.line()));
        }
    }
    out.trim_end().to_string()
}

/// `u`: every finding with an edit, rewritten.
fn update(g: &Group, call: &mut Call, ui: &mut Ui) -> Result<()> {
    let ids: Vec<&str> = g.findings.iter().filter(|f| f.edit.is_some()).map(|f| f.id.as_str()).collect();
    if ids.is_empty() {
        (ui.say)(&format!("{}: nothing to update automatically; fix the entries above by hand", g.label()));
        return Ok(());
    }
    migrate(g, json!({ "ids": ids, "fileHash": g.file_hash }), call, ui)
}

/// `c`: acked, but a root user in TOML, which is kept in the file
/// (`identity = "image"`, the finding gone for good). YAML isn't rewritten:
/// its root user is acked too.
fn keep(g: &Group, call: &mut Call, ui: &mut Ui) -> Result<()> {
    let (users, others): (Vec<&Finding>, Vec<&Finding>) =
        g.findings.iter().partition(|f| f.kind == "root-user" && !g.yaml);
    if !others.is_empty() {
        let ids: Vec<&str> = others.iter().map(|f| f.id.as_str()).collect();
        match call("config.ack", g.params(json!({ "ids": ids }))).map(reply) {
            Ok(Reply::Ok(_)) => {
                let n = ids.len();
                let entries = if n == 1 { "1 entry".to_string() } else { format!("{n} entries") };
                (ui.say)(&format!("{}: {entries} kept as is, not asked about again", g.label()));
            }
            Ok(Reply::Err(e)) => (ui.say)(&format!("warning: could not record that for {}: {}", g.label(), e.message)),
            Ok(Reply::Old(why)) => (ui.say)(&format!("warning: could not record that for {}: {why}", g.label())),
            Err(e) => (ui.say)(&format!("warning: could not record that for {}: {e:#}", g.label())),
        }
    }
    if !users.is_empty() {
        let keep: Vec<&str> = users.iter().map(|f| f.id.as_str()).collect();
        migrate(g, json!({ "ids": [], "keep": keep, "fileHash": g.file_hash }), call, ui)?;
    }
    Ok(())
}

/// `config.migrate` and its report.
fn migrate(g: &Group, params: Value, call: &mut Call, ui: &mut Ui) -> Result<()> {
    let warn = |why: &str| format!("warning: could not update {}: {why}", g.label());
    match call("config.migrate", g.params(params)).map(reply) {
        Ok(Reply::Ok(v)) => {
            let edits = v["edits"].as_u64().unwrap_or(0);
            let drifted: Vec<String> =
                v["drifted"].as_array().into_iter().flatten().filter_map(|n| n.as_str().map(str::to_string)).collect();
            (ui.say)(&updated(&g.file, edits, &drifted, g.machine.as_deref()));
        }
        Ok(Reply::Err(e)) if e.code == "conflict" => {
            bail!("{} changed while you were asked; nothing written, run the command again", g.label())
        }
        // YAML: the message is the edits to make by hand.
        Ok(Reply::Err(e)) if e.code == "unsupported" => (ui.say)(&format!("{}: {}", g.label(), e.message)),
        Ok(Reply::Err(e)) => (ui.say)(&warn(&e.message)),
        Ok(Reply::Old(why)) => (ui.say)(&warn(&why)),
        Err(e) => (ui.say)(&warn(&format!("{e:#}"))),
    }
    Ok(())
}

/// `updated devsandboxes.toml (5 edits); now drifted: web, web-2 →
/// devsandbox rebuild <name>`; a machine's instances qualified.
fn updated(file: &Path, edits: u64, drifted: &[String], machine: Option<&str>) -> String {
    let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| file.display().to_string());
    let edits = if edits == 1 { "1 edit".to_string() } else { format!("{edits} edits") };
    let mut line = format!("updated {name} ({edits})");
    if !drifted.is_empty() {
        let names: Vec<String> = drifted
            .iter()
            .map(|n| match machine {
                Some(m) => format!("{m}/{n}"),
                None => n.clone(),
            })
            .collect();
        line.push_str(&format!("; now drifted: {} → devsandbox rebuild <name>", names.join(", ")));
    }
    line
}

// ---- no daemon --------------------------------------------------------------

/// The calls the prompt makes, answered in this process by the API's own
/// method table (`serve::api::call`, `state.toml` at `State::path`): what
/// [`prompt`] goes through when no daemon is reachable.
pub(super) fn local_call(method: &str, params: Value) -> Result<Response> {
    if !matches!(method, "config.findings" | "config.migrate" | "config.ack" | "instances.resolve") {
        bail!("`{method}` needs devsandbox serve");
    }
    let ctx = api::Ctx { inbox: Path::new(""), client: "cli", bridges: None, forwards: None, host: None };
    Ok(match api::call(method, params, &ctx) {
        Ok(v) => Response::ok(None, v),
        Err(e) => Response::err(None, e.code, e.message),
    })
}

/// What to ask about before `verb` runs in-process, from what's known
/// before its own prompts: `run`'s sandbox when named (with none, the
/// in-process pick comes later and `run`'s warning covers it), the sandboxes
/// an instance name can mean (`instances.resolve` over the local state), the
/// whole root for `--all`. `run --json` asks nothing.
pub(super) fn local_asks(verb: &CliVerb, root: &Path, fed: &Fed, call: &mut Call) -> Result<Vec<Ask>> {
    let here = |scope| vec![Ask { root: root.to_path_buf(), machine: None, scope }];
    let bare = |name: &str| -> Result<String> { Ok(if fed.machines { parse_name(name)?.1 } else { name.to_string() }) };
    Ok(match verb {
        CliVerb::Run { sandbox: Some(s), json: false, .. } => here(Scope::Sandboxes(vec![bare(s)?])),
        CliVerb::Rebuild { target: Some(name), .. } | CliVerb::Start { target: Some(name), .. } => {
            let Ok(Reply::Ok(v)) = call("instances.resolve", json!({ "name": bare(name)? })).map(reply) else {
                return Ok(Vec::new());
            };
            let mut sandboxes: Vec<String> = found_of(v, &Where::Local)?.into_iter().filter_map(|f| f.sandbox).collect();
            sandboxes.sort();
            sandboxes.dedup();
            match sandboxes.is_empty() {
                true => Vec::new(),
                false => here(Scope::Sandboxes(sandboxes)),
            }
        }
        CliVerb::Rebuild { target: None, .. } | CliVerb::Start { target: None, .. } => here(Scope::Used),
        _ => Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_read_case_blind_and_empty_skips() {
        assert_eq!(choice("u\n"), Some(Choice::Update));
        assert_eq!(choice(" Update "), Some(Choice::Update));
        assert_eq!(choice("C"), Some(Choice::Keep));
        assert_eq!(choice("s"), Some(Choice::Skip));
        assert_eq!(choice(""), Some(Choice::Skip), "Enter, or stdin's end");
        assert_eq!(choice("y"), None);
    }

    #[test]
    fn the_report_names_the_file_and_what_drifted() {
        let file = Path::new("/cfg/devsandboxes.toml");
        assert_eq!(updated(file, 1, &[], None), "updated devsandboxes.toml (1 edit)");
        let drifted = ["web".to_string(), "web-2".to_string()];
        assert_eq!(
            updated(file, 5, &drifted, None),
            "updated devsandboxes.toml (5 edits); now drifted: web, web-2 → devsandbox rebuild <name>"
        );
        assert_eq!(updated(file, 2, &drifted[..1], Some("a")), "updated devsandboxes.toml (2 edits); now drifted: a/web → devsandbox rebuild <name>");
    }
}
