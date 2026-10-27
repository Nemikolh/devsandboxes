//! `devsandbox fleet-link --worker <name>` (hidden): the hub's end of a fleet
//! worker's link (docs/architecture/daemon/fleet.md, *Hub side*). A worker's
//! daemon runs it on its hub over its own ssh (`ssh -T -o BatchMode=yes <hub>
//! devsandbox fleet-link --worker <name>`, `serve::fleet::worker`) and answers the
//! streams it opens with connections to its own daemon.
//!
//! It is the opener of a `fleet::link` of `fleet::LINK`, as
//! `commands::agent_relay` is of the agent relay's: it binds
//! `endpoint::fleet_socket` (`fleet-<name>.sock`, 0600 in the 0700 socket
//! dir; the newest link for a worker wins), and each connection there (the
//! hub's link to the offered machine, its forwarded verbs: `fleet::offer::dial`)
//! becomes one `API` stream to the worker's daemon. Once the `Hello`s are
//! through it sends `fleet.offer {worker, version}` to this host's daemon
//! (started like any client's, if it isn't running), which registers the
//! worker as an offered machine, and holds that connection for the link's
//! life, so a linked worker keeps the hub's daemon from idling out. A refused
//! offer (not a hub, a name an ssh machine has) ends the link with why.

use std::io::{self, Read, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::json;

use crate::serve::client::{self, Conn};
use crate::serve::endpoint;
use crate::serve::fleet::{self, link};
use crate::serve::machines::check_name;

/// The CLI entry: the socket dir (created 0700 if missing), then [`run`] on
/// stdin/stdout, offering through this host's daemon (lazy start).
pub fn fleet_link(worker: &str) -> Result<()> {
    check_name(worker)?;
    let dir = endpoint::socket_dir()?;
    endpoint::ensure_private_dir(&dir)?;
    let daemon = dir.clone();
    run(io::stdin(), io::stdout(), &dir, worker, move || {
        client::connect(&daemon, "fleet-link").context("cannot reach this host's devsandbox serve")
    })
}

/// The link over `reader`/`writer` with its socket in `dir`: bind, sentinel
/// and `Hello`s (the peer must serve `API`), then the offer on the
/// connection `daemon` opens, held until the link ends; then accept and
/// relay (`link::opener`). `Ok` when the worker went away.
pub fn run(
    reader: impl Read + Send + 'static,
    writer: impl Write + Send + 'static,
    dir: &Path,
    worker: &str,
    daemon: impl FnOnce() -> Result<Conn>,
) -> Result<()> {
    check_name(worker)?;
    let mut held: Option<Conn> = None;
    let end = link::opener(&fleet::LINK, reader, writer, &endpoint::fleet_socket(dir, worker), |peer| {
        let mut conn = daemon()?;
        offer(&mut conn, worker, &peer.hash)?;
        held = Some(conn);
        Ok(())
    });
    drop(held);
    end
}

/// `fleet.offer` on `conn`; a refusal is an error with the daemon's reason.
fn offer(conn: &mut Conn, worker: &str, version: &str) -> Result<()> {
    let r = conn.call("fleet.offer", json!({ "worker": worker, "version": version }))?;
    if let Some(e) = r.error {
        bail!("offer refused: {}", e.message);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader};
    use std::net::Shutdown;
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use serde_json::Value;

    use super::*;
    use crate::serve::daemon::tests::{opts, scratch, spawn_daemon};
    use crate::serve::daemon::Exit;
    use crate::serve::machines::{self, Entry, ssh};
    use crate::serve::proto::{HelloResult, Version};

    fn wait_for(mut cond: impl FnMut() -> bool, what: &str) {
        let start = Instant::now();
        while !cond() {
            assert!(start.elapsed() < Duration::from_secs(10), "timed out waiting: {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A raw connection past a 0.0.0 hello (never a handoff), retrying
    /// while the daemon binds.
    fn stream_at(dir: &Path) -> endpoint::Stream {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(mut stream) = endpoint::connect(dir) {
                match client::hello(&mut stream, &Version { semver: "0.0.0".into(), build: 0 }, "test").unwrap() {
                    client::Hello::Ready(_) => return stream,
                    other => panic!("{other:?}"),
                }
            }
            assert!(Instant::now() < deadline, "daemon never listened");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn conn_at(dir: &Path) -> Conn {
        let reply = HelloResult { version: "0.0.0".into(), build: 0, protocol: 1, handoff: false };
        Conn::new(stream_at(dir), reply).unwrap()
    }

    /// The worker's end, faked: the answerer of `fleet::LINK` over `io`,
    /// each `API` stream a connection to the daemon in `daemon` that starts
    /// with the API relay's sentinel line, as the worker's
    /// (`daemon::Shared::open_lent`) does.
    fn fake_worker(io: UnixStream, daemon: PathBuf) -> std::thread::JoinHandle<link::End> {
        let (r, w) = (io.try_clone().unwrap(), io);
        std::thread::spawn(move || {
            let open = move || {
                let duplex = ssh::test_support::relayed(endpoint::connect(&daemon).ok()?).ok()?;
                Some(duplex.stream.into())
            };
            link::answerer(&fleet::LINK, r, w, open, || {}, || {})
        })
    }

    /// The hub's `fleet-link` for worker `lap` over `io`, its socket and
    /// daemon in `hub`.
    fn hub_link(io: UnixStream, hub: PathBuf) -> std::thread::JoinHandle<Result<()>> {
        let (r, w) = (io.try_clone().unwrap(), io);
        std::thread::spawn(move || run(r, w, &hub.clone(), "lap", move || Ok(conn_at(&hub))))
    }

    /// A worker linked to its hub: the hub's daemon registers it on offer,
    /// and quick calls and verbs reach the worker's daemon over the link
    /// through the offered dial, `link::handshake` and `forward::verb` as
    /// for an ssh machine.
    #[test]
    fn an_offered_worker_answers_quick_calls_and_verbs_over_its_link() {
        let base = scratch("fleet-e2e");
        let (worker_dir, hub_dir) = (base.join("w"), base.join("hub"));
        let worker_root = base.join("worker-root");
        std::fs::create_dir_all(&worker_root).unwrap();
        std::fs::write(worker_root.join(crate::config::CONFIG_FILE), "[sandbox.web]\nimage = \"a\"\n").unwrap();
        let worker_root = std::fs::canonicalize(&worker_root).unwrap();
        let worker_state = base.join("worker-data/state.toml");
        std::fs::create_dir_all(worker_state.parent().unwrap()).unwrap();
        std::fs::write(
            &worker_state,
            format!(
                "[instance.web-w]\nsandbox = \"web\"\ncontainer = \"devsandbox-web-w\"\nfolder = \"/w\"\n\
                 workspace = \"/w\"\ncreated_unix = 1\nconfig_dir = {:?}\n",
                worker_root.to_str().unwrap()
            ),
        )
        .unwrap();

        let (runner, verb_dir) = crate::serve::verbs::tests::script_runner("fleet-e2e", r#"for a; do n=$a; done; echo "hi $n""#);
        let mut worker = opts(1, 60_000);
        worker.verbs = Some(runner);
        worker.state = Some(worker_state);
        worker.home = Some(base.join("worker-home"));
        worker.machines = Some(base.join("worker-machines.toml"));
        let registry = base.join("machines.toml");
        let mut hub = opts(1, 300);
        hub.hub = true;
        hub.machines = Some(registry.clone());
        hub.state = Some(base.join("hub-data/state.toml"));
        let daemons = [spawn_daemon(&worker_dir, worker), spawn_daemon(&hub_dir, hub)];
        drop(conn_at(&worker_dir));

        let (worker_io, hub_io) = UnixStream::pair().unwrap();
        let worker_end = fake_worker(worker_io, worker_dir.clone());
        let hub_end = hub_link(hub_io.try_clone().unwrap(), hub_dir.clone());
        wait_for(|| machines::load(&registry).is_ok_and(|e| e == [Entry::offered("lap")]), "the offer registers `lap`");
        assert!(endpoint::fleet_socket(&hub_dir, "lap").exists());

        let mut c = conn_at(&hub_dir);
        // A quick call: the worker's answer, over the link.
        let r = c.call("instances.resolve", json!({"name": "web-w", "machine": "lap"})).unwrap();
        assert_eq!(r.result, Some(json!({"matches": ["web-w"], "found": [{"name": "web-w", "machine": null, "root": worker_root, "sandbox": "web"}]})), "{r:?}");
        let rows = c.call("machines.list", Value::Null).unwrap().result.unwrap();
        assert_eq!((&rows[0]["name"], &rows[0]["kind"], &rows[0]["target"]), (&json!("lap"), &json!("offered"), &Value::Null));
        assert_eq!(rows[0]["state"], "connected");
        assert_eq!(rows[0]["version"], "0.6.0", "the worker daemon's hello");

        // A verb: its own `API` stream, streamed back under this request's id.
        let stream = stream_at(&hub_dir);
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let params = json!({"root": verb_dir, "env": {}, "instance": "web-w", "machine": "lap"});
        use std::io::Write as _;
        writeln!(&stream, "{}", json!({"id": 5, "method": "stop", "params": params})).unwrap();
        let mut out = String::new();
        let answer = loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).unwrap() > 0, "closed before the answer");
            let msg: Value = serde_json::from_str(&line).unwrap();
            if msg.get("id").is_some() {
                break msg;
            }
            if msg["method"] == "output" && msg["params"]["stream"] == "stdout" {
                assert_eq!(msg["params"]["request"], 5);
                out.push_str(msg["params"]["data"].as_str().unwrap());
            }
        };
        assert!(answer["error"].is_null(), "{answer}");
        assert_eq!(out, "hi web-w\n");
        drop((reader, stream, c));

        // The worker going away ends the hub's end, which unlinks its socket.
        hub_io.shutdown(Shutdown::Both).unwrap();
        hub_end.join().unwrap().unwrap();
        assert!(!endpoint::fleet_socket(&hub_dir, "lap").exists());
        assert!(matches!(worker_end.join().unwrap(), link::End::Closed));
        assert!(conn_at(&worker_dir).call("shutdown", json!({})).unwrap().error.is_none());
        let [worker, hub] = daemons;
        assert_eq!(worker.join().unwrap().unwrap(), Exit::Shutdown);
        assert_eq!(hub.join().unwrap().unwrap(), Exit::Idle);
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&verb_dir);
    }

    /// A daemon that isn't a hub refuses the offer: the link ends with why,
    /// its socket gone, nothing registered.
    #[test]
    fn a_refused_offer_ends_the_link() {
        let base = scratch("fleet-nohub");
        let hub_dir = base.join("hub");
        let registry = base.join("machines.toml");
        let mut not_hub = opts(1, 300);
        not_hub.machines = Some(registry.clone());
        let daemon = spawn_daemon(&hub_dir, not_hub);
        drop(conn_at(&hub_dir));
        let (worker_io, hub_io) = UnixStream::pair().unwrap();
        let worker_end = fake_worker(worker_io, base.join("nowhere"));
        let err = hub_link(hub_io, hub_dir.clone()).join().unwrap().unwrap_err();
        assert!(format!("{err:#}").contains("offer refused: this devsandbox isn't a fleet hub"), "{err:#}");
        assert!(!endpoint::fleet_socket(&hub_dir, "lap").exists());
        assert!(!registry.exists());
        assert!(matches!(worker_end.join().unwrap(), link::End::Closed));
        assert_eq!(daemon.join().unwrap().unwrap(), Exit::Idle);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_bad_worker_name_is_refused_before_binding() {
        let (a, b) = UnixStream::pair().unwrap();
        let dir = std::env::temp_dir();
        let err = run(a, b, &dir, "a/b", || unreachable!()).unwrap_err();
        assert!(format!("{err:#}").contains("machine name"), "{err:#}");
    }
}
