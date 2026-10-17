//! Requests forwarded to a machine (docs/architecture/daemon/machines.md,
//! *Forwarded requests*).
//!
//! - Quick calls with `machine` ([`QUICK`]): the daemon strips it and
//!   sends the rest over the machine's held link (`service::Handle::forward`);
//!   the remote's answer is the answer, its error code and data kept.
//! - Verbs with `machine` ([`verb`]): one fresh connection per request, like
//!   a local verb's one connection, so a long build never holds up the link's
//!   quick calls. The remote's `output`/`progress` are relayed with `request`
//!   rewritten to the local id, its answer passed through.
//! - Federated `instances.resolve` and `sandboxes.list` (`machines: true`):
//!   this host and every machine in parallel, each machine given
//!   [`FANOUT_WAIT`] to answer; the joins are pure fns over the answers.
//! - [`HERE`] (`forwards.add`): not forwarded at all; the daemon serves it
//!   with `machine` kept, binding the forward on this host.

use std::net::Shutdown;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::Entry;
use super::link;
use super::service::Handle;
use super::ssh::Dial;
use crate::serve::api::{ApiError, Found, Resolved, SandboxEntry};
use crate::serve::client::Streamed;
use crate::serve::proto::{Notification, Response};
use crate::serve::verbs;

/// The non-verb methods that take a `machine`: the ones naming an
/// instance, a container or a config root. Any other with `machine` is
/// refused, so a client never gets a local answer it took for a remote one.
pub const QUICK: &[&str] = &[
    "instances.list",
    "instances.cancel",
    "instances.procs",
    "instances.signal",
    "instances.rename",
    "instances.done",
    "instances.buildLog",
    "instances.resolve",
    "containers.logs",
    "containers.inspect",
    "sandboxes.list",
    "sandboxes.get",
    "services.get",
];

pub fn is_quick(method: &str) -> bool {
    QUICK.contains(&method)
}

/// The methods whose `machine` names where the work is aimed, not where it
/// runs: the daemon answers them itself. `forwards.add` binds its listener
/// here (a forward into a machine's instance is owned by this side,
/// `docs/architecture/daemon/forwards.md`, *Remote bridges*).
pub const HERE: &[&str] = &["forwards.add"];

pub fn is_here(method: &str) -> bool {
    HERE.contains(&method)
}

/// How long a federated call waits for each machine: a link already
/// up answers well within it; one that isn't connecting costs the caller
/// this much at most, not `service`'s longer bound.
pub const FANOUT_WAIT: Duration = Duration::from_secs(5);
/// A forwarded verb's hello: ssh's connect and auth, the remote login
/// shell, a lazy start of the remote daemon.
const HANDSHAKE: Duration = Duration::from_secs(20);
/// How often a forwarded verb looks at its local client while the remote
/// is silent.
const TICK: Duration = Duration::from_millis(50);

/// A request's `machine`: `None` when absent or `null`, else a non-empty
/// string (`invalid` otherwise).
pub fn machine_of(params: &Value) -> Result<Option<String>, ApiError> {
    match params.get("machine") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(m)) if !m.is_empty() => Ok(Some(m.clone())),
        Some(_) => Err(ApiError::invalid("`machine` must be a machine name")),
    }
}

/// Whether a resolve or sandboxes listing asks every machine.
pub fn federated(params: &Value) -> bool {
    params.get("machines").and_then(Value::as_bool) == Some(true)
}

/// `params` as the machine gets them: without `machine`, and without
/// `machines` (the machine must not fan out to its own machines).
pub fn strip(mut params: Value) -> Value {
    if let Some(p) = params.as_object_mut() {
        p.remove("machine");
        p.remove("machines");
    }
    params
}

// ---- federated calls --------------------------------------------------------

/// `calls` (machine, method, params) in parallel, each on its machine's
/// link: the remote's response, or why there is none (nothing within
/// `wait`, a local error). A call still running at the deadline finishes on
/// its own thread, its answer dropped.
fn ask_all(links: &Handle, calls: Vec<(String, &'static str, Value)>, wait: Duration) -> Vec<Result<Response, String>> {
    let deadline = Instant::now() + wait;
    let (tx, rx) = mpsc::channel();
    let n = calls.len();
    let mut answers: Vec<Option<Result<Response, String>>> = (0..n).map(|_| None).collect();
    for (i, (machine, method, params)) in calls.into_iter().enumerate() {
        let (tx, links) = (tx.clone(), links.clone());
        let spawned = std::thread::Builder::new().name(format!("serve-fanout-{machine}")).spawn(move || {
            let answer = links.forward(&machine, method, params).map_err(|e| e.message);
            let _ = tx.send((i, answer));
        });
        if let Err(e) = spawned {
            answers[i] = Some(Err(format!("cannot start a thread: {e}")));
        }
    }
    drop(tx);
    while answers.iter().any(Option::is_none) {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok((i, answer)) => answers[i] = Some(answer),
            Err(_) => break,
        }
    }
    let late = format!("no answer within {} s", wait.as_secs());
    answers.into_iter().map(|a| a.unwrap_or_else(|| Err(late.clone()))).collect()
}

/// `instances.resolve {name, machines: true}`: `local` (this host's own
/// answer) joined with every registered machine's.
pub fn resolve_everywhere(params: &Value, local: Result<Value, ApiError>, links: Option<&Handle>) -> Result<Value, ApiError> {
    // A bad request (no `name`) or an unreadable state fails as it would
    // locally, before anything goes out.
    if let Err(e) = &local
        && e.code != "not-found"
    {
        return local;
    }
    let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
    let remotes = match links {
        Some(links) => {
            let machines: Vec<String> = links.entries().into_iter().map(|e| e.name).collect();
            let calls = machines.iter().map(|m| (m.clone(), "instances.resolve", json!({ "name": name }))).collect();
            machines.into_iter().zip(ask_all(links, calls, FANOUT_WAIT)).collect()
        }
        None => Vec::new(),
    };
    join_resolve(name, local, remotes)
}

/// The union of this host's resolve answer and each machine's: a remote
/// match qualified `<machine>/<name>`, all sorted, `found` in the same
/// order. A machine answering `not-found` has no matches; one that didn't
/// answer (or failed) is named in the `not-found` when nothing matched.
fn join_resolve(
    name: &str,
    local: Result<Value, ApiError>,
    remotes: Vec<(String, Result<Response, String>)>,
) -> Result<Value, ApiError> {
    let mut all: Vec<(String, Found)> = Vec::new();
    let local_message = match local {
        Ok(v) => {
            let r = resolved(v);
            all.extend(r.matches.into_iter().zip(r.found));
            None
        }
        Err(e) => Some(e.message),
    };
    let mut unreachable = Vec::new();
    for (machine, answer) in remotes {
        let r = match answer {
            Ok(Response { result: Some(v), error: None, .. }) => resolved(v),
            Ok(Response { error: Some(e), .. }) if e.code == "not-found" => continue,
            Ok(Response { error: Some(e), .. }) => {
                unreachable.push(format!("{machine} ({})", tidy(&machine, &e.message)));
                continue;
            }
            Ok(_) => continue,
            Err(why) => {
                unreachable.push(format!("{machine} ({})", tidy(&machine, &why)));
                continue;
            }
        };
        for (m, f) in r.matches.into_iter().zip(r.found) {
            all.push((format!("{machine}/{m}"), Found { machine: Some(machine.clone()), ..f }));
        }
    }
    if all.is_empty() {
        let mut message = local_message.unwrap_or_else(|| crate::commands::no_instance_matches(name));
        if !unreachable.is_empty() {
            message.push_str(&format!("; unreachable: {}", unreachable.join(", ")));
        }
        return Err(ApiError::not_found(message));
    }
    all.sort_by(|a, b| a.0.cmp(&b.0));
    let (matches, found) = all.into_iter().unzip();
    serde_json::to_value(Resolved { matches, found }).map_err(|e| ApiError::internal(e.into()))
}

/// A resolve answer with `found` filled for every match: an older daemon's
/// has none, so its matches get no root.
fn resolved(v: Value) -> Resolved {
    let mut r: Resolved = serde_json::from_value(v).unwrap_or(Resolved { matches: Vec::new(), found: Vec::new() });
    r.found = r
        .matches
        .iter()
        .map(|m| {
            let root = r.found.iter().find(|f| f.name == *m).and_then(|f| f.root.clone());
            Found { name: m.clone(), machine: None, root }
        })
        .collect();
    r
}

/// A link's error as one `unreachable` item: without the machine's name
/// (the item says it) or a second `unreachable:`.
fn tidy(machine: &str, message: &str) -> String {
    let m = message.strip_prefix(&format!("{machine}: ")).unwrap_or(message);
    m.strip_prefix("unreachable: ").unwrap_or(m).to_string()
}

/// `sandboxes.list {root, machines: true}`: `local` (this root's entries)
/// then each machine's registered roots', tagged `machine` and `root`. A
/// machine or root that doesn't answer adds nothing: the result stays an
/// array, and a client wanting to say why reads `machines.list`.
pub fn sandboxes_everywhere(local: Result<Value, ApiError>, links: Option<&Handle>) -> Result<Value, ApiError> {
    let local = local?;
    let Some(links) = links else { return Ok(local) };
    let roots: Vec<(String, PathBuf)> =
        links.entries().into_iter().flat_map(|e| e.roots.into_iter().map(move |r| (e.name.clone(), r))).collect();
    let calls = roots.iter().map(|(m, r)| (m.clone(), "sandboxes.list", json!({ "root": r }))).collect();
    let answers = ask_all(links, calls, FANOUT_WAIT);
    Ok(join_sandboxes(local, roots.into_iter().zip(answers).collect()))
}

fn join_sandboxes(local: Value, remotes: Vec<((String, PathBuf), Result<Response, String>)>) -> Value {
    let mut out = match local {
        Value::Array(rows) => rows,
        other => return other,
    };
    for ((machine, root), answer) in remotes {
        let Ok(Response { result: Some(v), error: None, .. }) = answer else { continue };
        let Ok(rows) = serde_json::from_value::<Vec<SandboxEntry>>(v) else { continue };
        out.extend(rows.into_iter().map(|row| {
            let row = SandboxEntry { machine: Some(machine.clone()), root: Some(root.clone()), ..row };
            serde_json::to_value(row).unwrap_or(Value::Null)
        }));
    }
    Value::Array(out)
}

// ---- verbs ------------------------------------------------------------------

/// What a forwarded verb needs from the daemon.
pub struct VerbCtx<'a> {
    /// The registered machines (the links' entries).
    pub entries: Vec<Entry>,
    pub dial: &'a Dial,
    /// The forwarded verbs in flight: an idle holder each, while its ssh runs.
    pub held: &'a AtomicUsize,
    /// The connection, for log lines.
    pub who: &'a str,
    pub log: fn(&str),
}

/// One forwarded verb in flight, counted in [`VerbCtx::held`] for its life.
struct Holding<'a>(&'a AtomicUsize);

impl<'a> Holding<'a> {
    fn new(n: &'a AtomicUsize) -> Self {
        n.fetch_add(1, Ordering::SeqCst);
        Self(n)
    }
}

impl Drop for Holding<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Verb `method` on machine `machine` (its `params` still carrying
/// `machine`): a fresh dial and hello, then the request minus `machine`,
/// its notifications relayed to `peer` under local request id `id`. The
/// remote relay swaps `env` for its login env, so only the agent is
/// taken from it here, for `ssh -A`.
///
/// `None`: the local client hung up, or this daemon drains. Either way the
/// connection to the machine is cut, which the remote daemon takes as its
/// client hanging up: the verb is cancelled there unless `detach`. So,
/// unlike a local verb, a held non-`detach` remote verb does *not* outlive
/// a drain (a handoff or a shutdown of this daemon cancels it): keeping its
/// ssh alive past this process would need it adopted like a spooled verb,
/// which nothing does yet. `detach` verbs (the dashboard's) run on.
pub fn verb(id: Option<u64>, method: &str, params: Value, machine: &str, ctx: &VerbCtx, peer: &dyn verbs::Peer) -> Option<Response> {
    let failed = |message: String| {
        Some(Response::err_with(id, "failed", message, Some(json!({ "exit": null, "instance": null }))))
    };
    let Some(entry) = ctx.entries.iter().find(|e| e.name == machine) else {
        return Some(Response::err(id, "not-found", format!("no machine named `{machine}`")));
    };
    let agent = match check_verb_params(&params) {
        Ok(agent) => agent.or_else(crate::devsbd::bridge::host_agent),
        Err(e) => return Some(Response::err(id, e.code, e.message)),
    };
    let _holding = Holding::new(ctx.held);
    let duplex = match (ctx.dial)(entry, agent.as_deref()) {
        Ok(duplex) => duplex,
        Err(e) => return failed(format!("{machine}: {e:#}")),
    };
    // Kept to cut the connection: a test's socket has no ssh to kill, and
    // shutting it is what the remote sees as a hang-up either way.
    let cut = duplex.stream.try_clone().ok();
    let mut link = match link::handshake(machine, duplex, HANDSHAKE) {
        Ok(link) => link,
        Err(e) => return failed(e.to_string()),
    };
    (ctx.log)(&format!("{}: {method} on {machine}", ctx.who));
    let mut on = |n: Option<&Notification>| match n {
        Some(n) if n.method == "output" || n.method == "progress" => {
            let mut params = n.params.clone();
            if let Some(p) = params.as_object_mut() {
                p.insert("request".into(), json!(id));
            }
            peer.notify(&n.method, params).is_ok() && !peer.draining() && !peer.hung_up(Duration::ZERO)
        }
        Some(_) => true,
        None => !peer.draining() && !peer.hung_up(TICK),
    };
    let streamed = link.conn.call_streaming(method, strip(params), TICK, &mut on);
    let answer = match streamed {
        Ok(Streamed::Answer(r)) => Some(Response { id, ..r }),
        Ok(Streamed::HungUp) => {
            (ctx.log)(&format!("{}: {method} on {machine}: the client left, closing the machine's connection", ctx.who));
            None
        }
        Ok(Streamed::Lost) => failed(format!("{machine}: connection lost; the verb may still be running there")),
        Err(e) => failed(format!("{machine}: {e:#}")),
    };
    if let Some(stream) = cut {
        let _ = stream.shutdown(Shutdown::Both);
    }
    drop(link);
    answer
}

/// What a forwarded verb checks itself: `env` an object, `root` an absolute
/// path (a path on the machine: whether it exists only the machine can
/// tell, so `verbs::parse` isn't run here). The env's `SSH_AUTH_SOCK`, when
/// it's an absolute path, is the agent for `-A`.
fn check_verb_params(params: &Value) -> Result<Option<PathBuf>, ApiError> {
    let Some(env) = params.get("env").and_then(Value::as_object) else {
        return Err(ApiError::invalid("bad params: `env` must be an object"));
    };
    match params.get("root").and_then(Value::as_str) {
        Some(root) if Path::new(root).is_absolute() => {}
        Some(root) => return Err(ApiError::invalid(format!("`root` must be absolute, got `{root}`"))),
        None => return Err(ApiError::invalid("bad params: `root` must be a path")),
    }
    let agent = env.get("SSH_AUTH_SOCK").and_then(Value::as_str).map(PathBuf::from).filter(|p| p.is_absolute());
    Ok(agent)
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::path::Path;
    use std::sync::Arc;

    use super::*;
    use crate::serve::client::{self, Conn, Hello};
    use crate::serve::daemon::tests::{opts, scratch, spawn_daemon};
    use crate::serve::daemon::Exit;
    use crate::serve::endpoint::{self, Stream};
    use crate::serve::machines::ssh;
    use crate::serve::proto::{ErrorBody, Version};

    #[test]
    fn machine_params_and_strip() {
        assert_eq!(machine_of(&json!({})).unwrap(), None);
        assert_eq!(machine_of(&json!({"machine": null})).unwrap(), None);
        assert_eq!(machine_of(&json!({"machine": "a"})).unwrap().as_deref(), Some("a"));
        assert_eq!(machine_of(&json!({"machine": ""})).unwrap_err().code, "invalid");
        assert_eq!(machine_of(&json!({"machine": 1})).unwrap_err().code, "invalid");
        assert_eq!(strip(json!({"machine": "a", "machines": true, "name": "w"})), json!({"name": "w"}));
        assert!(federated(&json!({"machines": true})) && !federated(&json!({"machines": false})) && !federated(&json!({})));
        assert!(is_quick("instances.buildLog") && !is_quick("inbox.threads.list") && !is_quick("run"));
    }

    fn ok(v: Value) -> Result<Response, String> {
        Ok(Response::ok(Some(1), v))
    }

    fn err(code: &str, message: &str) -> Result<Response, String> {
        Ok(Response { id: Some(1), result: None, error: Some(ErrorBody { code: code.into(), message: message.into(), data: None }) })
    }

    #[test]
    fn resolve_joins_qualify_and_sort() {
        let local = Ok(json!({"matches": ["web-1"], "found": [{"name": "web-1", "machine": null, "root": "/here"}]}));
        let remotes = vec![
            ("b".to_string(), ok(json!({"matches": ["web-2"], "found": [{"name": "web-2", "machine": null, "root": "/there"}]}))),
            // An older daemon: no `found`, so no root.
            ("a".to_string(), ok(json!({"matches": ["web-3"]}))),
            ("c".to_string(), err("not-found", "no sandbox instance matches `web`")),
            ("d".to_string(), Err("no answer within 5 s".into())),
        ];
        let r = join_resolve("web", local, remotes).unwrap();
        assert_eq!(r["matches"], json!(["a/web-3", "b/web-2", "web-1"]));
        assert_eq!(
            r["found"],
            json!([
                {"name": "web-3", "machine": "a", "root": null},
                {"name": "web-2", "machine": "b", "root": "/there"},
                {"name": "web-1", "machine": null, "root": "/here"},
            ])
        );

        let none = || Err(ApiError::not_found(crate::commands::no_instance_matches("web")));
        let e = join_resolve("web", none(), vec![("c".into(), err("not-found", "x"))]).unwrap_err();
        assert_eq!((e.code, e.message), ("not-found", crate::commands::no_instance_matches("web")));
        let down = vec![
            ("d".to_string(), err("failed", "d: unreachable: ssh: Could not resolve hostname d")),
            ("e".to_string(), Err("no answer within 5 s".into())),
        ];
        let e = join_resolve("web", none(), down).unwrap_err();
        let want = format!(
            "{}; unreachable: d (ssh: Could not resolve hostname d), e (no answer within 5 s)",
            crate::commands::no_instance_matches("web")
        );
        assert_eq!((e.code, e.message), ("not-found", want));
    }

    #[test]
    fn sandbox_joins_tag_the_machines_rows() {
        let local = json!([{"name": "loc", "root": "/here"}]);
        let remotes = vec![
            (("a".to_string(), PathBuf::from("/r")), ok(json!([{"name": "web", "root": "/r"}]))),
            (("a".to_string(), PathBuf::from("/gone")), err("not-found", "no directory /gone")),
            (("b".to_string(), PathBuf::from("/b")), Err("no answer within 5 s".into())),
        ];
        assert_eq!(
            join_sandboxes(local, remotes),
            json!([{"name": "loc", "root": "/here"}, {"name": "web", "machine": "a", "root": "/r"}])
        );
    }

    // ---- against two test daemons ------------------------------------------

    fn instance(key: &str, sandbox: &str, root: &Path) -> String {
        format!(
            "[instance.{key}]\nsandbox = \"{sandbox}\"\ncontainer = \"devsandbox-{key}\"\nfolder = \"/w\"\n\
             workspace = \"/w\"\ncreated_unix = 1\nconfig_dir = {:?}\n",
            root.to_str().unwrap()
        )
    }

    /// A machine (`box`, a test daemon whose verbs are `script`) and this
    /// host's test daemon, registered with `box` and `down` (whose dial
    /// fails). Each side has its own state and a config root defining one
    /// sandbox (`web` there, `loc` here).
    struct Pair {
        base: PathBuf,
        remote_dir: PathBuf,
        /// The remote verbs' scratch dir (also a root they accept).
        verb_dir: PathBuf,
        remote_root: PathBuf,
        local_root: PathBuf,
        local: Conn,
        daemons: Vec<std::thread::JoinHandle<anyhow::Result<Exit>>>,
    }

    impl Pair {
        fn new(tag: &str, script: &str) -> Self {
            let base = scratch(tag);
            let (remote_root, local_root) = (base.join("remote-root"), base.join("local-root"));
            for (root, sandbox) in [(&remote_root, "web"), (&local_root, "loc")] {
                std::fs::create_dir_all(root).unwrap();
                std::fs::write(root.join(crate::config::CONFIG_FILE), format!("[sandbox.{sandbox}]\nimage = \"a\"\n")).unwrap();
            }
            let remote_root = std::fs::canonicalize(&remote_root).unwrap();
            let local_root = std::fs::canonicalize(&local_root).unwrap();
            let (remote_state, local_state) = (base.join("remote-data/state.toml"), base.join("local-data/state.toml"));
            std::fs::create_dir_all(remote_state.parent().unwrap()).unwrap();
            std::fs::create_dir_all(local_state.parent().unwrap()).unwrap();
            std::fs::write(&remote_state, instance("web-r", "web", &remote_root)).unwrap();
            std::fs::write(&local_state, instance("web-l", "web", &local_root)).unwrap();

            let registry = base.join("machines.toml");
            let boxed = Entry { name: "box".into(), target: "me@box".into(), roots: vec![remote_root.clone()], command: None };
            let down = Entry { name: "down".into(), target: "me@down".into(), roots: vec!["/nonexistent".into()], command: None };
            crate::serve::machines::update(&registry, |e| e.extend([boxed, down])).unwrap();

            let remote_dir = base.join("remote");
            let (runner, verb_dir) = verbs::tests::script_runner(tag, script);
            let mut remote = opts(1, 60_000);
            remote.verbs = Some(runner);
            remote.state = Some(remote_state);
            remote.home = Some(base.join("remote-home"));
            remote.machines = Some(base.join("remote-machines.toml"));
            let mut local = opts(1, 300);
            local.state = Some(local_state);
            local.machines = Some(registry);
            local.dial = Some({
                let remote_dir = remote_dir.clone();
                Arc::new(move |e: &Entry, _: Option<&Path>| match e.name.as_str() {
                    "box" => ssh::test_support::relayed(endpoint::connect(&remote_dir)?),
                    other => anyhow::bail!("ssh: Could not resolve hostname {other}"),
                })
            });
            let daemons = vec![spawn_daemon(&remote_dir, remote), spawn_daemon(&base.join("local"), local)];
            drop(conn_at(&remote_dir));
            let local = conn_at(&base.join("local"));
            Pair { base, remote_dir, verb_dir, remote_root, local_root, local, daemons }
        }

        fn local_dir(&self) -> PathBuf {
            self.base.join("local")
        }

        fn finish(self) {
            let r = conn_at(&self.remote_dir).call("shutdown", json!({})).unwrap();
            assert!(r.error.is_none(), "{r:?}");
            drop(self.local);
            let mut daemons = self.daemons.into_iter();
            assert_eq!(daemons.next().unwrap().join().unwrap().unwrap(), Exit::Shutdown);
            assert_eq!(daemons.next().unwrap().join().unwrap().unwrap(), Exit::Idle);
            let _ = std::fs::remove_dir_all(&self.base);
            let _ = std::fs::remove_dir_all(&self.verb_dir);
        }
    }

    /// A raw connection past the hello, retrying while the daemon binds.
    fn stream_at(dir: &Path) -> Stream {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(mut stream) = endpoint::connect(dir) {
                let version = Version { semver: "0.0.0".into(), build: 0 };
                match client::hello(&mut stream, &version, "test").unwrap() {
                    Hello::Ready(_) => return stream,
                    other => panic!("{other:?}"),
                }
            }
            assert!(Instant::now() < deadline, "daemon never listened");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn conn_at(dir: &Path) -> Conn {
        let stream = stream_at(dir);
        let reply = crate::serve::proto::HelloResult { version: "0.0.0".into(), build: 0, protocol: 1, handoff: false };
        Conn::new(stream, reply).unwrap()
    }

    fn code_message(r: &Response) -> (&str, &str) {
        let e = r.error.as_ref().unwrap_or_else(|| panic!("succeeded: {r:?}"));
        (e.code.as_str(), e.message.as_str())
    }

    #[test]
    fn quick_calls_and_federated_lookups_reach_the_machine() {
        let mut pair = Pair::new("fwd-quick", "exit 0");
        let c = &mut pair.local;
        // The machine's own error, its code kept.
        let r = c.call("instances.buildLog", json!({"instance": "nope", "machine": "box"})).unwrap();
        assert_eq!(code_message(&r), ("not-found", "no instance `nope`"));
        // A forwarded resolve is the machine's answer as is.
        let r = c.call("instances.resolve", json!({"name": "web-r", "machine": "box"})).unwrap();
        assert_eq!(r.result.unwrap(), json!({"matches": ["web-r"], "found": [{"name": "web-r", "machine": null, "root": pair.remote_root}]}));
        let r = c.call("sandboxes.list", json!({"root": pair.remote_root, "machine": "box", "machines": true})).unwrap();
        assert_eq!(r.result.unwrap(), json!([{"name": "web", "root": pair.remote_root}]), "no fan-out there");
        let r = c.call("instances.buildLog", json!({"instance": "x", "machine": "nope"})).unwrap();
        assert_eq!(code_message(&r), ("not-found", "no machine named `nope`"));
        let r = c.call("instances.buildLog", json!({"instance": "x", "machine": ""})).unwrap();
        assert_eq!(code_message(&r).0, "invalid");
        let r = c.call("inbox.threads.list", json!({"machine": "box"})).unwrap();
        assert_eq!(code_message(&r), ("invalid", "`inbox.threads.list` can't take a machine yet"));
        // Served here, `machine` and all: the local API answers (a test
        // daemon has no host side), not the machine and not the refusal.
        let add = json!({"dir": pair.remote_root, "instance": "web-r", "spec": "3000", "machine": "box"});
        let r = c.call("forwards.add", add).unwrap();
        assert_eq!(code_message(&r), ("internal", "this daemon runs no forwards"));
        // The link to `down` can't connect: `failed`, why.
        let r = c.call("instances.list", json!({"dir": "/x", "machine": "down"})).unwrap();
        assert_eq!(code_message(&r), ("failed", "down: unreachable: down: ssh: Could not resolve hostname down"));

        // Federated: here and there, qualified, sorted; `down` adds nothing.
        let r = c.call("instances.resolve", json!({"name": "web", "machines": true})).unwrap().result.unwrap();
        assert_eq!(r["matches"], json!(["box/web-r", "web-l"]));
        assert_eq!(
            r["found"],
            json!([
                {"name": "web-r", "machine": "box", "root": pair.remote_root},
                {"name": "web-l", "machine": null, "root": pair.local_root},
            ])
        );
        crate::serve::dts::assert_matches("InstancesResolved", &r);
        let r = c.call("instances.resolve", json!({"name": "web-r", "machines": true})).unwrap().result.unwrap();
        assert_eq!(r["matches"], json!(["box/web-r"]), "only there");
        // Without `machines`: this host only, as before.
        let r = c.call("instances.resolve", json!({"name": "web-r"})).unwrap();
        assert_eq!(code_message(&r).0, "not-found");
        let r = c.call("instances.resolve", json!({"name": "zzz", "machines": true})).unwrap();
        let (code, message) = code_message(&r);
        let want = format!("{}; unreachable: down (", crate::commands::no_instance_matches("zzz"));
        assert!(code == "not-found" && message.starts_with(&want), "{message}");
        assert_eq!(code_message(&c.call("instances.resolve", json!({"machines": true})).unwrap()).0, "invalid");

        let r = c.call("sandboxes.list", json!({"root": pair.local_root, "machines": true})).unwrap().result.unwrap();
        assert_eq!(
            r,
            json!([{"name": "loc", "root": pair.local_root}, {"name": "web", "machine": "box", "root": pair.remote_root}])
        );
        let r = c.call("sandboxes.list", json!({"root": pair.local_root})).unwrap().result.unwrap();
        assert_eq!(r, json!([{"name": "loc", "root": pair.local_root}]), "this host only");
        pair.finish();
    }

    /// The remote verbs: `*slow*` traps SIGINT (a marker `int-<name>`),
    /// says `ready` and takes 2 s (then `done-<name>`); the rest fail.
    const SCRIPT: &str = r#"for a; do n=$a; done
        case "$*" in
        *slow*) trap 'touch "$DIR/int-$n"; exit 130' INT; echo ready; sleep 2; touch "$DIR/done-$n" ;;
        *) echo "hi $n"; echo 'Error: nope' >&2; exit 2 ;;
        esac"#;

    fn send(stream: &Stream, id: u64, method: &str, params: Value) {
        writeln!(&*stream, "{}", json!({"id": id, "method": method, "params": params})).unwrap();
    }

    fn verb_params(root: &Path, extra: Value) -> Value {
        let mut p = json!({"root": root, "env": {"PATH": std::env::var("PATH").unwrap_or_default()}, "machine": "box"});
        p.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        p
    }

    /// Read until request `id`'s answer: it and the notifications before.
    fn answer(reader: &mut BufReader<Stream>, id: u64) -> (Value, Vec<Value>) {
        let mut notes = Vec::new();
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).unwrap() > 0, "closed before the answer");
            let msg: Value = serde_json::from_str(&line).unwrap();
            if msg.get("id").is_some() {
                assert_eq!(msg["id"], id);
                return (msg, notes);
            }
            notes.push(msg);
        }
    }

    fn until_ready(reader: &mut BufReader<Stream>) {
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).unwrap() > 0, "closed before ready");
            assert!(!line.contains("\"id\""), "answered early: {line}");
            if line.contains("ready") {
                return;
            }
        }
    }

    fn wait_file(path: &Path, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while !path.exists() {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        true
    }

    #[test]
    fn forwarded_verbs_stream_and_a_hang_up_cancels_there() {
        let pair = Pair::new("fwd-verbs", SCRIPT);
        let root = pair.verb_dir.clone();
        let stream = stream_at(&pair.local_dir());
        let mut reader = BufReader::new(stream.try_clone().unwrap());

        // Streamed under this request's id, the machine's `failed` as is.
        send(&stream, 7, "stop", verb_params(&root, json!({"instance": "fast"})));
        let (a, notes) = answer(&mut reader, 7);
        assert_eq!(a["error"], json!({"code": "failed", "message": "nope", "data": {"exit": 2, "instance": null}}));
        let out: String = notes
            .iter()
            .filter(|n| n["method"] == "output" && n["params"]["stream"] == "stdout")
            .map(|n| n["params"]["data"].as_str().unwrap())
            .collect();
        assert_eq!(out, "hi fast\n");
        assert!(!notes.is_empty() && notes.iter().all(|n| n["params"]["request"] == 7), "{notes:?}");

        // Refused here, before any dial.
        send(&stream, 8, "stop", verb_params(Path::new("rel"), json!({"instance": "fast"})));
        assert_eq!(answer(&mut reader, 8).0["error"]["code"], "invalid");
        send(&stream, 9, "stop", verb_params(&root, json!({"instance": "fast", "machine": "nope"})));
        assert_eq!(answer(&mut reader, 9).0["error"]["code"], "not-found");
        // A dial that fails: `failed`, why, no exit.
        send(&stream, 10, "stop", verb_params(&root, json!({"instance": "fast", "machine": "down"})));
        let e = answer(&mut reader, 10).0["error"].clone();
        assert_eq!(
            e,
            json!({"code": "failed", "message": "down: ssh: Could not resolve hostname down", "data": {"exit": null, "instance": null}})
        );

        // A hang-up mid-verb: SIGINT there.
        send(&stream, 11, "stop", verb_params(&root, json!({"instance": "slow-a"})));
        until_ready(&mut reader);
        drop((reader, stream));
        assert!(wait_file(&root.join("int-slow-a"), Duration::from_secs(3)), "no SIGINT there");
        assert!(!root.join("done-slow-a").exists());

        // `detach`: a hang-up leaves it running to its end.
        let stream = stream_at(&pair.local_dir());
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        send(&stream, 1, "stop", verb_params(&root, json!({"instance": "slow-b", "detach": true})));
        until_ready(&mut reader);
        drop((reader, stream));
        assert!(wait_file(&root.join("done-slow-b"), Duration::from_secs(5)), "the verb didn't finish");
        assert!(!root.join("int-slow-b").exists(), "SIGINTed");
        pair.finish();
    }
}
