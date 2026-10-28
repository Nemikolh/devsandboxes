//! The `machines.*` methods (docs/architecture/daemon/machines.md,
//! *`devsandbox machine add|ls|set|rm`*): `probe`, `add`, `list`, `get`,
//! `set`, `rm`.
//! Connection-free like `serve::api`: a function of the method, its params
//! and a [`Ctx`] (the registry path and the [`Dial`]), so tests run them
//! against a second test daemon standing in as the machine. The one
//! per-connection method, `machines.hold`, is [`hold`]: it returns the
//! [`Want`] the daemon keeps on the connection.
//!
//! `probe` and `add` open a fresh connection each (ssh, the hello, one or
//! two calls) and drop it; the held link per machine is `service`'s, which
//! `add`, `set` and `rm` tell to reload and `list` reads the live state of. `add`
//! checks the typed roots *on the machine* (`roots.list {paths}`): only it
//! can tell whether they exist, expand a quoted `~`, and canonicalize them.

use std::path::PathBuf;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::link::{self, Link};
use super::service::{Handle, Want};
use super::ssh::{self, Dial};
use super::{Entry, MachineKind, check_name, default_name};
use crate::serve::api::{ApiError, RootCandidate, parse};
use crate::serve::capacity::Capacity;
use crate::serve::snapshots::LinkState;

/// The hello's wait: ssh's connect and auth, the remote login shell, and a
/// lazy start of the remote daemon.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);

type Answer = Result<Value, ApiError>;

/// What the methods need from the daemon.
pub struct Ctx {
    /// `machines.toml` ([`super::path`] outside tests).
    pub registry: PathBuf,
    pub dial: Dial,
    /// This daemon's semver, which `add` warns a differing machine's against.
    pub version: String,
    /// The links: `add`/`set`/`rm` reload them, `list` reports their state. `None`
    /// only where no service runs (then every machine lists as `idle`).
    pub links: Option<Handle>,
}

/// Run `method` if it's a `machines.*` one; `None` otherwise.
pub fn call(method: &str, params: Value, ctx: &Ctx) -> Option<Answer> {
    let answer = match method {
        "machines.probe" => parse(params).and_then(|p| probe(p, ctx)),
        "machines.add" => parse(params).and_then(|p| add(p, ctx)),
        "machines.list" => list(ctx),
        "machines.get" => parse(params).and_then(|p| get(p, ctx)),
        "machines.set" => parse(params).and_then(|p| set(p, ctx)),
        "machines.rm" => parse(params).and_then(|p| rm(p, ctx)),
        _ => return None,
    };
    Some(answer)
}

/// `machines.probe`'s answer: the machine's devsandbox and its candidate
/// roots.
#[derive(Debug, Serialize, Deserialize)]
pub struct Probed {
    pub version: String,
    pub protocol: u32,
    pub roots: Vec<RootCandidate>,
}

/// `machines.add`'s answer.
#[derive(Debug, Serialize, Deserialize)]
pub struct Added {
    pub name: String,
    /// As saved: the machine's canonical paths, deduplicated.
    pub roots: Vec<PathBuf>,
    /// The machine's devsandbox semver.
    pub version: String,
    /// Set when that semver differs from this daemon's (same protocol, so
    /// allowed).
    pub warning: Option<String>,
}

/// One `machines.list` row: the registry entry and its link's live state
/// (`error` the last failure while `unreachable`; `version` from the last
/// hello).
#[derive(Debug, Serialize, Deserialize)]
pub struct MachineRow {
    pub name: String,
    pub kind: MachineKind,
    /// `None` for an offered machine.
    pub target: Option<String>,
    pub roots: Vec<PathBuf>,
    pub command: Option<String>,
    pub agent: bool,
    pub state: LinkState,
    pub error: Option<String>,
    pub version: Option<String>,
    /// Its host's capacity, from its last pushed snapshot
    /// (`snapshots::Machine::capacity`).
    #[serde(default)]
    pub capacity: Option<Capacity>,
}

#[derive(Deserialize)]
struct ProbeParams {
    target: String,
    #[serde(default)]
    command: Option<String>,
}

fn probe(p: ProbeParams, ctx: &Ctx) -> Answer {
    ssh::parse_target(&p.target).map_err(|e| ApiError::invalid(format!("{e:#}")))?;
    check_command(p.command.as_deref())?;
    // No name yet: errors say the target.
    let entry = Entry { command: p.command, ..Entry::ssh(p.target.clone(), p.target) };
    let mut link = connect(&entry, ctx)?;
    let roots: Vec<RootCandidate> = remote_call(&mut link, &entry.name, "roots.list", json!({}))?;
    let remote = link.remote();
    to_value(Probed { version: remote.version.clone(), protocol: remote.protocol, roots })
}

#[derive(Deserialize)]
struct AddParams {
    target: String,
    #[serde(default)]
    name: Option<String>,
    roots: Vec<String>,
    #[serde(default)]
    agent: bool,
    #[serde(default)]
    command: Option<String>,
}

fn add(p: AddParams, ctx: &Ctx) -> Answer {
    let invalid = |e: anyhow::Error| ApiError::invalid(format!("{e:#}"));
    ssh::parse_target(&p.target).map_err(invalid)?;
    let name = match p.name {
        Some(name) => name,
        None => default_name(&p.target).map_err(invalid)?,
    };
    check_name(&name).map_err(invalid)?;
    check_command(p.command.as_deref())?;
    if p.roots.is_empty() {
        return Err(ApiError::invalid("no roots: pass --config <path on the machine>"));
    }
    // Checked before the ssh, so a taken name fails fast; again at the save.
    if registered(ctx)?.iter().any(|e| e.name == name) {
        return Err(taken(&name));
    }

    let mut entry = Entry { command: p.command, agent: p.agent, ..Entry::ssh(name, p.target) };
    let mut link = connect(&entry, ctx)?;
    let checked: Vec<RootCandidate> = remote_call(&mut link, &entry.name, "roots.list", json!({ "paths": p.roots }))?;
    // A daemon that ignores `paths` answers its candidates instead.
    if checked.len() != p.roots.len() {
        return Err(ApiError::failed(format!("{}: its devsandbox can't check roots; upgrade it", entry.name)));
    }
    let refused: Vec<String> = checked
        .iter()
        .zip(&p.roots)
        .filter_map(|(c, given)| c.error.as_ref().map(|e| format!("`{given}`: {e}")))
        .collect();
    if !refused.is_empty() {
        return Err(ApiError::invalid(format!("{}: {}", entry.name, refused.join("; "))));
    }
    for c in checked {
        if !entry.roots.contains(&c.root) {
            entry.roots.push(c.root);
        }
    }
    let version = link.remote().version.clone();
    drop(link);

    let saved = super::update(&ctx.registry, |entries| {
        if entries.iter().any(|e| e.name == entry.name) {
            return Err(taken(&entry.name));
        }
        entries.push(entry.clone());
        Ok(())
    });
    saved.map_err(ApiError::internal)??;
    if let Some(links) = &ctx.links {
        links.reload();
    }
    let warning = (version != ctx.version)
        .then(|| format!("{} runs devsandbox {version}, this is {}", entry.name, ctx.version));
    to_value(Added { name: entry.name, roots: entry.roots, version, warning })
}

fn list(ctx: &Ctx) -> Answer {
    let rows: Vec<MachineRow> = registered(ctx)?
        .into_iter()
        .map(|e| {
            let live = ctx.links.as_ref().and_then(|l| l.remotes().get(&e.name)).unwrap_or_default();
            let (kind, target) = e.kind.wire();
            MachineRow {
                name: e.name,
                kind,
                target,
                roots: e.roots,
                command: e.command,
                agent: e.agent,
                capacity: live.capacity(),
                state: live.state,
                error: live.error,
                version: live.version,
            }
        })
        .collect();
    to_value(rows)
}

/// `machines.get`'s answer: what an ssh command to the machine needs. No
/// argv on the wire: Rust callers build it (`ssh::tty_argv`), other clients
/// from `target` and `command`. An offered machine has no `target`: no
/// ssh to it.
#[derive(Debug, Serialize, Deserialize)]
pub struct MachineInfo {
    pub name: String,
    pub kind: MachineKind,
    pub target: Option<String>,
    pub command: Option<String>,
    pub roots: Vec<PathBuf>,
    pub agent: bool,
}

fn get(p: NameParams, ctx: &Ctx) -> Answer {
    let entry = registered(ctx)?
        .into_iter()
        .find(|e| e.name == p.name)
        .ok_or_else(|| ApiError::not_found(format!("no machine named `{}`", p.name)))?;
    let (kind, target) = entry.kind.wire();
    to_value(MachineInfo {
        name: entry.name,
        kind,
        target,
        command: entry.command,
        roots: entry.roots,
        agent: entry.agent,
    })
}

#[derive(Deserialize)]
struct SetParams {
    name: String,
    agent: bool,
}

/// Change a registered machine's settings (today only `agent`). The links
/// reload only when something changed. An offered machine's `agent` gates
/// `fleet.agent`, through which its `fleet-link` serves this host's agent
/// over the fleet link (`fleet::agent`).
fn set(p: SetParams, ctx: &Ctx) -> Answer {
    let changed = super::update(&ctx.registry, |entries| {
        let entry = entries.iter_mut().find(|e| e.name == p.name)?;
        let changed = entry.agent != p.agent;
        entry.agent = p.agent;
        Some(changed)
    });
    let Some(changed) = changed.map_err(ApiError::internal)? else {
        return Err(ApiError::not_found(format!("no machine named `{}`", p.name)));
    };
    if changed && let Some(links) = &ctx.links {
        links.reload();
    }
    Ok(json!({ "ok": true }))
}

#[derive(Deserialize)]
struct NameParams {
    name: String,
}

#[derive(Deserialize)]
struct HoldParams {
    name: String,
    #[serde(default)]
    agent: Option<PathBuf>,
}

/// `machines.hold`'s answer: `agent`, whether the machine's agent relay is
/// up (always `false` for a machine with `agent` off).
#[derive(Debug, Serialize, Deserialize)]
pub struct Held {
    pub ok: bool,
    pub agent: bool,
}

/// `machines.hold {name, agent?}`: hold machine `name` wanted (its link
/// and, with `agent` on, its agent relay up) for as long as the caller keeps
/// the returned [`Want`], which the daemon keeps until the connection
/// closes. `agent` is the caller's `SSH_AUTH_SOCK`, reported to this daemon
/// (`bridge::report_agent`) like a forwarded verb's, so the relay serves
/// it. With `agent` on, waits up to [`super::forward::AGENT_WAIT`] for the
/// relay, as a forwarded verb does (the caller's ssh would race it).
pub fn hold(params: Value, ctx: &Ctx) -> Result<(Want, Held), ApiError> {
    let p: HoldParams = parse(params)?;
    if let Some(agent) = &p.agent
        && !agent.is_absolute()
    {
        return Err(ApiError::invalid(format!("`agent` must be absolute, got `{}`", agent.display())));
    }
    let no_machine = || ApiError::not_found(format!("no machine named `{}`", p.name));
    let links = ctx.links.as_ref().ok_or_else(no_machine)?;
    let on = links.entries().iter().find(|e| e.name == p.name).ok_or_else(no_machine)?.agent;
    if let Some(agent) = p.agent {
        crate::devsbd::bridge::report_agent(agent);
    }
    let want = links.want(&p.name).ok_or_else(no_machine)?;
    let ready = on && want.agent_ready(super::forward::AGENT_WAIT);
    Ok((want, Held { ok: true, agent: ready }))
}

fn rm(p: NameParams, ctx: &Ctx) -> Answer {
    let removed = super::update(&ctx.registry, |entries| {
        let before = entries.len();
        entries.retain(|e| e.name != p.name);
        entries.len() != before
    });
    if !removed.map_err(ApiError::internal)? {
        return Err(ApiError::not_found(format!("no machine named `{}`", p.name)));
    }
    if let Some(links) = &ctx.links {
        links.reload();
    }
    Ok(json!({ "ok": true }))
}

fn registered(ctx: &Ctx) -> Result<Vec<Entry>, ApiError> {
    super::load(&ctx.registry).map_err(ApiError::internal)
}

/// `command` is shell text the machine runs, like the registry's hand-set
/// field, so the only check is that there's something to run.
fn check_command(command: Option<&str>) -> Result<(), ApiError> {
    match command {
        Some(c) if c.trim().is_empty() => Err(ApiError::invalid("empty `command`: omit it for `devsandbox`")),
        _ => Ok(()),
    }
}

fn taken(name: &str) -> ApiError {
    ApiError::invalid(format!("a machine named `{name}` exists; pass --name"))
}

/// Dial `entry` and say hello: `failed` with ssh's or the hello's reason.
fn connect(entry: &Entry, ctx: &Ctx) -> Result<Link, ApiError> {
    let duplex = (ctx.dial)(entry).map_err(|e| ApiError::failed(format!("{}: {e:#}", entry.name)))?;
    link::handshake(&entry.name, duplex, HANDSHAKE_TIMEOUT).map_err(|e| ApiError::failed(e.to_string()))
}

/// One call on `link`; any error answer, or an answer that doesn't parse,
/// is `failed`, named after `machine`.
fn remote_call<T: DeserializeOwned>(link: &mut Link, machine: &str, method: &str, params: Value) -> Result<T, ApiError> {
    let r = link.conn.call(method, params).map_err(|e| ApiError::failed(format!("{machine}: {e:#}")))?;
    if let Some(e) = r.error {
        return Err(ApiError::failed(format!("{machine}: {method}: {} ({})", e.message, e.code)));
    }
    serde_json::from_value(r.result.unwrap_or(Value::Null))
        .map_err(|e| ApiError::failed(format!("{machine}: {method}: unexpected answer: {e}")))
}

fn to_value(x: impl Serialize) -> Answer {
    serde_json::to_value(x).map_err(|e| ApiError::internal(e.into()))
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Instant;

    use super::*;
    use crate::serve::client::{self, Conn, Hello};
    use crate::serve::daemon::tests::{opts, scratch, spawn_daemon};
    use crate::serve::daemon::{Exit, Options};
    use crate::serve::endpoint;
    use crate::serve::machines::ssh;
    use crate::serve::proto::Version;

    /// Connect to the test daemon in `dir` as a client, retrying while it binds.
    fn client_at(dir: &Path) -> Conn {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(mut stream) = endpoint::connect(dir) {
                // Older than any daemon: never a handoff, whatever its semver.
                let version = Version { semver: "0.0.0".into(), build: 0 };
                match client::hello(&mut stream, &version, "test").unwrap() {
                    Hello::Ready(reply) => return Conn::new(stream, reply).unwrap(),
                    other => panic!("{other:?}"),
                }
            }
            assert!(Instant::now() < deadline, "daemon never listened");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A machine: a test daemon on `base/remote` with its own state and
    /// home (`base/remote-home`), reporting `semver`.
    fn remote_opts(base: &Path, semver: &str) -> Options {
        let mut o = opts(1, 1500);
        o.version.semver = semver.into();
        o.state = Some(base.join("remote-state.toml"));
        o.home = Some(base.join("remote-home"));
        o
    }

    /// This host's test daemon on `base/local`, its registry in `base`,
    /// dialing the machine's socket in `remote`.
    fn local_opts(base: &Path, remote: Option<PathBuf>) -> Options {
        let mut o = opts(1, 1500);
        o.machines = Some(base.join("machines.toml"));
        o.dial = Some(match remote {
            Some(remote) => Arc::new(move |_: &Entry| ssh::test_support::relayed(endpoint::connect(&remote)?)),
            None => Arc::new(|e: &Entry| anyhow::bail!("ssh: Could not resolve hostname {}", e.label())),
        });
        o
    }

    fn ok(conn: &mut Conn, method: &str, params: Value) -> Value {
        let r = conn.call(method, params).unwrap();
        assert!(r.error.is_none(), "{method}: {:?}", r.error);
        r.result.unwrap()
    }

    fn err(conn: &mut Conn, method: &str, params: Value) -> (String, String) {
        let e = conn.call(method, params).unwrap().error.unwrap_or_else(|| panic!("{method} succeeded"));
        (e.code, e.message)
    }

    /// Two daemons: the machine (with `~/.devsandboxes` holding `web` and
    /// `api`, and `~/broken` whose config doesn't parse) and this host's.
    struct Pair {
        base: PathBuf,
        home: PathBuf,
        local: Conn,
        daemons: Vec<std::thread::JoinHandle<anyhow::Result<Exit>>>,
    }

    impl Pair {
        fn new(tag: &str, remote_semver: &str) -> Self {
            Self::with(tag, remote_semver, |_| {})
        }

        /// As [`Pair::new`], `local` adjusting this host's daemon's options.
        fn with(tag: &str, remote_semver: &str, local: impl FnOnce(&mut Options)) -> Self {
            let base = scratch(tag);
            let home = base.join("remote-home");
            std::fs::create_dir_all(home.join(".devsandboxes")).unwrap();
            std::fs::create_dir_all(home.join("broken")).unwrap();
            let cfg = crate::config::CONFIG_FILE;
            std::fs::write(home.join(".devsandboxes").join(cfg), "[sandbox.web]\nimage = \"a\"\n[sandbox.api]\nimage = \"a\"\n").unwrap();
            std::fs::write(home.join("broken").join(cfg), "[sandbox.web\n").unwrap();
            let remote = base.join("remote");
            let mut local_o = local_opts(&base, Some(remote.clone()));
            local(&mut local_o);
            let daemons =
                vec![spawn_daemon(&remote, remote_opts(&base, remote_semver)), spawn_daemon(&base.join("local"), local_o)];
            // The dial connects straight away: the machine must be up.
            drop(client_at(&base.join("remote")));
            let local = client_at(&base.join("local"));
            Pair { base, home, local, daemons }
        }

        fn finish(self) {
            drop(self.local);
            for d in self.daemons {
                assert_eq!(d.join().unwrap().unwrap(), Exit::Idle);
            }
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    #[test]
    fn probe_answers_the_machines_roots() {
        let mut pair = Pair::new("mach-probe", "0.6.0");
        let r = ok(&mut pair.local, "machines.probe", json!({"target": "me@box"}));
        let dot = std::fs::canonicalize(pair.home.join(".devsandboxes")).unwrap();
        assert_eq!(
            r,
            json!({"version": "0.6.0", "protocol": 1, "roots": [{"root": dot, "sandboxes": [{"name": "api"}, {"name": "web"}], "error": null}]})
        );
        assert_eq!(err(&mut pair.local, "machines.probe", json!({"target": "-oProxyCommand=x"})).0, "invalid");
        assert_eq!(err(&mut pair.local, "machines.probe", json!({})).0, "invalid");
        for empty in ["", "  "] {
            let (code, message) = err(&mut pair.local, "machines.probe", json!({"target": "me@box", "command": empty}));
            assert!(code == "invalid" && message.contains("`command`"), "{code}: {message}");
        }
        pair.finish();
    }

    #[test]
    fn a_failing_dial_is_failed_with_its_reason() {
        let base = scratch("mach-nodial");
        let daemon = spawn_daemon(&base.join("local"), local_opts(&base, None));
        let mut local = client_at(&base.join("local"));
        let (code, message) = err(&mut local, "machines.probe", json!({"target": "me@box"}));
        assert_eq!((code.as_str(), message.as_str()), ("failed", "me@box: ssh: Could not resolve hostname me@box"));
        let (code, _) = err(&mut local, "machines.add", json!({"target": "me@box", "roots": ["/x"]}));
        assert_eq!(code, "failed");
        assert!(!base.join("machines.toml").exists(), "nothing saved");
        drop(local);
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Idle);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn add_checks_the_roots_on_the_machine_then_saves() {
        let mut pair = Pair::new("mach-add", "0.6.0");
        let registry = pair.base.join("machines.toml");
        // A typed bad root is refused, naming it; nothing is saved.
        let (code, message) =
            err(&mut pair.local, "machines.add", json!({"target": "me@box", "roots": ["~/.devsandboxes", "~/gone", "rel"]}));
        assert_eq!(code, "invalid");
        assert!(message.starts_with("box: `~/gone`: no directory ") && message.ends_with("; `rel`: not absolute"), "{message}");
        let (code, message) = err(&mut pair.local, "machines.add", json!({"target": "me@box", "roots": ["~/broken"]}));
        assert!(code == "invalid" && message.contains("devsandboxes.toml"), "{code}: {message}");
        assert_eq!(err(&mut pair.local, "machines.add", json!({"target": "me@box", "roots": []})).0, "invalid");
        let (code, message) =
            err(&mut pair.local, "machines.add", json!({"target": "me@box", "roots": ["~"], "command": " "}));
        assert!(code == "invalid" && message.contains("`command`"), "{code}: {message}");
        assert!(!registry.exists(), "nothing saved");

        // The default name is the host; roots are the machine's canonical
        // paths, deduplicated in order.
        let dot = std::fs::canonicalize(pair.home.join(".devsandboxes")).unwrap();
        let home = std::fs::canonicalize(&pair.home).unwrap();
        let via = format!("{}/broken/../.devsandboxes", pair.home.display());
        let r = ok(&mut pair.local, "machines.add", json!({"target": "me@box", "roots": ["~/.devsandboxes", "~", via]}));
        assert_eq!(r, json!({"name": "box", "roots": [dot, home], "version": "0.6.0", "warning": null}));
        let saved = crate::serve::machines::load(&registry).unwrap();
        assert_eq!(saved, vec![Entry { roots: vec![dot.clone(), home], ..Entry::ssh("box", "me@box") }]);

        // Taken: by default name or by `name`; `--name` gets around it.
        let (code, message) = err(&mut pair.local, "machines.add", json!({"target": "ssh://box:2200", "roots": ["~"]}));
        assert_eq!((code.as_str(), message.as_str()), ("invalid", "a machine named `box` exists; pass --name"));
        let (code, _) = err(&mut pair.local, "machines.add", json!({"target": "x", "name": "a/b", "roots": ["~"]}));
        assert_eq!(code, "invalid");
        let (code, message) = err(&mut pair.local, "machines.add", json!({"target": "me@[::1]", "roots": ["~"]}));
        assert!(code == "invalid" && message.ends_with("pass --name"), "{message}");
        let cmd = "~/.local/bin/devsandbox";
        let r = ok(
            &mut pair.local,
            "machines.add",
            json!({"target": "ssh://box:2200", "name": "box2", "roots": ["~/.devsandboxes"], "agent": true, "command": cmd}),
        );
        assert_eq!(r["name"], "box2");
        let box2 = &crate::serve::machines::load(&registry).unwrap()[1];
        assert!(box2.agent, "`agent` saved");
        assert_eq!(box2.command.as_deref(), Some(cmd), "`command` saved");
        assert!(std::fs::read_to_string(&registry).unwrap().contains("command = \"~/.local/bin/devsandbox\""));

        // Listed sorted by name, idle (no link wanted); then removed.
        let rows = ok(&mut pair.local, "machines.list", Value::Null);
        assert_eq!(
            rows,
            json!([
                {"name": "box", "kind": "ssh", "target": "me@box", "roots": [dot, std::fs::canonicalize(&pair.home).unwrap()], "command": null, "agent": false, "state": "idle", "error": null, "version": null, "capacity": null},
                {"name": "box2", "kind": "ssh", "target": "ssh://box:2200", "roots": [dot], "command": cmd, "agent": true, "state": "idle", "error": null, "version": null, "capacity": null},
            ])
        );
        let info = ok(&mut pair.local, "machines.get", json!({"name": "box2"}));
        assert_eq!(info, json!({"name": "box2", "kind": "ssh", "target": "ssh://box:2200", "command": cmd, "roots": [dot], "agent": true}));
        assert_eq!(err(&mut pair.local, "machines.get", json!({"name": "nope"})).0, "not-found");
        assert_eq!(ok(&mut pair.local, "machines.rm", json!({"name": "box"})), json!({"ok": true}));
        let (code, _) = err(&mut pair.local, "machines.rm", json!({"name": "box"}));
        assert_eq!(code, "not-found");
        assert_eq!(ok(&mut pair.local, "machines.list", Value::Null).as_array().unwrap().len(), 1);
        pair.finish();
    }

    #[test]
    fn probe_and_add_dial_with_the_command() {
        let dialed: Arc<std::sync::Mutex<Vec<Option<String>>>> = Arc::default();
        let log = dialed.clone();
        let mut pair = Pair::with("mach-cmd", "0.6.0", move |o| {
            let inner = o.dial.take().unwrap();
            o.dial = Some(Arc::new(move |e: &Entry| {
                log.lock().unwrap().push(e.command.clone());
                inner(e)
            }));
        });
        let cmd = "~/.local/bin/devsandbox";
        ok(&mut pair.local, "machines.probe", json!({"target": "me@box", "command": cmd}));
        ok(&mut pair.local, "machines.probe", json!({"target": "me@box"}));
        ok(&mut pair.local, "machines.add", json!({"target": "me@box", "roots": ["~/.devsandboxes"], "command": cmd}));
        assert_eq!(*dialed.lock().unwrap(), [Some(cmd.to_string()), None, Some(cmd.to_string())]);
        pair.finish();
    }

    #[test]
    fn set_flips_agent_and_saves_it() {
        let base = scratch("mach-set");
        let registry = base.join("machines.toml");
        let entry = Entry { roots: vec!["/r".into()], ..Entry::ssh("box", "me@box") };
        crate::serve::machines::update(&registry, |e| e.push(entry.clone())).unwrap();
        let daemon = spawn_daemon(&base.join("local"), local_opts(&base, None));
        let mut local = client_at(&base.join("local"));
        let agent = || crate::serve::machines::load(&registry).unwrap()[0].agent;
        assert_eq!(ok(&mut local, "machines.set", json!({"name": "box", "agent": true})), json!({"ok": true}));
        assert!(agent());
        assert!(std::fs::read_to_string(&registry).unwrap().contains("agent = true"));
        assert_eq!(ok(&mut local, "machines.get", json!({"name": "box"}))["agent"], true);
        // Again is a no-op, not an error.
        assert_eq!(ok(&mut local, "machines.set", json!({"name": "box", "agent": true})), json!({"ok": true}));
        assert_eq!(ok(&mut local, "machines.set", json!({"name": "box", "agent": false})), json!({"ok": true}));
        assert!(!agent());
        assert!(!std::fs::read_to_string(&registry).unwrap().contains("agent"));
        let (code, message) = err(&mut local, "machines.set", json!({"name": "nope", "agent": true}));
        assert_eq!((code.as_str(), message.as_str()), ("not-found", "no machine named `nope`"));
        assert_eq!(err(&mut local, "machines.set", json!({"name": "box"})).0, "invalid");
        assert_eq!(crate::serve::machines::load(&registry).unwrap(), vec![entry.clone()]);
        // An offered machine's `agent` rides its fleet link: on and off are
        // taken like an ssh machine's; `get` says its kind, no target.
        crate::serve::machines::update(&registry, |e| e.push(Entry::offered("lap"))).unwrap();
        assert_eq!(ok(&mut local, "machines.set", json!({"name": "lap", "agent": true})), json!({"ok": true}));
        let lap_on = Entry { agent: true, ..Entry::offered("lap") };
        assert_eq!(crate::serve::machines::load(&registry).unwrap(), vec![entry.clone(), lap_on]);
        let info = ok(&mut local, "machines.get", json!({"name": "lap"}));
        assert_eq!(info, json!({"name": "lap", "kind": "offered", "target": null, "command": null, "roots": [], "agent": true}));
        assert_eq!(ok(&mut local, "machines.set", json!({"name": "lap", "agent": false})), json!({"ok": true}));
        assert_eq!(crate::serve::machines::load(&registry).unwrap(), vec![entry, Entry::offered("lap")]);
        drop(local);
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Idle);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn another_semver_is_a_warning() {
        let mut pair = Pair::new("mach-skew", "0.5.0");
        let r = ok(&mut pair.local, "machines.add", json!({"target": "box", "roots": ["~/.devsandboxes"]}));
        assert_eq!(r["warning"], "box runs devsandbox 0.5.0, this is 0.6.0");
        pair.finish();
    }

    #[test]
    fn npm_typings_match() {
        use crate::serve::dts::{assert_matches, prop_type, union_members};
        let root = || RootCandidate { root: "/cfg".into(), sandboxes: vec![], error: None };
        assert_matches("MachineProbe", &to_value(Probed { version: "0.6.0".into(), protocol: 1, roots: vec![root()] }).unwrap());
        let added = Added { name: "m".into(), roots: vec!["/cfg".into()], version: "0.6.0".into(), warning: None };
        assert_matches("MachineAdded", &to_value(added).unwrap());
        let row = MachineRow {
            name: "m".into(),
            kind: MachineKind::Offered,
            target: None,
            roots: vec![],
            command: None,
            agent: false,
            state: LinkState::Idle,
            error: None,
            version: None,
            capacity: Some(Capacity::default()),
        };
        let row = to_value(row).unwrap();
        assert_matches("MachineRow", &row);
        assert_matches("Capacity", &row["capacity"]);
        let info = MachineInfo { name: "m".into(), kind: MachineKind::Ssh, target: Some("m".into()), command: None, roots: vec![], agent: false };
        assert_matches("MachineInfo", &to_value(info).unwrap());
        assert_eq!(union_members("MachineKind"), ["'ssh'", "'offered'"]);
        assert_matches("MachineHeld", &to_value(Held { ok: true, agent: false }).unwrap());
        assert_eq!(union_members("LinkState"), ["'idle'", "'connecting'", "'connected'", "'unreachable'"]);
        for (method, result) in [
            ("machines.probe", "MachineProbe"),
            ("machines.add", "MachineAdded"),
            ("machines.list", "MachineRow[]"),
            ("machines.rm", "OkResult"),
            ("machines.set", "OkResult"),
            ("machines.get", "MachineInfo"),
            ("machines.hold", "MachineHeld"),
        ] {
            let ty = prop_type("ApiMethods", method).unwrap_or_else(|| panic!("ApiMethods lacks `{method}`"));
            assert!(ty.contains(&format!("result: {result} ")), "{method}: {ty}");
        }
    }
}
