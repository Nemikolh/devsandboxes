//! The requester's side of routed streams (docs/architecture/daemon/fleet.md,
//! *Routed streams*; fleet plan step 10b.5, decision 21): a
//! `route-<member>.sock` (`endpoint::route_socket`) for each member of this
//! host's link graph it has no link up to ([`wanted`]), bound and unbound
//! as the graph and the links move by the `serve-routes` thread
//! ([`Routes`]). Each connection there names its channel in its first
//! byte, as on a `fleet-<member>.sock`; the daemon opens it as a routed
//! stream over the graph's paths in order ([`first_open`]), each attempt
//! within [`ATTEMPT`], the next path's turn at a refusal, a timeout or a
//! failed connect, and splices the connection to the first that opens.
//! When none does, an `API` client reads why as the answer to its hello
//! ([`refuse_api`]); any other channel's connection is closed.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Result;

use super::graph::Graph;
use super::link::Socket;
use super::membership::Stored;
use super::proof;
use crate::serve::endpoint;
use crate::serve::proto::{MAX_LINE, Response};

/// The most one path's attempt may take (its opening's reads together):
/// a stalled hop costs this much before the next path is tried, so a few
/// attempts still fit a client's hello wait (`service::Timing::handshake`,
/// which bounds them all together).
pub const ATTEMPT: Duration = Duration::from_secs(5);

/// How often the accept loop looks up for a change or a stop.
const POLL: Duration = Duration::from_millis(200);

/// How long a refused `API` client has to send its hello before it's
/// answered anyway.
const HELLO_WAIT: Duration = Duration::from_secs(2);

/// The members to bind a route socket for: every member `graph` names
/// that `stored` knows (a log member or a held introduction), but `me`
/// and those `direct` says this host has a link up to (their
/// `fleet-<name>.sock` is there).
pub fn wanted(graph: &Graph, stored: &Stored, me: &str, direct: impl Fn(&str) -> bool) -> BTreeSet<String> {
    graph.members().into_iter().filter(|m| m != me && proof::resolve(stored, m).is_some() && !direct(m)).collect()
}

/// `paths` tried in order (decision 21): one whose first hop `up` says has
/// no link up here is skipped, the others `open`ed until one is `Ok`.
/// Every path's failure otherwise, in order (the skipped ones' too).
pub fn first_open<T>(paths: &[Vec<String>], up: impl Fn(&str) -> bool, mut open: impl FnMut(&[String]) -> Result<T>) -> Result<T, Vec<String>> {
    let mut failed = Vec::new();
    for path in paths {
        let Some(first) = path.first() else { continue };
        let via = path.join(" → ");
        if !up(first) {
            failed.push(format!("via {via}: no link to `{first}` is up here"));
            continue;
        }
        match open(path) {
            Ok(stream) => return Ok(stream),
            Err(e) => failed.push(format!("via {via}: {e:#}")),
        }
    }
    Err(failed)
}

/// Where a routed stream to `dest` goes first now (pure): the first hop
/// of the first of `paths` whose first hop `up` says is a link up here (the
/// one [`first_open`] tries first), else why none is. What a routed
/// machine's row says (`snapshots::Routed`): a path that's there may still
/// fail to open past its first hop.
pub fn reach(dest: &str, paths: &[Vec<String>], up: impl Fn(&str) -> bool) -> Result<String, String> {
    if let Some(hop) = paths.iter().filter_map(|p| p.first()).find(|h| up(h)) {
        return Ok(hop.clone());
    }
    if paths.is_empty() {
        return Err(failure(dest, &[]));
    }
    Err(format!("no path to `{dest}` starts with a link up here"))
}

/// Why no routed stream to `dest` opened, from [`first_open`]'s failures.
pub fn failure(dest: &str, failed: &[String]) -> String {
    if failed.is_empty() {
        return format!("no path to `{dest}` in this host's link graph");
    }
    format!("no path to `{dest}` opened: {}", failed.join("; "))
}

/// Answer a refused `API` client on `client` with `why`: its hello line
/// read first (within [`HELLO_WAIT`]: answered before it wrote it, its
/// write would fail and the answer be lost), then one error response with
/// its id, `failed`.
pub fn refuse_api(client: &UnixStream, why: &str) {
    let _ = client.set_read_timeout(Some(HELLO_WAIT));
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while (line.len() as u64) < MAX_LINE {
        match (&mut &*client).read(&mut byte) {
            Ok(1) if byte[0] != b'\n' => line.push(byte[0]),
            // A timed read fails with it even under `SA_RESTART`.
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            _ => break,
        }
    }
    let id = serde_json::from_slice::<serde_json::Value>(&line).ok().and_then(|v| v["id"].as_u64());
    if let Ok(mut out) = serde_json::to_vec(&Response::err(id, "failed", why)) {
        out.push(b'\n');
        let _ = (&mut &*client).write_all(&out);
    }
    let _ = client.shutdown(std::net::Shutdown::Write);
}

/// What the `serve-routes` thread works with.
pub struct Io {
    /// The socket dir: where the route sockets are bound.
    pub dir: PathBuf,
    /// The members to bind for now ([`wanted`] over the daemon's graph and
    /// links), read at each change.
    pub wanted: Box<dyn Fn() -> BTreeSet<String> + Send>,
    /// One connection to member `name`'s route socket: the daemon's, which
    /// must return fast (it reads the channel byte on a thread of its own).
    pub accept: Box<dyn Fn(&str, UnixStream) + Send>,
    pub log: fn(&str),
}

#[derive(Default)]
struct State {
    dirty: bool,
    stop: bool,
}

#[derive(Default)]
struct Inner {
    state: Mutex<State>,
    cv: Condvar,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Tells the `serve-routes` thread to look at the graph and links again.
#[derive(Clone)]
pub struct Poke(Arc<Inner>);

impl Poke {
    /// Doesn't block.
    pub fn poke(&self) {
        self.0.lock().dirty = true;
        self.0.cv.notify_all();
    }
}

/// The `serve-routes` thread, owned by the daemon's `run`; dropping it
/// stops and joins it, its sockets unlinked.
pub struct Routes {
    inner: Arc<Inner>,
    thread: Option<JoinHandle<()>>,
}

impl Routes {
    /// Every `route-*.sock` left in `io.dir` removed (the caller holds the
    /// daemon's start lock: they're a dead daemon's), then the thread.
    pub fn spawn(io: Io) -> Routes {
        sweep(&io.dir);
        let inner = Arc::new(Inner { state: Mutex::new(State { dirty: true, stop: false }), cv: Condvar::new() });
        let i = Arc::clone(&inner);
        let thread = std::thread::Builder::new().name("serve-routes".into()).spawn(move || run(&i, &io));
        let thread = match thread {
            Ok(t) => Some(t),
            Err(e) => {
                crate::serve::daemon::log(&format!("cannot start the route sockets' thread: {e}"));
                None
            }
        };
        Routes { inner, thread }
    }

    pub fn poke(&self) -> Poke {
        Poke(Arc::clone(&self.inner))
    }
}

impl Drop for Routes {
    fn drop(&mut self) {
        self.inner.lock().stop = true;
        self.inner.cv.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The route sockets' files in `dir` removed.
fn sweep(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(endpoint::ROUTE_PREFIX) && name.ends_with(".sock") {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

fn run(inner: &Inner, io: &Io) {
    let mut bound: BTreeMap<String, Socket> = BTreeMap::new();
    loop {
        let dirty = {
            let mut st = inner.lock();
            if st.stop {
                return;
            }
            if bound.is_empty() && !st.dirty {
                // Nothing to accept on: wait for a change.
                st = inner.cv.wait_timeout(st, Duration::from_secs(1)).unwrap_or_else(|e| e.into_inner()).0;
            }
            std::mem::take(&mut st.dirty)
        };
        if dirty {
            reconcile(&mut bound, (io.wanted)(), &io.dir, io.log);
        }
        if bound.is_empty() {
            continue;
        }
        let (names, listeners): (Vec<&String>, Vec<_>) = bound.iter().map(|(n, s)| (n, &s.listener)).unzip();
        match endpoint::accept_any(&listeners, POLL) {
            Ok(Some((i, conn))) => (io.accept)(names[i], conn),
            Ok(None) => {}
            Err(e) => {
                (io.log)(&format!("fleet: route sockets: accept failed: {e}"));
                std::thread::sleep(POLL);
            }
        }
    }
}

/// `bound` made `wanted`'s: the others dropped (unlinked), the missing
/// bound.
fn reconcile(bound: &mut BTreeMap<String, Socket>, wanted: BTreeSet<String>, dir: &Path, log: fn(&str)) {
    bound.retain(|name, _| wanted.contains(name));
    for name in wanted {
        if bound.contains_key(&name) {
            continue;
        }
        match Socket::bind(&endpoint::route_socket(dir, &name)) {
            Ok(socket) => {
                bound.insert(name, socket);
            }
            Err(e) => log(&format!("fleet: cannot bind the route socket for {name}: {e:#}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use anyhow::bail;

    use super::*;
    use crate::serve::fleet::graph::{LinkReport, Report, sign_report};
    use crate::serve::fleet::identity::{Signer, SigningKey};
    use crate::serve::fleet::membership::{Log, Op};
    use crate::serve::snapshots::LinkState;
    use crate::worker_policy::Trust;

    fn key(n: u8) -> SigningKey {
        SigningKey::from_seed([n; 32])
    }

    fn path(p: &[&str]) -> Vec<String> {
        p.iter().map(|s| s.to_string()).collect()
    }

    /// `lap` (1, the authority), `k` (2, the hub), `b` (3), `m` (4).
    fn stored() -> Stored {
        let mut log = Log::default();
        log.append(Op::genesis_with_id([1; 16], "lap", key(1).public_key(), "k"), "lap", &key(1)).unwrap();
        for (name, k) in [("k", 2), ("b", 3), ("m", 4)] {
            log.append(Op::Add { name: name.into(), key: key(k).public_key() }, "lap", &key(1)).unwrap();
        }
        Stored { log, introductions: vec![] }
    }

    fn report(s: &Stored, graph: &mut Graph, member: &str, k: u8, peers: &[&str]) {
        let links = peers.iter().map(|p| LinkReport { peer: p.to_string(), dials: true, state: LinkState::Connected, since_ms: 1 }).collect();
        let r = Report { member: member.into(), trust: Trust::default(), always_on: false, links, agent: vec![] };
        let genesis = s.log.membership().unwrap().genesis;
        let (st, r) = sign_report(&key(k), &genesis, &r, 0, 10);
        graph.apply(&st, &r, s).unwrap();
    }

    #[test]
    fn reach_is_the_first_path_whose_first_hop_is_up() {
        let paths = [path(&["b"]), path(&["m", "b"]), path(&["k", "b"])];
        assert_eq!(reach("b", &paths, |h| h == "k"), Ok("k".into()));
        assert_eq!(reach("b", &paths, |h| h == "m" || h == "k"), Ok("m".into()));
        assert_eq!(reach("b", &paths, |_| false), Err("no path to `b` starts with a link up here".into()));
        assert_eq!(reach("b", &[], |_| true), Err("no path to `b` in this host's link graph".into()));
    }

    #[test]
    fn route_sockets_are_for_known_members_without_a_link_up() {
        let s = stored();
        let mut g = Graph::default();
        report(&s, &mut g, "lap", 1, &["k"]);
        report(&s, &mut g, "k", 2, &["lap", "b", "zed"]);
        let names = |w: BTreeSet<String>| w.into_iter().collect::<Vec<_>>();
        // `zed` isn't a member; `lap` is this host; `k` is linked.
        assert_eq!(names(wanted(&g, &s, "lap", |n| n == "k")), ["b"]);
        assert_eq!(names(wanted(&g, &s, "lap", |_| false)), ["b", "k"]);
        assert_eq!(names(wanted(&g, &s, "b", |_| false)), ["k", "lap"]);
        assert!(wanted(&Graph::default(), &s, "lap", |_| false).is_empty());
    }

    #[test]
    fn paths_are_tried_in_order_until_one_opens() {
        let paths = [path(&["m", "b"]), path(&["k", "x", "b"]), path(&["k", "b"]), path(&["k", "y", "b"])];
        let tried = RefCell::new(Vec::new());
        let open = |p: &[String]| {
            tried.borrow_mut().push(p.join(","));
            match p[1].as_str() {
                "x" => bail!("the routed stream with `b` is refused: `x`: down"),
                _ => Ok(p.join(",")),
            }
        };
        let got = first_open(&paths, |hop| hop == "k", open);
        assert_eq!(got.unwrap(), "k,b");
        // `m` isn't linked: skipped unopened; the open stops at the first Ok.
        assert_eq!(*tried.borrow(), ["k,x,b", "k,b"]);
    }

    #[test]
    fn every_failure_is_said_when_none_opens() {
        let paths = [path(&["m", "b"]), path(&["k", "b"])];
        let failed = first_open(&paths, |hop| hop == "k", |_| -> Result<()> { bail!("timed out") }).unwrap_err();
        assert_eq!(failed, ["via m → b: no link to `m` is up here", "via k → b: timed out"]);
        assert_eq!(
            failure("b", &failed),
            "no path to `b` opened: via m → b: no link to `m` is up here; via k → b: timed out"
        );
        assert_eq!(failure("b", &first_open(&[], |_| true, |_| Ok(())).unwrap_err()), "no path to `b` in this host's link graph");
    }

    #[test]
    fn a_refused_api_client_reads_why_as_its_hellos_answer() {
        let (client, served) = UnixStream::pair().unwrap();
        (&client).write_all(b"{\"id\":0,\"method\":\"hello\"}\n").unwrap();
        refuse_api(&served, "no path to `b` in this host's link graph");
        let mut got = String::new();
        (&client).read_to_string(&mut got).unwrap();
        let r: Response = serde_json::from_str(got.trim_end()).unwrap();
        assert_eq!(r.id, Some(0));
        let e = r.error.unwrap();
        assert_eq!((e.code.as_str(), e.message.as_str()), ("failed", "no path to `b` in this host's link graph"));
    }

    #[test]
    fn the_thread_binds_and_unbinds_what_is_wanted() {
        let dir = std::env::temp_dir().join(format!("dsv-routes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A dead daemon's leftover, swept.
        std::fs::write(endpoint::route_socket(&dir, "old"), "").unwrap();
        let want: Arc<Mutex<BTreeSet<String>>> = Arc::new(Mutex::new(["b".to_string()].into()));
        let (accepted_tx, accepted) = std::sync::mpsc::channel();
        let w = Arc::clone(&want);
        let accepted_tx = Mutex::new(accepted_tx);
        let routes = Routes::spawn(Io {
            dir: dir.clone(),
            wanted: Box::new(move || w.lock().unwrap().clone()),
            accept: Box::new(move |name, _| accepted_tx.lock().unwrap().send(name.to_string()).unwrap()),
            log: |_| {},
        });
        assert!(!endpoint::route_socket(&dir, "old").exists());
        let wait = |what: &str, cond: &dyn Fn() -> bool| {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !cond() {
                assert!(std::time::Instant::now() < deadline, "timed out: {what}");
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        wait("route-b.sock", &|| endpoint::route_socket(&dir, "b").exists());
        drop(UnixStream::connect(endpoint::route_socket(&dir, "b")).unwrap());
        assert_eq!(accepted.recv_timeout(Duration::from_secs(5)).unwrap(), "b");
        *want.lock().unwrap() = ["m".to_string()].into();
        routes.poke().poke();
        wait("route-m.sock, route-b.sock gone", &|| {
            endpoint::route_socket(&dir, "m").exists() && !endpoint::route_socket(&dir, "b").exists()
        });
        drop(routes);
        assert!(!endpoint::route_socket(&dir, "m").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
