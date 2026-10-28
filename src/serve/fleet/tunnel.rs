//! The worker's end of a fleet link's `TUNNEL` stream
//! (docs/architecture/daemon/fleet.md, *TUNNEL*): one port forward's bridge
//! into a lent instance, or into a service one of them uses, what `devsandbox
//! tunnel` is at the far end of an ssh machine's forward.
//!
//! The hub's first line is a `commands::tunnel::Request` (`{"root",
//! "instance"?, "service"?, "port"}`). The worker answers one
//! `commands::tunnel::Header` line, `{"error"}` for any refusal, then runs
//! the in-container `devsbd bridge` (`bridge::bridge_command`) with the
//! stream itself as its stdin and stdout, so the bridge's handshake and mux
//! follow the header as over ssh. The bridge is killed and reaped once the
//! hub closes the stream.
//!
//! The gate: the root confined to the peer's reach (`lent::confine_path`:
//! the fleet root or an own root under `worker`, any under `full`); routes
//! resolved (`commands::tunnel::resolve`) over [`scoped`] state only, the
//! lent instances whose config root is under it, so neither an instance
//! name nor a service's via-instance pick can reach another; a service must
//! be one such an instance (the named one, when given) uses
//! ([`check_service`]), so an injection is only into a service container of
//! that root's project. [`read_request`], [`scoped`], [`confine`] and
//! [`check_service`] are pure (`confine` canonicalizes); [`serve`] is the
//! I/O.

use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::Arc;
use std::time::Duration;

use super::lent::{self, Files, Peer, Reach};
use crate::commands::port::{self, LoadState};
use crate::commands::tunnel::{self as far, Header, Request};
use crate::config::Config;
use crate::devsbd::bridge;
use crate::devsbd::forward::Route;
use crate::serve::api::ApiError;
use crate::serve::endpoint::Stream;
use crate::state::State;

/// How long the hub has to send its request once it opened the stream.
const REQUEST_WAIT: Duration = Duration::from_secs(30);

/// The longest request line taken.
const REQUEST_LIMIT: usize = 64 * 1024;

/// How often a running bridge is checked for its exit and the hub's
/// hang-up.
const WATCH_TICK: Duration = Duration::from_millis(200);

/// The hub's request, its first line: read one byte at a time, so nothing
/// past its newline is taken (there's nothing before the bridge's bytes, but
/// the stream is the bridge's stdin next). Bounded by [`REQUEST_LIMIT`].
pub fn read_request(r: &mut impl Read) -> Result<Request, String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match r.read(&mut byte) {
            Ok(0) => return Err("the stream ended before the tunnel request".into()),
            Ok(_) if byte[0] == b'\n' => return Request::parse(&line),
            Ok(_) if line.len() >= REQUEST_LIMIT => return Err(format!("no tunnel request in the first {REQUEST_LIMIT} bytes")),
            Ok(_) => line.push(byte[0]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(format!("reading the tunnel request: {e}")),
        }
    }
}

/// `state` with only the instances a `TUNNEL` into `root` may reach: lent
/// ones (`lent::lent_instances`' rule, over `reach`) whose config root is
/// under `root` (canonical, already [`confine`]d).
pub fn scoped(mut state: State, root: &Path, reach: &Reach) -> State {
    state.instances.retain(|_, i| i.config_dir.as_deref().is_some_and(|d| reach.admits_recorded(d) && lent::root_under(d, root)));
    state
}

/// The request's root, canonical and confined to `reach` as
/// `lent::confine_path` does (it must exist).
pub fn confine(req: &Request, reach: &Reach) -> Result<PathBuf, ApiError> {
    lent::confine_path(reach, &req.root, true)
}

/// A service forward's own gate, over [`scoped`] `state` and the config at
/// the root: `service` must be one an instance there uses, the named one
/// (as `resolve_instance` takes `instance`) when there is one.
pub fn check_service(config: &Config, state: &State, instance: Option<&str>, service: &str) -> Result<(), String> {
    let key = match instance {
        Some(name) => Some(crate::commands::resolve_instance_noninteractive(state, name).map_err(|e| format!("{e:#}"))?),
        None => None,
    };
    if port::uses_service(config, state, key.as_deref(), service) {
        return Ok(());
    }
    Err(match key {
        Some(key) => format!("instance `{key}` doesn't use service `{service}`"),
        None => format!("no instance here uses service `{service}`"),
    })
}

/// Serve one `TUNNEL` stream, `link` the worker's end, to its end (see the
/// module doc). `files` are this host's (its `state.toml`, and what the
/// peer's [`Reach`] is read from), `peer` the link's. Refusals with
/// `denied` and bridges that can't start are logged.
pub fn serve(link: Stream, files: &Files, peer: &Peer, log: fn(&str)) {
    let _ = link.set_read_timeout(Some(REQUEST_WAIT));
    let request = read_request(&mut &link);
    let _ = link.set_read_timeout(None);
    let route = request.and_then(|req| route(req, files, peer, log));
    let header = Header::of(&route);
    if (&link).write_all(header.line().as_bytes()).is_err() {
        return;
    }
    if let Ok(route) = route {
        match spawn_bridge(&link, &route, log) {
            Ok(child) => watch(&link, child),
            Err(e) => log(&format!("fleet: cannot start a TUNNEL bridge into {} for hub {}: {e}", route.container, peer.name)),
        }
    }
    let _ = link.shutdown(Shutdown::Both);
}

/// The gate, then the route (see the module doc).
fn route(req: Request, files: &Files, peer: &Peer, log: fn(&str)) -> Result<Route, String> {
    let (reach, warning) = Reach::load(files, peer.fleet_root.as_deref());
    if let Some(w) = warning {
        log(&format!("fleet: a TUNNEL stream from hub {}: {w}", peer.name));
    }
    let root = confine(&req, &reach).map_err(|e| {
        if e.code == "denied" {
            log(&format!("fleet: refused a TUNNEL stream from hub {}: {}", peer.name, e.message));
        }
        e.message
    })?;
    let load: LoadState = {
        let (path, root) = (files.state.clone(), root.clone());
        Arc::new(move || State::load_at(&path).map(|s| scoped(s, &root, &reach)).map_err(|e| format!("{e:#}")))
    };
    if let Some(service) = &req.service {
        let config = Config::load(&root).map_err(|e| format!("{e:#}"))?;
        check_service(&config, &load()?, req.instance.as_deref(), service)?;
    }
    far::resolve(root, req.instance, req.service, req.port, load)
}

/// `bridge_command(container)` with `link` as its stdin and stdout; its
/// stderr goes to the daemon's log, line by line.
fn spawn_bridge(link: &Stream, route: &Route, log: fn(&str)) -> std::io::Result<Child> {
    let (stdin, stdout) = (OwnedFd::from(link.try_clone()?), OwnedFd::from(link.try_clone()?));
    let mut child = bridge::bridge_command(&route.container)
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(stderr) = child.stderr.take() {
        let container = route.container.clone();
        let _ = std::thread::Builder::new().name("serve-tunnel-err".into()).spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(stderr).lines().map_while(Result::ok) {
                log(&format!("fleet: TUNNEL bridge into {container}: {line}"));
            }
        });
    }
    Ok(child)
}

/// Until the bridge exits or the hub closes its end (then it's killed),
/// then reap it.
fn watch(link: &Stream, mut child: Child) {
    loop {
        if !matches!(child.try_wait(), Ok(None)) {
            break;
        }
        if crate::serve::daemon::hung_up(link, WATCH_TICK) {
            let _ = child.kill();
            break;
        }
    }
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixStream;

    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dsv-tunnel-w-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    #[test]
    fn the_request_is_one_bounded_line() {
        let req = Request { root: "/f/cfg".into(), instance: Some("web".into()), service: None, port: 3000 };
        let wire = req.line() + "BRIDGE";
        let mut r = wire.as_bytes();
        assert_eq!(read_request(&mut r).unwrap(), req);
        assert_eq!(r, b"BRIDGE", "nothing past the newline is read");
        assert_eq!(read_request(&mut &b"{\"root\":"[..]).unwrap_err(), "the stream ended before the tunnel request");
        let long = "x".repeat(REQUEST_LIMIT + 1);
        assert!(read_request(&mut long.as_bytes()).unwrap_err().starts_with("no tunnel request in the first"));
        assert!(read_request(&mut &b"nope\n"[..]).unwrap_err().starts_with("bad tunnel request"));
    }

    const CONFIG: &str = r#"
[services.db]
image = "postgres"
scope = "isolated"

[services.cache]
image = "redis"
scope = "global"

[sandbox.app]
image = "node"
services = ["db"]

[sandbox.other]
image = "node"
services = ["cache"]
"#;

    /// `state.toml` text: `app-1` (sandbox `app`) and `other-1` lent under
    /// `cfg`, `far` lent under the fleet root's `elsewhere`, `own` (sandbox
    /// `app` too) outside the fleet root.
    fn state_with(base: &Path, root: &Path) -> State {
        let inst = |name: &str, sandbox: &str, dir: &Path| {
            format!(
                "[instance.{name}]\ninstance_id = \"id-{name}\"\nsandbox = \"{sandbox}\"\ncontainer = \"devsandbox-{name}\"\n\
                 folder = \"/w\"\nworkspace = \"/w\"\ncreated_unix = 1\nconfig_dir = {:?}\n",
                dir.to_str().unwrap()
            )
        };
        let text = inst("app-1", "app", &root.join("cfg"))
            + &inst("other-1", "other", &root.join("cfg"))
            + &inst("far", "app", &root.join("elsewhere"))
            + &inst("own", "app", &base.join("mine"));
        let path = base.join("state.toml");
        std::fs::write(&path, text).unwrap();
        State::load_at(&path).unwrap()
    }

    #[test]
    fn the_gate_confines_the_root_and_scopes_the_instances() {
        let b = scratch("gate");
        let root = b.join("fleet/hub");
        for d in ["cfg", "elsewhere"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::create_dir_all(b.join("mine/nested")).unwrap();
        let req = |root: &Path| Request { root: root.to_path_buf(), instance: None, service: Some("db".into()), port: 5432 };
        let r = fleet(&root, &[]);
        assert_eq!(confine(&req(&root.join("cfg")), &r).unwrap(), root.join("cfg"));
        assert_eq!(confine(&req(&b.join("mine")), &r).unwrap_err().code, "denied");
        assert_eq!(confine(&req(&root.join("cfg/../../../mine")), &r).unwrap_err().code, "denied");
        assert_eq!(confine(&req(&root.join("gone")), &r).unwrap_err().code, "not-found");
        assert_eq!(confine(&req(Path::new("rel")), &r).unwrap_err().code, "invalid");
        // An own root, exactly.
        let with_own = fleet(&root, &[&b.join("mine")]);
        assert_eq!(confine(&req(&b.join("mine")), &with_own).unwrap(), b.join("mine"));
        assert_eq!(confine(&req(&b.join("mine/nested")), &with_own).unwrap_err().code, "denied");

        let names = |s: &State| s.instances.keys().cloned().collect::<Vec<_>>();
        assert_eq!(names(&scoped(state_with(&b, &root), &root.join("cfg"), &r)), ["app-1", "other-1"]);
        assert_eq!(names(&scoped(state_with(&b, &root), &root, &r)), ["app-1", "far", "other-1"], "the fleet root holds them all");
        assert!(names(&scoped(state_with(&b, &root), &b.join("mine"), &r)).is_empty(), "not reached: not scoped in");
        assert_eq!(names(&scoped(state_with(&b, &root), &b.join("mine"), &with_own)), ["own"], "an own root's");
        let _ = std::fs::remove_dir_all(&b);
    }

    #[test]
    fn a_service_must_be_one_a_reachable_instance_uses() {
        let b = scratch("service");
        let root = b.join("fleet/hub");
        std::fs::create_dir_all(root.join("cfg")).unwrap();
        let config = Config::parse(CONFIG).unwrap();
        let state = scoped(state_with(&b, &root), &root.join("cfg"), &fleet(&root, &[]));
        assert_eq!(check_service(&config, &state, None, "db"), Ok(()));
        assert_eq!(check_service(&config, &state, Some("app-1"), "db"), Ok(()));
        assert_eq!(check_service(&config, &state, Some("id-app-1"), "db"), Ok(()), "by id");
        assert_eq!(check_service(&config, &state, None, "cache"), Ok(()), "a global one `other-1` uses");
        assert_eq!(check_service(&config, &state, Some("app-1"), "cache").unwrap_err(), "instance `app-1` doesn't use service `cache`");
        assert!(check_service(&config, &state, Some("own"), "db").is_err(), "not lent: out of scope");
        assert!(check_service(&config, &state, Some("far"), "db").is_err(), "another root's");
        let none = scoped(state, &b.join("nothing"), &fleet(&root, &[]));
        assert_eq!(check_service(&config, &none, None, "db").unwrap_err(), "no instance here uses service `db`");
        let _ = std::fs::remove_dir_all(&b);
    }

    /// A `worker` reach of fleet root `root` and own roots `own`.
    fn fleet(root: &Path, own: &[&Path]) -> Reach {
        Reach {
            trust: crate::worker_policy::Trust::Worker,
            fleet_root: Some(root.to_path_buf()),
            own: own.iter().map(|p| p.to_path_buf()).collect(),
        }
    }

    /// The hub's end of a stream served on a thread, its request sent;
    /// what it reads back to the end.
    fn served(peer: &Peer, state: &Path, request: &str) -> String {
        let (hub, ours) = UnixStream::pair().unwrap();
        let (peer, files) = (peer.clone(), Files { state: state.to_path_buf(), member: None, home: None });
        let t = std::thread::spawn(move || serve(ours, &files, &peer, crate::serve::helper::tests::log));
        (&hub).write_all(request.as_bytes()).unwrap();
        hub.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut out = String::new();
        (&hub).read_to_string(&mut out).unwrap();
        t.join().unwrap();
        out
    }

    /// Every refusal is one error header, then the stream ends (no
    /// runtime: each is refused before a route is tried).
    #[test]
    fn refusals_are_one_error_header() {
        let b = scratch("refuse");
        let root = b.join("fleet/hub");
        std::fs::create_dir_all(root.join("cfg")).unwrap();
        std::fs::create_dir_all(b.join("mine")).unwrap();
        std::fs::write(root.join("cfg").join(crate::config::CONFIG_FILE), CONFIG).unwrap();
        state_with(&b, &root);
        let state = b.join("state.toml");
        let scope = Peer { name: "me@hub".into(), fleet_root: Some(root.clone()), env: None };
        let error = |out: &str| match crate::commands::tunnel::read_header(&mut out.as_bytes()) {
            Ok(Header::Error { error }) => {
                assert!(out.ends_with("}\n"), "nothing after the header: {out:?}");
                error
            }
            other => panic!("{other:?}"),
        };
        let cfg = root.join("cfg");
        let line = |instance: Option<&str>, service: Option<&str>, root: &Path| {
            Request { root: root.to_path_buf(), instance: instance.map(Into::into), service: service.map(Into::into), port: 5432 }.line()
        };
        std::fs::create_dir_all(b.join("other")).unwrap();
        let e = error(&served(&scope, &state, &line(Some("own"), None, &b.join("other"))));
        assert!(e.contains("neither under this member's fleet root nor one of its own roots"), "{e}");
        // An own root (`own`'s, from `state.toml`) is reached: here a
        // service no instance of it uses is the refusal.
        std::fs::write(b.join("mine").join(crate::config::CONFIG_FILE), CONFIG).unwrap();
        assert_eq!(error(&served(&scope, &state, &line(Some("own"), Some("cache"), &b.join("mine")))), "instance `own` doesn't use service `cache`");
        assert!(crate::serve::helper::tests::logged("fleet: refused a TUNNEL stream from hub me@hub"));
        assert!(error(&served(&scope, &state, &line(Some("own"), None, &cfg))).contains("own"), "not lent: no such instance");
        assert!(error(&served(&scope, &state, &line(Some("far"), None, &cfg))).contains("far"), "another root's");
        assert_eq!(error(&served(&scope, &state, &line(None, Some("nope"), &cfg))), "no instance here uses service `nope`");
        assert_eq!(
            error(&served(&scope, &state, &line(Some("app-1"), Some("cache"), &cfg))),
            "instance `app-1` doesn't use service `cache`"
        );
        assert_eq!(error(&served(&scope, &state, &line(None, None, &cfg))), "name an instance or a service");
        assert!(error(&served(&scope, &state, "garbage\n")).starts_with("bad tunnel request"));
        let _ = std::fs::remove_dir_all(&b);
    }
}
