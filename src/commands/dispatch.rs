//! Host side of the dispatcher control API (docs/automations.md,
//! "Dispatchers"): authorize and carry out one decoded [`Request`] from a
//! dispatcher instance, answering a [`Response`]. The transport (a `CONTROL`
//! stream on the dispatcher's bridge) only decodes, calls [`handle`], and
//! encodes.
//!
//! Every request is checked against the config as it is *now*: the
//! dispatcher's sandbox (resolved from the config root recorded on its
//! instance) must declare `dispatcher`, `ensure` needs the target in its
//! `spawn` list, `stop`/`rm` only reach instances it owns, and `ensure` of a
//! new child respects `max-instances` (all owned children in state count,
//! stopped ones included).
//!
//! Children are ordinary instances named `<sandbox>-<key>`. Operations run as
//! `devsandbox -C <config root> run|start|rebuild|stop|rm …` subprocesses, not
//! in-process: the handler runs on the TUI's bridge worker and those commands
//! print (and stream docker/git output) to inherited stdio, which would land
//! on the alternate screen. Their output goes to
//! `<data>/devsandbox/logs/dispatch-<unix>-<op>.log`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Serialize;

use crate::config::Config;
use crate::devsbd::control::{Op, Request, Response, Status};
use crate::runtime::backend;
use crate::state::{Instance, State};

/// Longest accepted child key (the charset is checked by [`valid_key`]).
pub const MAX_KEY: usize = 40;

/// Side effects of a request, injectable so the decisions are testable
/// without a runtime.
pub trait Executor {
    /// Container liveness, as `Backend::is_running`: `None` = no container.
    fn is_running(&self, container: &str) -> Result<Option<bool>, String>;
    /// Run `devsandbox -C <config_dir> <args…>`; `Err` is a short message
    /// for the response body.
    fn devsandbox(&mut self, config_dir: &Path, args: &[String]) -> Result<(), String>;
}

/// Load state and the dispatcher's config, then [`handle_with`] the real
/// executor. `dispatcher_key` is the state key of the instance whose bridge
/// the request arrived on. Blocking (it waits for the subprocess): run it off
/// the UI thread. Called by the TUI bridges' `CONTROL` handler
/// (`devsbd::bridge`), which is unix-only.
#[cfg_attr(not(unix), allow(dead_code))]
pub fn handle(dispatcher_key: &str, req: &Request) -> Response {
    let state = match State::load() {
        Ok(state) => state,
        Err(e) => return Response::new(Status::Failed, format!("{e:#}")),
    };
    let config_dir = match state.instances.get(dispatcher_key) {
        None => return denied(format!("`{dispatcher_key}` is not an instance")),
        Some(owner) => match &owner.config_dir {
            Some(dir) => dir.clone(),
            None => return no_config_dir(dispatcher_key),
        },
    };
    let config = match Config::load(&config_dir) {
        Ok(config) => config,
        Err(e) => return Response::new(Status::Failed, format!("{e:#}")),
    };
    handle_with(&state, &config, dispatcher_key, req, &mut Subprocess)
}

/// Whether `key` is an instance whose sandbox declares `dispatcher`. Lets the
/// bridge skip its host-wide lock for requests `handle` will deny anyway, so a
/// child's lifecycle command calling `devsbd` can't stall behind the parent
/// op that is waiting for it.
#[cfg_attr(not(unix), allow(dead_code))]
pub fn declares_dispatcher(key: &str) -> bool {
    let Ok(state) = State::load() else { return false };
    let Some(info) = state.instances.get(key) else { return false };
    let Some(dir) = info.config_dir.as_deref() else { return false };
    Config::load(dir)
        .and_then(|c| c.resolve_sandbox(&info.sandbox))
        .is_ok_and(|s| s.properties.dispatcher.is_some())
}

fn denied(msg: impl Into<String>) -> Response {
    Response::new(Status::Denied, msg)
}

fn usage(msg: impl Into<String>) -> Response {
    Response::new(Status::Usage, msg)
}

fn failed(msg: impl Into<String>) -> Response {
    Response::new(Status::Failed, msg)
}

fn no_config_dir(key: &str) -> Response {
    denied(format!(
        "`{key}` predates dispatcher support (no recorded config root); \
         `devsandbox rebuild --force {key}` records it"
    ))
}

/// The pure core of [`handle`], over already-loaded `state` and `config`
/// (the config root recorded on the dispatcher instance).
pub(crate) fn handle_with(
    state: &State,
    config: &Config,
    dispatcher_key: &str,
    req: &Request,
    exec: &mut dyn Executor,
) -> Response {
    let Some(owner) = state.instances.get(dispatcher_key) else {
        return denied(format!("`{dispatcher_key}` is not an instance"));
    };
    let Some(config_dir) = owner.config_dir.as_deref() else {
        return no_config_dir(dispatcher_key);
    };
    let decl = match config.resolve_sandbox(&owner.sandbox) {
        Ok(sandbox) => sandbox.properties.dispatcher,
        Err(e) => return denied(format!("{e:#}")),
    };
    let Some(decl) = decl else {
        return denied(format!("sandbox `{}` does not declare `dispatcher`", owner.sandbox));
    };
    if let Err(msg) = check_fields(req) {
        return usage(msg);
    }
    let owner_id = owner.instance_id.as_str();

    match req.op {
        Op::Ls => ls(state, owner_id, exec),
        Op::Ensure => {
            let sandbox = req.sandbox.as_deref().expect("checked by check_fields");
            let key = req.key.as_deref().expect("checked by check_fields");
            if !decl.may_spawn(sandbox) {
                return denied(format!(
                    "sandbox `{}` may not spawn `{sandbox}` (not in `dispatcher.spawn`)",
                    owner.sandbox
                ));
            }
            if !config.sandboxes.contains_key(sandbox) {
                return denied(format!("unknown sandbox `{sandbox}`"));
            }
            let name = child_name(sandbox, key);
            let child = state.instances.get(&name);
            let owned = children(state, owner_id).count();
            let action = ensure_action(&name, child, owner_id, sandbox, owned, decl.max_instances, |c| {
                exec.is_running(c)
            });
            match action {
                Err(resp) => resp,
                Ok(Ensure::Nothing) => Response::new(Status::Ok, name),
                Ok(action) => {
                    let args = ensure_args(&action, sandbox, &name, req, owner_id);
                    match exec.devsandbox(config_dir, &args) {
                        Ok(()) => Response::new(Status::Ok, name),
                        Err(e) => failed(e),
                    }
                }
            }
        }
        Op::Stop | Op::Rm => {
            let key = req.key.as_deref().expect("checked by check_fields");
            let name = match find_child(state, owner_id, key, req.sandbox.as_deref()) {
                Ok(name) => name,
                Err(resp) => return resp,
            };
            let args = vec![req.op.as_str().to_string(), name.clone()];
            match exec.devsandbox(config_dir, &args) {
                Ok(()) => Response::new(Status::Ok, name),
                Err(e) => failed(e),
            }
        }
    }
}

/// Which fields each op takes; the codec only checks syntax. Also validates
/// the key's charset.
fn check_fields(req: &Request) -> Result<(), String> {
    let op = req.op.as_str();
    let (sandbox, key, extras) = match req.op {
        Op::Ls => (Some(false), Some(false), false),
        Op::Ensure => (Some(true), Some(true), true),
        Op::Stop | Op::Rm => (None, Some(true), false),
    };
    let field = |name: &str, want: Option<bool>, has: bool| match want {
        Some(true) if !has => Err(format!("`{op}` needs `{name}`")),
        Some(false) if has => Err(format!("`{op}` takes no `{name}`")),
        _ => Ok(()),
    };
    field("sandbox", sandbox, req.sandbox.is_some())?;
    field("key", key, req.key.is_some())?;
    if !extras && (req.branch.is_some() || !req.env.is_empty()) {
        return Err(format!("`{op}` takes no `branch`/`env`"));
    }
    if let Some(key) = &req.key {
        if !valid_key(key) {
            return Err(format!(
                "bad key `{key}`: use lowercase letters, digits and `-`, starting with a \
                 letter or digit, at most {MAX_KEY} chars"
            ));
        }
    }
    Ok(())
}

/// `[a-z0-9][a-z0-9-]{0,39}`: always a valid tail for an instance name, a
/// container name (`devsandbox-<sandbox>-<key>`), and a path segment
/// (`.worktrees/<id>`).
pub fn valid_key(key: &str) -> bool {
    let mut chars = key.chars();
    let lower_alnum = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    key.len() <= MAX_KEY
        && chars.next().is_some_and(lower_alnum)
        && chars.all(|c| lower_alnum(c) || c == '-')
}

pub fn child_name(sandbox: &str, key: &str) -> String {
    format!("{sandbox}-{key}")
}

/// The key a child was ensured with, from its name; `None` once renamed away
/// from `<sandbox>-<key>`.
fn child_key<'a>(name: &'a str, info: &Instance) -> Option<&'a str> {
    name.strip_prefix(info.sandbox.as_str())?.strip_prefix('-').filter(|k| valid_key(k))
}

/// `owner_id`'s children in state, by name.
fn children<'a>(
    state: &'a State,
    owner_id: &'a str,
) -> impl Iterator<Item = (&'a String, &'a Instance)> {
    state.instances.iter().filter(move |(_, i)| i.dispatcher.as_deref() == Some(owner_id))
}

/// What `ensure` must do for the child it names.
#[derive(Debug, PartialEq, Eq)]
enum Ensure {
    /// Not in state: create it.
    Run,
    /// Stopped: start it.
    Start,
    /// In state without a container: recreate it, keeping its worktree.
    Rebuild,
    /// Running: nothing to do.
    Nothing,
}

/// The `ensure` decision: ownership, cap, and the child's state. An existing
/// child is only (re)started; a changed `branch`/`env` is ignored (they apply
/// at creation). `liveness` is only consulted for an owned child.
fn ensure_action(
    name: &str,
    child: Option<&Instance>,
    owner_id: &str,
    sandbox: &str,
    owned: usize,
    max: Option<u32>,
    liveness: impl FnOnce(&str) -> Result<Option<bool>, String>,
) -> Result<Ensure, Response> {
    let Some(child) = child else {
        if max.is_some_and(|max| owned >= max as usize) {
            return Err(denied(format!(
                "`max-instances` reached ({owned}); `rm` a child first"
            )));
        }
        return Ok(Ensure::Run);
    };
    if child.dispatcher.as_deref() != Some(owner_id) {
        return Err(denied(format!("`{name}` exists and is not this dispatcher's child")));
    }
    if child.sandbox != sandbox {
        return Err(denied(format!(
            "that name is taken by this dispatcher's child of sandbox `{}`",
            child.sandbox
        )));
    }
    match liveness(&child.container) {
        Ok(Some(true)) => Ok(Ensure::Nothing),
        Ok(Some(false)) => Ok(Ensure::Start),
        Ok(None) => Ok(Ensure::Rebuild),
        Err(e) => Err(failed(e)),
    }
}

/// `devsandbox` argv (after `-C <dir>`) carrying out `action`.
fn ensure_args(action: &Ensure, sandbox: &str, name: &str, req: &Request, owner: &str) -> Vec<String> {
    let s = |v: &str| v.to_string();
    match action {
        Ensure::Run => {
            let mut args = vec![s("run"), s(sandbox), s("--name"), s(name)];
            if let Some(branch) = &req.branch {
                args.extend([s("--branch"), branch.clone()]);
            }
            for (k, v) in &req.env {
                args.extend([s("--env"), format!("{k}={v}")]);
            }
            args.extend([s("--dispatcher"), s(owner)]);
            args
        }
        Ensure::Start => vec![s("start"), s(name)],
        Ensure::Rebuild => vec![s("rebuild"), s(name)],
        Ensure::Nothing => Vec::new(),
    }
}

/// Resolve `stop`/`rm`'s `key` (optionally narrowed by `sandbox`) to one of
/// `owner_id`'s children.
fn find_child(
    state: &State,
    owner_id: &str,
    key: &str,
    sandbox: Option<&str>,
) -> Result<String, Response> {
    let matches = |name: &str, info: &Instance| {
        child_key(name, info) == Some(key) && sandbox.is_none_or(|s| s == info.sandbox)
    };
    let mine: Vec<&String> =
        children(state, owner_id).filter(|(n, i)| matches(n, i)).map(|(n, _)| n).collect();
    match mine.as_slice() {
        [one] => Ok((*one).clone()),
        [] => {
            let foreign = state.instances.iter().find(|(n, i)| matches(n, i));
            Err(match foreign {
                Some((name, _)) => denied(format!("`{name}` is not this dispatcher's child")),
                None => failed(format!("no child with key `{key}`")),
            })
        }
        many => {
            let names: Vec<&str> = many.iter().map(|n| n.as_str()).collect();
            Err(usage(format!(
                "key `{key}` matches {}; pass the sandbox",
                names.join(", ")
            )))
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "lowercase")]
enum ChildState {
    Running,
    Stopped,
    Missing,
}

/// One `ls` entry.
#[derive(Debug, Serialize)]
struct ChildRow<'a> {
    name: &'a str,
    sandbox: &'a str,
    key: Option<&'a str>,
    state: ChildState,
    branch: Option<&'a str>,
}

fn ls(state: &State, owner_id: &str, exec: &mut dyn Executor) -> Response {
    let mut rows = Vec::new();
    for (name, info) in children(state, owner_id) {
        let live = match exec.is_running(&info.container) {
            Ok(Some(true)) => ChildState::Running,
            Ok(Some(false)) => ChildState::Stopped,
            Ok(None) => ChildState::Missing,
            Err(e) => return failed(e),
        };
        rows.push(ChildRow {
            name,
            sandbox: &info.sandbox,
            key: child_key(name, info),
            state: live,
            branch: info.branch.as_deref(),
        });
    }
    match serde_json::to_string_pretty(&rows) {
        Ok(json) => Response::new(Status::Ok, json),
        Err(e) => failed(e.to_string()),
    }
}

/// The real executor: the runtime for liveness, a quiet `devsandbox`
/// subprocess for everything else.
struct Subprocess;

/// Lines of the log quoted back in a failure response.
const TAIL_LINES: usize = 5;

impl Executor for Subprocess {
    fn is_running(&self, container: &str) -> Result<Option<bool>, String> {
        backend().is_running(container).map_err(|e| format!("{e:#}"))
    }

    fn devsandbox(&mut self, config_dir: &Path, args: &[String]) -> Result<(), String> {
        let exe = std::env::current_exe().map_err(|e| format!("cannot find devsandbox: {e}"))?;
        let op = args.first().map(String::as_str).unwrap_or("?");
        let path = log_path(op).ok_or("cannot create the dispatch log")?;
        let mut log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
        let _ = writeln!(log, "$ devsandbox -C {} {}", config_dir.display(), args.join(" "));
        let err = log.try_clone().map_err(|e| e.to_string())?;
        let status = Command::new(exe)
            .arg("-C")
            .arg(config_dir)
            .args(args)
            // Null stdin: every prompt (`rm`'s delete-branch `confirm`,
            // ambiguous-name `pick`) takes its non-TTY default. Git would
            // still ask for credentials on the controlling terminal — the
            // TUI's — so forbid that too.
            .stdin(Stdio::null())
            .stdout(log)
            .stderr(err)
            .env("GIT_TERMINAL_PROMPT", "0")
            .status()
            .map_err(|e| format!("cannot run devsandbox: {e}"))?;
        if status.success() {
            return Ok(());
        }
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        Err(format!(
            "`devsandbox {op}` failed ({status}); log: {}\n{}",
            path.display(),
            tail(&text, TAIL_LINES)
        ))
    }
}

/// `<data>/devsandbox/logs/dispatch-<unix>-<op>.log` (next to `state.toml`,
/// like the TUI's failed-command logs), its dir created.
fn log_path(op: &str) -> Option<PathBuf> {
    let dir = State::path().ok()?.parent()?.join("logs");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join(format!("dispatch-{}-{op}.log", Instance::now())))
}

/// The last `n` non-empty lines of `text`.
fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    const CONFIG: &str = r#"
[sandbox.disp]
folder = "."
dispatcher = { spawn = ["web"], max-instances = 2 }

[sandbox.any]
folder = "."
dispatcher = { spawn = ["*"] }

[sandbox.plain]
folder = "."

[sandbox.web]
folder = "."

[sandbox.api]
folder = "."
"#;

    fn inst(sandbox: &str, id: &str, owner: Option<&str>) -> Instance {
        Instance {
            sandbox: sandbox.into(),
            instance_id: id.into(),
            project: "proj".into(),
            container: format!("devsandbox-{id}"),
            folder: format!("/w/{id}").into(),
            base_folder: "/w".into(),
            worktree: None,
            branch: owner.map(|_| format!("sandbox/{id}")),
            shell_history: None,
            workspace: "/workspaces/w".into(),
            workspace_file: None,
            remote_env: BTreeMap::new(),
            remote_user: None,
            ssh_auth_sock: None,
            devsbd_arch: None,
            volumes: Vec::new(),
            dispatcher: owner.map(str::to_string),
            config_dir: Some("/cfg".into()),
            created_unix: 0,
        }
    }

    /// Dispatchers `d` (sandbox `disp`), `a` (`any`), plain `p`; `d` owns
    /// `web-one` (running) and `web-two` (stopped); `web-other` belongs to
    /// `a`; `web-mine` is a user instance.
    fn state() -> State {
        let mut state = State::default();
        for (key, sandbox, owner) in [
            ("d", "disp", None),
            ("a", "any", None),
            ("p", "plain", None),
            ("web-one", "web", Some("d")),
            ("web-two", "web", Some("d")),
            ("web-other", "web", Some("a")),
            ("web-mine", "web", None),
        ] {
            state.instances.insert(key.into(), inst(sandbox, key, owner));
        }
        state
    }

    /// Records calls; `running`: container -> liveness (absent = no container).
    #[derive(Default)]
    struct Fake {
        running: BTreeMap<String, bool>,
        calls: Vec<Vec<String>>,
        fail: bool,
    }

    impl Fake {
        fn new() -> Fake {
            let mut fake = Fake::default();
            fake.running.insert("devsandbox-web-one".into(), true);
            fake.running.insert("devsandbox-web-two".into(), false);
            fake
        }
    }

    impl Executor for Fake {
        fn is_running(&self, container: &str) -> Result<Option<bool>, String> {
            Ok(self.running.get(container).copied())
        }
        fn devsandbox(&mut self, config_dir: &Path, args: &[String]) -> Result<(), String> {
            assert_eq!(config_dir, Path::new("/cfg"));
            self.calls.push(args.to_vec());
            if self.fail { Err("boom".into()) } else { Ok(()) }
        }
    }

    fn req(op: Op, sandbox: Option<&str>, key: Option<&str>) -> Request {
        Request {
            op,
            sandbox: sandbox.map(str::to_string),
            key: key.map(str::to_string),
            branch: None,
            env: Vec::new(),
        }
    }

    fn call(state: &State, who: &str, r: &Request, fake: &mut Fake) -> Response {
        let config = Config::parse(CONFIG).unwrap();
        handle_with(state, &config, who, r, fake)
    }

    fn argv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn authorization_matrix() {
        let s = state();
        let ensure = |sb: &str| req(Op::Ensure, Some(sb), Some("new"));
        let cases: &[(&str, Request, Status, &str)] = &[
            // Not a dispatcher / not an instance at all.
            ("p", ensure("web"), Status::Denied, "does not declare `dispatcher`"),
            ("p", req(Op::Ls, None, None), Status::Denied, "does not declare"),
            ("ghost", ensure("web"), Status::Denied, "not an instance"),
            // Not in `spawn`, even though it exists.
            ("d", ensure("api"), Status::Denied, "may not spawn `api`"),
            // `*` allows any sandbox of the root, but only existing ones.
            ("a", ensure("api"), Status::Ok, "api-new"),
            ("a", ensure("nope"), Status::Denied, "unknown sandbox `nope`"),
            // Name taken by a foreign instance (user-made or another dispatcher's).
            ("d", req(Op::Ensure, Some("web"), Some("mine")), Status::Denied, "not this dispatcher's"),
            ("d", req(Op::Ensure, Some("web"), Some("other")), Status::Denied, "not this dispatcher's"),
            // stop/rm only reach owned children.
            ("d", req(Op::Stop, Some("web"), Some("other")), Status::Denied, "`web-other` is not"),
            ("d", req(Op::Rm, None, Some("mine")), Status::Denied, "`web-mine` is not"),
            ("a", req(Op::Rm, None, Some("one")), Status::Denied, "`web-one` is not"),
            ("d", req(Op::Stop, None, Some("zzz")), Status::Failed, "no child with key `zzz`"),
        ];
        for (who, r, status, needle) in cases {
            let mut fake = Fake::new();
            let resp = call(&s, who, r, &mut fake);
            assert_eq!(resp.status, *status, "{who} {r:?}: {resp:?}");
            assert!(resp.body.contains(needle), "{who} {r:?}: {resp:?}");
            if *status != Status::Ok {
                assert!(fake.calls.is_empty(), "denied requests run nothing: {r:?}");
            }
        }
    }

    #[test]
    fn missing_config_dir_or_sandbox_is_denied() {
        let mut s = state();
        s.instances.get_mut("d").unwrap().config_dir = None;
        let resp = call(&s, "d", &req(Op::Ls, None, None), &mut Fake::new());
        assert_eq!(resp.status, Status::Denied);
        assert!(resp.body.contains("rebuild --force d"), "{resp:?}");
        let mut s = state();
        s.instances.get_mut("d").unwrap().sandbox = "gone".into();
        let resp = call(&s, "d", &req(Op::Ls, None, None), &mut Fake::new());
        assert_eq!(resp.status, Status::Denied);
        assert!(resp.body.contains("unknown sandbox `gone`"), "{resp:?}");
    }

    #[test]
    fn ensure_per_child_state() {
        let mut fake = Fake::new();
        let mut s = state();
        // Room for one more under the cap: drop `web-two`.
        s.instances.remove("web-two");
        let mut r = req(Op::Ensure, Some("web"), Some("pr-1"));
        r.branch = Some("feat/x".into());
        r.env = vec![("A".into(), "1".into())];
        let resp = call(&s, "d", &r, &mut fake);
        assert_eq!(resp, Response::new(Status::Ok, "web-pr-1"));
        assert_eq!(
            fake.calls,
            vec![argv(&[
                "run", "web", "--name", "web-pr-1", "--branch", "feat/x", "--env", "A=1",
                "--dispatcher", "d",
            ])]
        );

        // Running: nothing, still the name.
        let s = state();
        let mut fake = Fake::new();
        let resp = call(&s, "d", &req(Op::Ensure, Some("web"), Some("one")), &mut fake);
        assert_eq!(resp, Response::new(Status::Ok, "web-one"));
        assert!(fake.calls.is_empty());

        // Stopped: start (branch/env of an existing child are ignored).
        let resp = call(&s, "d", &r_with(Op::Ensure, "two"), &mut fake);
        assert_eq!(resp, Response::new(Status::Ok, "web-two"));
        assert_eq!(fake.calls, vec![argv(&["start", "web-two"])]);

        // No container: rebuild, keeping the worktree.
        let mut fake = Fake::new();
        fake.running.remove("devsandbox-web-two");
        call(&s, "d", &r_with(Op::Ensure, "two"), &mut fake);
        assert_eq!(fake.calls, vec![argv(&["rebuild", "web-two"])]);

        // A failing subprocess is Failed with its message.
        let mut fake = Fake { fail: true, ..Fake::new() };
        let resp = call(&s, "d", &r_with(Op::Ensure, "two"), &mut fake);
        assert_eq!(resp, Response::new(Status::Failed, "boom"));
    }

    fn r_with(op: Op, key: &str) -> Request {
        Request {
            branch: Some("ignored".into()),
            env: vec![("X".into(), "y".into())],
            ..req(op, Some("web"), Some(key))
        }
    }

    #[test]
    fn cap_counts_owned_children_only() {
        // `d` owns two (one stopped) with max-instances = 2.
        let s = state();
        let mut fake = Fake::new();
        let resp = call(&s, "d", &req(Op::Ensure, Some("web"), Some("three")), &mut fake);
        assert_eq!(resp.status, Status::Denied);
        assert!(resp.body.contains("max-instances"), "{resp:?}");
        assert!(fake.calls.is_empty());
        // Existing children are still ensured at the cap.
        let resp = call(&s, "d", &req(Op::Ensure, Some("web"), Some("two")), &mut fake);
        assert_eq!(resp.status, Status::Ok);
        // No cap: `a` (spawn "*") creates freely; others' children don't count.
        let resp = call(&s, "a", &req(Op::Ensure, Some("web"), Some("three")), &mut fake);
        assert_eq!(resp, Response::new(Status::Ok, "web-three"));
    }

    #[test]
    fn same_name_from_another_sandbox_is_denied() {
        // `web-pr` + `1` and `web` + `pr-1` both name `web-pr-1`.
        let mut s = state();
        s.instances.insert("web-pr-1".into(), inst("web", "web-pr-1", Some("a")));
        let mut config = Config::parse(CONFIG).unwrap();
        config.sandboxes.insert("web-pr".into(), Default::default());
        let r = req(Op::Ensure, Some("web-pr"), Some("1"));
        let resp = handle_with(&s, &config, "a", &r, &mut Fake::new());
        assert_eq!(resp.status, Status::Denied);
        assert!(resp.body.contains("child of sandbox `web`"), "{resp:?}");
    }

    #[test]
    fn key_and_field_validation() {
        for good in ["a", "0", "pr-123", "a-", &"x".repeat(MAX_KEY)] {
            assert!(valid_key(good), "{good}");
        }
        for bad in ["", "-a", "PR-1", "a_b", "a.b", "a/b", "a b", "é", &"x".repeat(MAX_KEY + 1)] {
            assert!(!valid_key(bad), "{bad}");
        }
        let s = state();
        let cases: &[(Request, &str)] = &[
            (req(Op::Ensure, Some("web"), Some("Bad")), "bad key `Bad`"),
            (req(Op::Ensure, None, Some("k")), "needs `sandbox`"),
            (req(Op::Ensure, Some("web"), None), "needs `key`"),
            (req(Op::Stop, None, None), "needs `key`"),
            (req(Op::Ls, None, Some("k")), "takes no `key`"),
            (req(Op::Ls, Some("web"), None), "takes no `sandbox`"),
            (r_with(Op::Rm, "one"), "takes no `branch`/`env`"),
        ];
        for (r, needle) in cases {
            let mut fake = Fake::new();
            let resp = call(&s, "d", r, &mut fake);
            assert_eq!(resp.status, Status::Usage, "{r:?}: {resp:?}");
            assert!(resp.body.contains(needle), "{r:?}: {resp:?}");
            assert!(fake.calls.is_empty());
        }
    }

    #[test]
    fn stop_and_rm_run_on_owned_children() {
        let s = state();
        let mut fake = Fake::new();
        let resp = call(&s, "d", &req(Op::Stop, None, Some("one")), &mut fake);
        assert_eq!(resp, Response::new(Status::Ok, "web-one"));
        let resp = call(&s, "d", &req(Op::Rm, Some("web"), Some("two")), &mut fake);
        assert_eq!(resp, Response::new(Status::Ok, "web-two"));
        assert_eq!(fake.calls, vec![argv(&["stop", "web-one"]), argv(&["rm", "web-two"])]);
        // The sandbox filter narrows the lookup.
        let resp = call(&s, "d", &req(Op::Stop, Some("api"), Some("one")), &mut fake);
        assert_eq!(resp.status, Status::Failed);
    }

    #[test]
    fn stop_with_a_key_shared_across_sandboxes_is_ambiguous() {
        let mut s = state();
        s.instances.insert("api-one".into(), inst("api", "api-one", Some("d")));
        let mut fake = Fake::new();
        let resp = call(&s, "d", &req(Op::Stop, None, Some("one")), &mut fake);
        assert_eq!(resp.status, Status::Usage);
        assert!(resp.body.contains("api-one, web-one"), "{resp:?}");
        let resp = call(&s, "d", &req(Op::Stop, Some("api"), Some("one")), &mut fake);
        assert_eq!(resp, Response::new(Status::Ok, "api-one"));
    }

    #[test]
    fn ls_lists_owned_children_as_json() {
        let mut s = state();
        // A renamed child keeps its owner but loses its key.
        let mut renamed = inst("web", "web-x", Some("d"));
        renamed.branch = None;
        s.instances.insert("renamed".into(), renamed);
        let mut fake = Fake::new();
        let resp = call(&s, "d", &req(Op::Ls, None, None), &mut fake);
        assert_eq!(resp.status, Status::Ok, "{resp:?}");
        let rows: serde_json::Value = serde_json::from_str(&resp.body).unwrap();
        assert_eq!(
            rows,
            serde_json::json!([
                {"name": "renamed", "sandbox": "web", "key": null, "state": "missing",
                 "branch": null},
                {"name": "web-one", "sandbox": "web", "key": "one", "state": "running",
                 "branch": "sandbox/web-one"},
                {"name": "web-two", "sandbox": "web", "key": "two", "state": "stopped",
                 "branch": "sandbox/web-two"},
            ])
        );
        assert!(fake.calls.is_empty());
        // No children: an empty array.
        let resp = call(&s, "a", &req(Op::Ls, None, None), &mut Fake::default());
        let rows: serde_json::Value = serde_json::from_str(&resp.body).unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 1, "only `web-other`");
        let mut s = state();
        s.instances.remove("web-other");
        let resp = call(&s, "a", &req(Op::Ls, None, None), &mut Fake::default());
        assert_eq!(resp.body, "[]");
    }

    #[test]
    fn tail_keeps_last_non_empty_lines() {
        assert_eq!(tail("a\nb\n\nc\nd\n", 2), "c\nd");
        assert_eq!(tail("only\n", 5), "only");
        assert_eq!(tail("", 5), "");
    }
}
