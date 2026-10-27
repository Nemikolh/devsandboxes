//! The worker side of a fleet link (docs/architecture/daemon/fleet.md,
//! *Worker side*): while `lend.toml` says `on` or `drain`, the daemon's
//! `serve-lend` thread holds `ssh -T -o BatchMode=yes <hub> devsandbox
//! fleet-link --worker <name>` ([`link_argv`]) and runs the answerer of
//! `fleet::LINK` on its stdio. Each `API` stream the hub opens is answered
//! by the daemon's `open` (a socket pair: the API relay's sentinel on the
//! hub's end, the other end served as a `Worker` connection, `fleet::lent`).
//! A link that ends is retried with the machine links' backoff
//! (`service::backoff`), reset once a link came up. [`Lending::status`] is
//! the link's state for `lend.get`; [`Lending::set_draining`] changes what
//! the next link offers (`--drain`) without dropping the current one.
//!
//! The transport is injectable ([`Transport`]): production spawns ssh
//! (`ssh::spawn`), tests hand back one end of a socket pair whose other end
//! is the hub's `fleet-link` in-process.

use std::ffi::OsString;
use std::net::Shutdown;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};

use super::{LINK, link};
use crate::devsbd::mux::Conn;
use crate::serve::endpoint::Stream;
use crate::serve::machines::check_name;
use crate::serve::machines::service::{self, Timing};
use crate::serve::machines::ssh::{self, Duplex};

/// How the worker reaches its hub: the argv in, a connection out (ssh's
/// stdio behind a socket, as `ssh::spawn` returns it).
pub type Transport = Arc<dyn Fn(&[OsString]) -> Result<Duplex> + Send + Sync>;

/// The production [`Transport`]: ssh.
pub fn ssh_transport() -> Transport {
    Arc::new(|argv: &[OsString]| ssh::spawn(argv))
}

/// How the daemon answers one `API` stream: the hub's end of a new
/// connection, `None` to refuse it.
pub type Open = Arc<dyn Fn() -> Option<Conn> + Send + Sync>;

/// `ssh -T -o BatchMode=yes [-p N] -- <hub> <devsandbox fleet-link --worker
/// <name>>` through the hub's login shell, as every machine argv is
/// (`ssh::remote_command`). `BatchMode`: the daemon has no TTY.
/// `draining` adds `--drain`: the offer says this worker takes no new work.
pub fn link_argv(hub: &str, name: &str, draining: bool) -> Result<Vec<OsString>> {
    check_name(name)?;
    let mut args = vec!["fleet-link", "--worker", name];
    if draining {
        args.push("--drain");
    }
    ssh::target_argv(hub, &["-T", "-o", "BatchMode=yes"], "devsandbox", &args)
}

/// The link's state, for `lend.get`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// The first attempt hasn't come up (or failed) yet.
    Connecting,
    /// The `Hello`s are through.
    Connected,
    /// The last attempt failed or the link ended: why. The next one is
    /// after the backoff.
    Retrying(String),
}

/// The lending thread. Dropping it stops it: the link's stream is shut
/// (which ends the answerer) and the thread joined; the ssh dies with its
/// `Duplex`.
pub struct Lending {
    ctl: Arc<Ctl>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct Ctl {
    stopped: Mutex<bool>,
    wake: Condvar,
    /// The live link's stream, to shut on stop.
    current: Mutex<Option<Stream>>,
    /// What the next link offers: `--drain`.
    draining: AtomicBool,
    status: Mutex<Option<Status>>,
}

impl Lending {
    /// Start lending to `hub` as `name` (`draining`: offering `--drain`).
    pub fn spawn(
        hub: String,
        name: String,
        draining: bool,
        transport: Transport,
        open: Open,
        timing: Timing,
        log: fn(&str),
    ) -> Result<Lending> {
        link_argv(&hub, &name, draining)?;
        let ctl = Arc::new(Ctl::default());
        ctl.draining.store(draining, Ordering::SeqCst);
        let thread = {
            let ctl = Arc::clone(&ctl);
            std::thread::Builder::new()
                .name("serve-lend".into())
                .spawn(move || lend_loop(&hub, &name, &transport, &open, &timing, &ctl, log))
                .context("cannot start the lending thread")?
        };
        Ok(Lending { ctl, thread: Some(thread) })
    }

    pub fn status(&self) -> Status {
        self.ctl.status.lock().unwrap().clone().unwrap_or(Status::Connecting)
    }

    /// What the next link offers; the current one stays up (the hub learns
    /// it when the link is next made).
    pub fn set_draining(&self, draining: bool) {
        self.ctl.draining.store(draining, Ordering::SeqCst);
    }
}

impl Drop for Lending {
    fn drop(&mut self) {
        *self.ctl.stopped.lock().unwrap() = true;
        self.ctl.wake.notify_all();
        if let Some(s) = self.ctl.current.lock().unwrap().take() {
            let _ = s.shutdown(Shutdown::Both);
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Ctl {
    fn stopped(&self) -> bool {
        *self.stopped.lock().unwrap()
    }

    fn set_status(&self, status: Status) {
        *self.status.lock().unwrap() = Some(status);
    }

    /// Sleep `d` unless stopped first; `false` once stopped.
    fn wait(&self, d: Duration) -> bool {
        let guard = self.stopped.lock().unwrap();
        let (guard, _) = self.wake.wait_timeout_while(guard, d, |stopped| !*stopped).unwrap();
        !*guard
    }
}

fn lend_loop(hub: &str, name: &str, transport: &Transport, open: &Open, timing: &Timing, ctl: &Ctl, log: fn(&str)) {
    let mut failures = 0;
    while !ctl.stopped() {
        let argv = match link_argv(hub, name, ctl.draining.load(Ordering::SeqCst)) {
            Ok(argv) => argv,
            Err(e) => {
                // Checked by `spawn`: unreachable short of a bug.
                log(&format!("fleet: {e:#}"));
                return;
            }
        };
        match transport(&argv) {
            Err(e) => {
                log(&format!("fleet: cannot reach hub {hub}: {e:#}"));
                ctl.set_status(Status::Retrying(format!("{e:#}")));
            }
            Ok(duplex) => {
                if run_link(hub, name, duplex, open, ctl, log) {
                    failures = 0;
                }
            }
        }
        if !ctl.wait(service::backoff(timing, &mut failures)) {
            break;
        }
    }
}

/// One link over `duplex` until it ends; `true` if it came up.
fn run_link(hub: &str, name: &str, mut duplex: Duplex, open: &Open, ctl: &Ctl, log: fn(&str)) -> bool {
    let (Ok(reader), Ok(writer), Ok(killer)) = (duplex.stream.try_clone(), duplex.stream.try_clone(), duplex.stream.try_clone())
    else {
        log("fleet: cannot clone the link's socket");
        return false;
    };
    {
        let mut current = ctl.current.lock().unwrap();
        if ctl.stopped() {
            return false;
        }
        *current = duplex.stream.try_clone().ok();
    }
    let up = AtomicBool::new(false);
    let end = link::answerer(
        &LINK,
        reader,
        writer,
        || open(),
        || {
            up.store(true, Ordering::Relaxed);
            ctl.set_status(Status::Connected);
            log(&format!("fleet: lending to {hub} as {name}"));
        },
        move || {
            let _ = killer.shutdown(Shutdown::Both);
        },
    );
    ctl.current.lock().unwrap().take();
    if !ctl.stopped() {
        let why = match end {
            link::End::Handshake(why) => why,
            link::End::Dead => "no frame from the hub: link dead".into(),
            link::End::Closed => "link closed".into(),
        };
        let tail = duplex.peer.stderr_tail(Duration::from_millis(500));
        let why = format!("{why}{}", if tail.is_empty() { String::new() } else { format!(" (ssh: {tail})") });
        log(&format!("fleet: link to {hub} ended: {why}"));
        ctl.set_status(Status::Retrying(why));
    }
    duplex.peer.close();
    up.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_link_argv() {
        let argv = link_argv("ssh://me@hub:2200", "lap", false).unwrap();
        let want = [
            "ssh",
            "-T",
            "-o",
            "BatchMode=yes",
            "-p",
            "2200",
            "--",
            "me@hub",
            r#"exec "$SHELL" -lc 'exec devsandbox '\''fleet-link'\'' '\''--worker'\'' '\''lap'\'''"#,
        ];
        assert_eq!(argv, want.map(OsString::from));
        assert_eq!(link_argv("hub", "lap", false).unwrap()[4..6], [OsString::from("--"), "hub".into()]);
        let drain = link_argv("hub", "lap", true).unwrap();
        assert!(drain[6].to_string_lossy().ends_with(r#"'\''--drain'\'''"#), "{drain:?}");
        assert!(link_argv("-oProxyCommand=x", "lap", false).is_err());
        assert!(link_argv("hub", "a/b", false).is_err());
    }

    mod e2e {
        use std::collections::BTreeMap;
        use std::io::{BufRead, BufReader, Write as _};
        use std::os::unix::net::UnixStream;
        use std::path::{Path, PathBuf};
        use std::time::Instant;

        use serde_json::{Value, json};

        use super::*;
        use crate::serve::client::{self, Hello};
        use crate::serve::daemon::Exit;
        use crate::serve::daemon::tests::{opts, scratch, spawn_daemon};
        use crate::serve::endpoint;
        use crate::serve::fleet::lend::{self, Lend};
        use crate::serve::machines::{self, Entry, link as mlink};
        use crate::serve::proto::{HelloResult, Version};

        fn wait_for(mut cond: impl FnMut() -> bool, what: &str) {
            let start = Instant::now();
            while !cond() {
                assert!(start.elapsed() < Duration::from_secs(10), "timed out waiting: {what}");
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        /// A raw connection past a hello as `version`, retrying while the
        /// daemon binds.
        fn stream_at(dir: &Path) -> endpoint::Stream {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Ok(mut stream) = endpoint::connect(dir) {
                    match client::hello(&mut stream, &Version { semver: "0.0.0".into(), build: 0 }, "test").unwrap() {
                        Hello::Ready(_) => return stream,
                        other => panic!("{other:?}"),
                    }
                }
                assert!(Instant::now() < deadline, "daemon never listened");
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        fn conn_at(dir: &Path) -> client::Conn {
            let reply = HelloResult { version: "0.0.0".into(), build: 0, protocol: 1, handoff: false };
            client::Conn::new(stream_at(dir), reply).unwrap()
        }

        /// A transport whose every link is the hub's `fleet-link` for `lap`
        /// in-process, its socket and daemon in `hub`.
        fn in_process_hub(hub: PathBuf) -> Transport {
            Arc::new(move |argv: &[OsString]| {
                let words: Vec<_> = argv.iter().map(|a| a.to_string_lossy().into_owned()).collect();
                assert_eq!(&words[..5], ["ssh", "-T", "-o", "BatchMode=yes", "--"]);
                assert_eq!(words[5], "me@hub");
                assert!(words[6].contains("fleet-link") && words[6].contains("lap"), "{}", words[6]);
                let (worker, hub_io) = UnixStream::pair()?;
                let hub = hub.clone();
                std::thread::spawn(move || {
                    let (r, w) = (hub_io.try_clone().unwrap(), hub_io);
                    crate::commands::fleet_link::run(r, w, &hub.clone(), "lap", false, move || Ok(conn_at(&hub)))
                });
                Ok(Duplex::socket(worker))
            })
        }

        /// A raw stream over the hub's offered dial to `lap`, past the
        /// API relay's sentinel line.
        fn offered_stream(hub_dir: &Path) -> endpoint::Stream {
            let dial = crate::serve::fleet::offer::dial(hub_dir.to_path_buf(), Arc::new(|_: &Entry| unreachable!()));
            let duplex = dial(&Entry::offered("lap")).unwrap();
            let mut reader = BufReader::new(duplex.stream.try_clone().unwrap());
            duplex.stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut line = String::new();
            while line != "devsandbox-api 1\n" {
                line.clear();
                assert!(reader.read_line(&mut line).unwrap() > 0, "no sentinel");
            }
            assert!(reader.buffer().is_empty());
            duplex.stream
        }

        /// A worker lending to an in-process hub: its offer registers it,
        /// and through the hub's offered dial the lent API answers inside
        /// the fleet root and nowhere else.
        #[test]
        fn a_lending_worker_serves_its_hub_the_lent_api_only() {
            let base = scratch("fleet-lend");
            let (worker_dir, hub_dir) = (base.join("w"), base.join("hub"));
            let data = base.join("worker-data");
            let fleet_root = lend::fleet_root(&data, "me@hub").unwrap();
            let cfg = fleet_root.join("cfg");
            std::fs::create_dir_all(&cfg).unwrap();
            std::fs::write(cfg.join(crate::config::CONFIG_FILE), "[sandbox.web]\nimage = \"a\"\n").unwrap();
            let mine = base.join("mine");
            std::fs::create_dir_all(&mine).unwrap();
            std::fs::write(mine.join(crate::config::CONFIG_FILE), "[sandbox.web]\nimage = \"a\"\n").unwrap();
            let (cfg, mine) = (std::fs::canonicalize(&cfg).unwrap(), std::fs::canonicalize(&mine).unwrap());
            let inst = |name: &str, dir: &Path| {
                format!(
                    "[instance.{name}]\nsandbox = \"web\"\ncontainer = \"devsandbox-{name}\"\nfolder = \"/w\"\n\
                     workspace = \"/w\"\ncreated_unix = 1\nconfig_dir = {:?}\n",
                    dir.to_str().unwrap()
                )
            };
            let worker_state = data.join("state.toml");
            std::fs::write(&worker_state, inst("web-w", &cfg) + &inst("own", &mine)).unwrap();
            let lend_path = data.join("lend.toml");
            lend::write_for_test(&lend_path, &Lend {
                state: lend::State::On,
                hub: "me@hub".into(),
                name: "lap".into(),
                ..Default::default()
            });

            let (runner, verb_dir) =
                crate::serve::verbs::tests::script_runner("fleet-lend", r#"for a; do n=$a; done; echo "hi $n path=$PATH""#);
            let mut worker = opts(1, 300);
            worker.verbs = Some(runner);
            worker.state = Some(worker_state);
            worker.home = Some(base.join("worker-home"));
            worker.machines = Some(base.join("worker-machines.toml"));
            worker.lend = Some(lend_path.clone());
            worker.lend_transport = Some(in_process_hub(hub_dir.clone()));
            worker.lend_env = Some(BTreeMap::from([("PATH".into(), "/worker/bin".into())]));
            let registry = base.join("machines.toml");
            let mut hub = opts(1, 300);
            hub.hub = true;
            hub.machines = Some(registry.clone());
            hub.state = Some(base.join("hub-data/state.toml"));
            let hub_daemon = spawn_daemon(&hub_dir, hub);
            drop(conn_at(&hub_dir));
            let worker_daemon = spawn_daemon(&worker_dir, worker);

            wait_for(|| machines::load(&registry).is_ok_and(|e| e == [Entry::offered("lap")]), "the offer registers `lap`");
            wait_for(|| endpoint::fleet_socket(&hub_dir, "lap").exists(), "the hub's fleet socket");
            // Lending keeps the worker up past its 300 ms idle timeout.
            std::thread::sleep(Duration::from_millis(600));
            assert!(!worker_daemon.is_finished(), "a lending daemon doesn't idle out");

            // The hub's own link to it: `machines::link::handshake` as for
            // an ssh machine.
            let dial = crate::serve::fleet::offer::dial(hub_dir.clone(), Arc::new(|_: &Entry| unreachable!()));
            let mut link = mlink::handshake("lap", dial(&Entry::offered("lap")).unwrap(), Duration::from_secs(5)).unwrap();
            assert_eq!(link.remote().version, "0.6.0");
            let call = |link: &mut mlink::Link, m: &str, p: Value| link.conn.call(m, p).unwrap();
            let r = call(&mut link, "sandboxes.list", json!({"root": cfg}));
            assert_eq!(r.result.unwrap()[0]["name"], "web", "inside the fleet root");
            let r = call(&mut link, "sandboxes.list", json!({"root": mine}));
            assert_eq!(r.error.unwrap().code, "denied", "outside it");
            let r = call(&mut link, "sandboxes.list", json!({"root": cfg.join("../../../../mine")}));
            assert_eq!(r.error.unwrap().code, "denied", "`..`");
            for (m, p) in [("machines.list", json!({})), ("shutdown", json!({})), ("roots.list", json!({}))] {
                assert_eq!(call(&mut link, m, p).error.unwrap().code, "denied", "{m}");
            }
            // `web` matches both instances; the hub sees its own only.
            let r = call(&mut link, "instances.resolve", json!({"name": "web"}));
            assert_eq!(r.result.unwrap()["matches"], json!(["web-w"]));
            assert_eq!(call(&mut link, "instances.done", json!({"instance": "own", "done": true})).error.unwrap().code, "not-found");
            drop(link);

            // Through the hub as any client would: a quick call forwarded.
            let mut c = conn_at(&hub_dir);
            let r = c.call("instances.resolve", json!({"name": "web", "machine": "lap"})).unwrap();
            assert_eq!(r.result.unwrap()["found"], json!([{"name": "web-w", "machine": null, "root": cfg, "sandbox": "web"}]));
            let r = c.call("sandboxes.list", json!({"root": mine, "machine": "lap"})).unwrap();
            assert_eq!(r.error.unwrap().code, "denied");

            // A verb: the worker's env, not the hub's.
            let stream = stream_at(&hub_dir);
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let params = json!({"root": cfg, "env": {"PATH": "/hub/bin"}, "instance": "web-w", "machine": "lap"});
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
                    out.push_str(msg["params"]["data"].as_str().unwrap());
                }
            };
            assert!(answer["error"].is_null(), "{answer}");
            assert_eq!(out, "hi web-w path=/worker/bin\n");
            drop((reader, stream, c));

            // A newer hello from the hub never hands the worker off.
            let mut raw = offered_stream(&hub_dir);
            let newer = Version { semver: "99.0.0".into(), build: 99 };
            match client::hello(&mut raw, &newer, "hub").unwrap() {
                Hello::Ready(r) => assert!(!r.handoff),
                other => panic!("{other:?}"),
            }
            drop(raw);
            assert!(!worker_daemon.is_finished());

            // `lend.*` on a local connection: the link's state, and a change
            // applied at once (off drops the link, on brings it back).
            let mut local = conn_at(&worker_dir);
            let info = local.call("lend.get", Value::Null).unwrap().result.unwrap();
            assert_eq!((&info["state"], &info["link"], &info["name"]), (&json!("on"), &json!("connected"), &json!("lap")));
            assert_eq!(info["fleet_root"], json!(std::fs::canonicalize(&fleet_root).unwrap()));
            let off = local.call("lend.set", json!({"state": "off"})).unwrap().result.unwrap();
            assert_eq!(off["link"], "off");
            wait_for(|| !endpoint::fleet_socket(&hub_dir, "lap").exists(), "off: the hub's socket gone");
            assert_eq!(lend::load(&lend_path).unwrap().unwrap().state, lend::State::Off, "written");
            local.call("lend.set", json!({"state": "drain"})).unwrap().result.unwrap();
            wait_for(|| endpoint::fleet_socket(&hub_dir, "lap").exists(), "drain: linked again");
            wait_for(
                || local.call("lend.get", Value::Null).unwrap().result.unwrap()["link"] == "connected",
                "drain: connected",
            );
            drop(local);

            // A local client stops the worker; its link goes with it, which
            // ends the hub's end and unlinks its socket.
            assert!(conn_at(&worker_dir).call("shutdown", json!({})).unwrap().error.is_none());
            assert_eq!(worker_daemon.join().unwrap().unwrap(), Exit::Shutdown);
            wait_for(|| !endpoint::fleet_socket(&hub_dir, "lap").exists(), "the hub's socket gone");
            assert_eq!(hub_daemon.join().unwrap().unwrap(), Exit::Idle);
            let _ = std::fs::remove_dir_all(&base);
            let _ = std::fs::remove_dir_all(&verb_dir);
        }
    }

    #[test]
    fn a_failing_transport_is_retried_until_dropped() {
        let tries = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&tries);
        let transport: Transport = Arc::new(move |_: &[OsString]| {
            counted.fetch_add(1, Ordering::SeqCst);
            anyhow::bail!("no route")
        });
        let timing = Timing { backoff: Duration::from_millis(5), backoff_cap: Duration::from_millis(10), ..Timing::default() };
        let lending = Lending::spawn("hub".into(), "lap".into(), false, transport, Arc::new(|| None), timing, |_| {}).unwrap();
        let start = std::time::Instant::now();
        while tries.load(Ordering::SeqCst) < 3 {
            assert!(start.elapsed() < Duration::from_secs(5), "retried");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(lending.status(), Status::Retrying("no route".into()));
        drop(lending);
        let n = tries.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(tries.load(Ordering::SeqCst), n, "stopped");
    }
}
