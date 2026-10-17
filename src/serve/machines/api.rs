//! The `machines.*` methods (docs/architecture/daemon/machines.md,
//! *`devsandbox machine add|ls|rm`*): `probe`, `add`, `list`, `rm`.
//! Connection-free like `serve::api`: a function of the method, its params
//! and a [`Ctx`] (the registry path and the [`Dial`]), so tests run them
//! against a second test daemon standing in as the machine.
//!
//! `probe` and `add` open a fresh connection each (ssh, the hello, one or
//! two calls) and drop it; the held link per machine is `service`'s, which
//! `add` and `rm` tell to reload and `list` reads the live state of. `add`
//! checks the typed roots *on the machine* (`roots.list {paths}`): only it
//! can tell whether they exist, expand a quoted `~`, and canonicalize them.

use std::path::PathBuf;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::link::{self, Link};
use super::service::Handle;
use super::ssh::{self, Dial};
use super::{Entry, check_name, default_name};
use crate::serve::api::{ApiError, RootCandidate, parse};
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
    /// The links: `add`/`rm` reload them, `list` reports their state. `None`
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
    pub target: String,
    pub roots: Vec<PathBuf>,
    pub command: Option<String>,
    pub state: LinkState,
    pub error: Option<String>,
    pub version: Option<String>,
}

#[derive(Deserialize)]
struct ProbeParams {
    target: String,
}

fn probe(p: ProbeParams, ctx: &Ctx) -> Answer {
    ssh::parse_target(&p.target).map_err(|e| ApiError::invalid(format!("{e:#}")))?;
    // No name yet: errors say the target.
    let entry = Entry { name: p.target.clone(), target: p.target, roots: Vec::new(), command: None };
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
}

fn add(p: AddParams, ctx: &Ctx) -> Answer {
    let invalid = |e: anyhow::Error| ApiError::invalid(format!("{e:#}"));
    ssh::parse_target(&p.target).map_err(invalid)?;
    let name = match p.name {
        Some(name) => name,
        None => default_name(&p.target).map_err(invalid)?,
    };
    check_name(&name).map_err(invalid)?;
    if p.roots.is_empty() {
        return Err(ApiError::invalid("no roots: pass --config <path on the machine>"));
    }
    // Checked before the ssh, so a taken name fails fast; again at the save.
    if registered(ctx)?.iter().any(|e| e.name == name) {
        return Err(taken(&name));
    }

    let mut entry = Entry { name, target: p.target, roots: Vec::new(), command: None };
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
            MachineRow {
                name: e.name,
                target: e.target,
                roots: e.roots,
                command: e.command,
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
/// from `target` and `command`.
#[derive(Debug, Serialize, Deserialize)]
pub struct MachineInfo {
    pub name: String,
    pub target: String,
    pub command: Option<String>,
    pub roots: Vec<PathBuf>,
}

fn get(p: NameParams, ctx: &Ctx) -> Answer {
    let entry = registered(ctx)?
        .into_iter()
        .find(|e| e.name == p.name)
        .ok_or_else(|| ApiError::not_found(format!("no machine named `{}`", p.name)))?;
    to_value(MachineInfo { name: entry.name, target: entry.target, command: entry.command, roots: entry.roots })
}

#[derive(Deserialize)]
struct NameParams {
    name: String,
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

fn taken(name: &str) -> ApiError {
    ApiError::invalid(format!("a machine named `{name}` exists; pass --name"))
}

/// Dial `entry` and say hello: `failed` with ssh's or the hello's reason.
/// The daemon's preferred host agent for `-A`, as the held links use.
fn connect(entry: &Entry, ctx: &Ctx) -> Result<Link, ApiError> {
    let agent = crate::devsbd::bridge::host_agent();
    let duplex = (ctx.dial)(entry, agent.as_deref()).map_err(|e| ApiError::failed(format!("{}: {e:#}", entry.name)))?;
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
            Some(remote) => Arc::new(move |_: &Entry, _: Option<&Path>| ssh::test_support::relayed(endpoint::connect(&remote)?)),
            None => Arc::new(|e: &Entry, _: Option<&Path>| anyhow::bail!("ssh: Could not resolve hostname {}", e.target)),
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
            let base = scratch(tag);
            let home = base.join("remote-home");
            std::fs::create_dir_all(home.join(".devsandboxes")).unwrap();
            std::fs::create_dir_all(home.join("broken")).unwrap();
            let cfg = crate::config::CONFIG_FILE;
            std::fs::write(home.join(".devsandboxes").join(cfg), "[sandbox.web]\nimage = \"a\"\n[sandbox.api]\nimage = \"a\"\n").unwrap();
            std::fs::write(home.join("broken").join(cfg), "[sandbox.web\n").unwrap();
            let remote = base.join("remote");
            let daemons = vec![
                spawn_daemon(&remote, remote_opts(&base, remote_semver)),
                spawn_daemon(&base.join("local"), local_opts(&base, Some(remote))),
            ];
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
        assert!(!registry.exists(), "nothing saved");

        // The default name is the host; roots are the machine's canonical
        // paths, deduplicated in order.
        let dot = std::fs::canonicalize(pair.home.join(".devsandboxes")).unwrap();
        let home = std::fs::canonicalize(&pair.home).unwrap();
        let via = format!("{}/broken/../.devsandboxes", pair.home.display());
        let r = ok(&mut pair.local, "machines.add", json!({"target": "me@box", "roots": ["~/.devsandboxes", "~", via]}));
        assert_eq!(r, json!({"name": "box", "roots": [dot, home], "version": "0.6.0", "warning": null}));
        let saved = crate::serve::machines::load(&registry).unwrap();
        assert_eq!(saved, vec![Entry { name: "box".into(), target: "me@box".into(), roots: vec![dot.clone(), home], command: None }]);

        // Taken: by default name or by `name`; `--name` gets around it.
        let (code, message) = err(&mut pair.local, "machines.add", json!({"target": "ssh://box:2200", "roots": ["~"]}));
        assert_eq!((code.as_str(), message.as_str()), ("invalid", "a machine named `box` exists; pass --name"));
        let (code, _) = err(&mut pair.local, "machines.add", json!({"target": "x", "name": "a/b", "roots": ["~"]}));
        assert_eq!(code, "invalid");
        let (code, message) = err(&mut pair.local, "machines.add", json!({"target": "me@[::1]", "roots": ["~"]}));
        assert!(code == "invalid" && message.ends_with("pass --name"), "{message}");
        let r = ok(&mut pair.local, "machines.add", json!({"target": "ssh://box:2200", "name": "box2", "roots": ["~/.devsandboxes"]}));
        assert_eq!(r["name"], "box2");

        // Listed sorted by name, idle (no link wanted); then removed.
        let rows = ok(&mut pair.local, "machines.list", Value::Null);
        assert_eq!(
            rows,
            json!([
                {"name": "box", "target": "me@box", "roots": [dot, std::fs::canonicalize(&pair.home).unwrap()], "command": null, "state": "idle", "error": null, "version": null},
                {"name": "box2", "target": "ssh://box:2200", "roots": [dot], "command": null, "state": "idle", "error": null, "version": null},
            ])
        );
        let info = ok(&mut pair.local, "machines.get", json!({"name": "box2"}));
        assert_eq!(info, json!({"name": "box2", "target": "ssh://box:2200", "command": null, "roots": [dot]}));
        assert_eq!(err(&mut pair.local, "machines.get", json!({"name": "nope"})).0, "not-found");
        assert_eq!(ok(&mut pair.local, "machines.rm", json!({"name": "box"})), json!({"ok": true}));
        let (code, _) = err(&mut pair.local, "machines.rm", json!({"name": "box"}));
        assert_eq!(code, "not-found");
        assert_eq!(ok(&mut pair.local, "machines.list", Value::Null).as_array().unwrap().len(), 1);
        pair.finish();
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
            target: "m".into(),
            roots: vec![],
            command: None,
            state: LinkState::Idle,
            error: None,
            version: None,
        };
        assert_matches("MachineRow", &to_value(row).unwrap());
        let info = MachineInfo { name: "m".into(), target: "m".into(), command: None, roots: vec![] };
        assert_matches("MachineInfo", &to_value(info).unwrap());
        assert_eq!(union_members("LinkState"), ["'idle'", "'connecting'", "'connected'", "'unreachable'"]);
        for (method, result) in [
            ("machines.probe", "MachineProbe"),
            ("machines.add", "MachineAdded"),
            ("machines.list", "MachineRow[]"),
            ("machines.rm", "OkResult"),
            ("machines.get", "MachineInfo"),
        ] {
            let ty = prop_type("ApiMethods", method).unwrap_or_else(|| panic!("ApiMethods lacks `{method}`"));
            assert!(ty.contains(&format!("result: {result} ")), "{method}: {ty}");
        }
    }
}
