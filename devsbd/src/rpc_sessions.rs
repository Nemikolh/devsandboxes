//! The `session.*` RPC methods (docs/architecture/helper/in-container.md,
//! *Sessions over RPC*): `sessions.rs`' operations for callers that run no
//! CLI in the container (the serve daemon, for a fleet hub's `PTY`). The
//! daemon is then the session's client, like `devsbd session new|attach`
//! without the terminal: the same `meta` and hello checks, the same one-client
//! takeover.
//!
//! | Method | Params | Records |
//! |---|---|---|
//! | `session.attach` | `id`, `create?` `{argv, cwd?, env?, user?}` | → `stdin`, `stdin-eof`, `resize`; ← `stdout`…, then `exit` or `error` |
//! | `session.ls` | — | `result {sessions: [line…]}` (`session ls`' lines) |
//! | `session.rm` | `id` | `result {}` |
//!
//! `session.attach`'s end: `exit <code>` (`128 + n` for signal `n`) when the
//! command ended, else `error` `no-session` (unknown, `lost`, gone without an
//! end), `detached` (a later attach took over) or `version` (started by a
//! helper of another session wire). A caller that hangs up mid-attach
//! detaches: the session runs on, a later attach gets it. All three methods
//! are root only (`rpc_server::METHODS`).

use std::io::{self, PipeWriter, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::boot::Account;
use crate::control::valid_run_id;
use crate::rpc::{self, code, Call, Exit, Json, Obj, Record};
use crate::rpc_runs::{hand_over, lookup, reap, switch_user};
use crate::rpc_server::{hung_up, Ctx, Out};
use crate::sessions::{self, End, SizeFn};

/// How often an attach whose caller sent its `Eof` checks whether the caller
/// hung up altogether (the stream's `Close`, or the bridge gone).
const HANGUP_POLL: Duration = Duration::from_millis(100);

fn bad(msg: impl Into<String>) -> Record {
    Record::error(code::BAD_REQUEST, msg)
}

fn failed(e: impl ToString) -> Record {
    Record::error(code::FAILED, e.to_string())
}

/// The `id` param, a valid session id.
fn id(call: &Call) -> Result<&str, Record> {
    let id = call.params.str("id").ok_or_else(|| bad(format!("{} needs `id`", call.method)))?;
    if !valid_run_id(id) {
        return Err(bad(format!("bad session id `{id}`")));
    }
    Ok(id)
}

/// `session.attach`'s `create`.
struct Create {
    argv: Vec<String>,
    cwd: Option<String>,
    env: Vec<(String, String)>,
    user: Option<(Account, Vec<u32>)>,
}

fn create_params(c: &Obj) -> Result<Create, Record> {
    let Some(argv) = c.strs("argv").filter(|a| !a.is_empty()) else {
        return Err(bad("session.attach: `create` needs a non-empty `argv`"));
    };
    let env = match c.get("env") {
        None | Some(Json::Null) => Vec::new(),
        Some(_) => c.map("env").ok_or_else(|| bad("session.attach: `env` must map names to strings"))?,
    };
    let user = match c.str("user") {
        None => None,
        Some(user) => Some(lookup(user).ok_or_else(|| failed(format!("unknown user `{user}`")))?),
    };
    Ok(Create { argv: argv.to_vec(), cwd: c.str("cwd").map(str::to_string), env, user })
}

/// `session.attach`: with `create`, as `devsbd session new --id <id> --cwd
/// <cwd> -- <argv>` run as `user` (the session's dir handed to them, its
/// supervisor started as them like `run.start`'s, so the session and its
/// `sock` are theirs); an id that is already a session attaches to it.
/// Without, attach only. Then relays the stream's input into the session and
/// its output back as `stdout` until the end.
pub fn attach(ctx: &Ctx, call: &Call, out: &mut Out) -> Record {
    let id = match id(call) {
        Ok(id) => id,
        Err(rec) => return rec,
    };
    let create = match call.params.get("create") {
        None | Some(Json::Null) => None,
        Some(Json::Obj(c)) => match create_params(c) {
            Ok(c) => Some(c),
            Err(rec) => return rec,
        },
        Some(_) => return bad("session.attach: `create` must be an object"),
    };
    let Some(stream) = out.take_input() else { return failed("session.attach needs its stream's input") };
    let mut child = match &create {
        None => None,
        Some(c) => match start(ctx, id, c) {
            Ok(child) => child,
            Err(e) => return failed(e),
        },
    };
    let end = relay(&ctx.sessions, id, child.as_mut(), stream);
    // The daemon outlives the session: wait for the supervisor it spawned.
    if let Some(child) = child {
        reap(child);
    }
    match end {
        Ok(end) => end_record(id, end),
        Err(e) => failed(e),
    }
}

/// Create session `id` (`Ok(None)`: it exists, attach to it).
fn start(ctx: &Ctx, id: &str, c: &Create) -> io::Result<Option<Child>> {
    sessions::create_with(&ctx.sessions, id, &c.argv, c.cwd.as_deref(), |id| {
        if let Some((a, _)) = &c.user {
            hand_over(&ctx.sessions.join(id), a)?;
        }
        let mut cmd = (ctx.session_supervisor)(&ctx.sessions, id);
        if let Some((a, groups)) = &c.user {
            switch_user(&mut cmd, a.uid, a.gid, groups.clone());
            cmd.env("HOME", &a.home);
            if let Some(name) = &a.name {
                cmd.env("USER", name);
            }
        }
        cmd.envs(c.env.iter().map(|(k, v)| (k, v)));
        Ok(cmd)
    })
}

/// The attach's last record.
fn end_record(id: &str, end: End) -> Record {
    let code = match end {
        End::Exit(n) => return Record::Exit(Exit::Code(n)),
        End::NoSession(_) => code::NO_SESSION,
        End::Detached => code::DETACHED,
        End::Version(_) => code::VERSION,
    };
    Record::error(code, end.done(id).note.unwrap_or_default())
}

/// The session's socket, once connected, and whether the caller hung up:
/// whichever comes second shuts the socket, so the relay ends as a lost
/// connection (the supervisor drops its client, the session runs on).
#[derive(Default)]
struct Link {
    sock: Mutex<(Option<UnixStream>, bool)>,
    done: AtomicBool,
}

impl Link {
    fn connected(&self, sock: &UnixStream) {
        let mut g = self.sock.lock().unwrap_or_else(|e| e.into_inner());
        if g.1 {
            let _ = sock.shutdown(Shutdown::Both);
        } else {
            g.0 = sock.try_clone().ok();
        }
    }

    fn hang_up(&self) {
        let mut g = self.sock.lock().unwrap_or_else(|e| e.into_inner());
        g.1 = true;
        if let Some(s) = &g.0 {
            let _ = s.shutdown(Shutdown::Both);
        }
    }
}

/// The session's output as `stdout` records on the RPC stream, from
/// `sessions::relay`'s writer thread. [`Stdout::close`] (before the handler
/// returns) waits out a write in progress and drops later ones, so nothing
/// lands after the last record.
#[derive(Clone)]
struct Stdout(Arc<Mutex<Option<UnixStream>>>);

impl Stdout {
    fn close(&self) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

impl Write for Stdout {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        let mut g = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(s) = g.as_mut() {
            rpc::write_record(s, &Record::Stdout(b.to_vec()))?;
        }
        Ok(b.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Be session `id`'s client over the RPC `stream`: its `stdin` records go
/// through a pipe into `sessions::join_end`'s input, its `resize` records set
/// what the relay's size fn answers (polled every `sessions::SIZE_POLL`),
/// the session's output comes back as `stdout`. A hang-up shuts the session
/// socket ([`Link`]).
fn relay(root: &Path, id: &str, child: Option<&mut Child>, stream: UnixStream) -> io::Result<End> {
    let (stdin_r, stdin_w) = io::pipe()?;
    let size = Arc::new(Mutex::new(None::<(u16, u16)>));
    let link = Arc::new(Link::default());
    let stdout = Stdout(Arc::new(Mutex::new(Some(stream.try_clone()?))));
    {
        let (size, link) = (Arc::clone(&size), Arc::clone(&link));
        std::thread::Builder::new()
            .name("rpc-session-in".into())
            .stack_size(64 * 1024)
            .spawn(move || feed(stream, stdin_w, &size, &link))?;
    }
    let size_fn: SizeFn = {
        let size = Arc::clone(&size);
        Box::new(move || *size.lock().unwrap_or_else(|e| e.into_inner()))
    };
    let l = Arc::clone(&link);
    let end = sessions::join_end(root, id, child, stdin_r, Box::new(stdout.clone()), Some(size_fn), move |sock| {
        l.connected(sock)
    });
    link.done.store(true, Ordering::SeqCst);
    stdout.close();
    Ok(end)
}

/// The caller's records after the `call`, until its input ends; then, until
/// the attach ends, a watch for the caller hanging up altogether (its `Eof`
/// alone is not one). `signal` and anything else are ignored: a session's
/// program gets signals from its terminal, i.e. as input bytes.
fn feed(mut stream: UnixStream, stdin: PipeWriter, size: &Mutex<Option<(u16, u16)>>, link: &Link) {
    let mut stdin = Some(stdin);
    loop {
        match rpc::read_record(&mut stream) {
            Ok(Some(Record::Stdin(bytes))) => {
                // A failed write: the relay is over; keep reading to the end.
                if stdin.as_mut().is_some_and(|w| w.write_all(&bytes).is_err()) {
                    stdin = None;
                }
            }
            Ok(Some(Record::StdinEof)) => stdin = None,
            Ok(Some(Record::Resize { cols, rows })) => {
                *size.lock().unwrap_or_else(|e| e.into_inner()) = Some((cols, rows))
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
    drop(stdin);
    while !link.done.load(Ordering::SeqCst) {
        if hung_up(&stream) {
            return link.hang_up();
        }
        std::thread::sleep(HANGUP_POLL);
    }
}

/// `session.ls`: `session ls`' lines.
pub fn ls(ctx: &Ctx, _: &Call, _: &mut Out) -> Record {
    match sessions::ls(&ctx.sessions) {
        Ok(text) => Record::Result(Obj::new().with("sessions", text.lines().map(str::to_string).collect::<Vec<_>>())),
        Err(e) => failed(e),
    }
}

/// `session.rm`: as `devsbd session rm <id>`.
pub fn rm(ctx: &Ctx, call: &Call, _: &mut Out) -> Record {
    let id = match id(call) {
        Ok(id) => id,
        Err(rec) => return rec,
    };
    match sessions::rm(&ctx.sessions, id) {
        Ok(()) => Record::Result(Obj::new()),
        Err(e) => failed(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mux::Mux;
    use crate::proto::channel;
    use crate::rpc_server::tests::{ctx, pair_with, round_trip};
    use crate::rpc_server::METHODS;
    use crate::sessions::tests::{meta, temp_root, test_supervisor, wait_for};
    use std::fs;

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// A daemon serving the real table (as root) over sessions under `root`,
    /// the test binary as their supervisor.
    fn host(root: &Path, as_root: bool) -> Arc<Mux> {
        let r = root.to_path_buf();
        pair_with(move || Ctx { sessions: r.clone(), session_supervisor: test_supervisor, ..ctx(as_root, METHODS) })
    }

    fn attach_call(id: &str, create: Option<Obj>) -> Record {
        let mut params = Obj::new().with("id", id);
        if let Some(c) = create {
            params = params.with("create", c);
        }
        Record::Call(Call { method: "session.attach".into(), params, ..Call::default() })
    }

    fn sh(script: &str) -> Obj {
        Obj::new().with("argv", strings(&["sh", "-c", script]))
    }

    /// One host-side caller of a held stream.
    struct Caller {
        s: UnixStream,
        recs: Vec<Record>,
        ended: bool,
    }

    impl Caller {
        fn open(host: &Arc<Mux>, n: u32, call: Record) -> Caller {
            let (s, theirs) = UnixStream::pair().unwrap();
            s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
            host.attach((1 << 31) | n, theirs, Some(channel::RPC)).unwrap();
            let mut c = Caller { s, recs: Vec::new(), ended: false };
            c.send(call);
            c
        }

        fn send(&mut self, rec: Record) {
            rpc::write_record(&mut self.s, &rec).unwrap();
        }

        fn stdin(&mut self, text: &str) {
            self.send(Record::Stdin(text.as_bytes().to_vec()));
        }

        fn stdout(&self) -> String {
            let bytes: Vec<u8> = self
                .recs
                .iter()
                .flat_map(|r| match r {
                    Record::Stdout(b) => b.clone(),
                    _ => Vec::new(),
                })
                .collect();
            String::from_utf8_lossy(&bytes).into_owned()
        }

        /// Read one record; false at the stream's end.
        fn read_one(&mut self) -> bool {
            match rpc::read_record(&mut self.s).expect("a record within the timeout") {
                Some(rec) => {
                    self.recs.push(rec);
                    true
                }
                None => {
                    self.ended = true;
                    false
                }
            }
        }

        fn wait_output(&mut self, needle: &str) {
            while !self.stdout().contains(needle) {
                assert!(!self.ended && self.read_one(), "no `{needle}` in {:?}", self.recs);
            }
        }

        /// Every record to the end; the last one.
        fn finish(&mut self) -> Record {
            while !self.ended && self.read_one() {}
            self.recs.iter().rev().find(|r| !matches!(r, Record::Stdout(_))).cloned().expect("a last record")
        }
    }

    fn is_error(rec: &Record, want: &str) -> bool {
        matches!(rec, Record::Error { code, .. } if code == want)
    }

    #[test]
    fn create_attach_round_trip_and_ends() {
        let root = temp_root("rpc-echo");
        let user = host(&root, false);
        let host = host(&root, true);
        let id = "1790000000-0001";
        let mut c = Caller::open(&host, 1, attach_call(id, Some(sh("read l; echo got:$l; exit 3"))));
        c.stdin("hi\n");
        c.send(Record::StdinEof);
        assert_eq!(c.finish(), Record::Exit(Exit::Code(3)), "{:?}", c.recs);
        assert!(c.stdout().contains("got:hi"), "{:?}", c.stdout());
        // Ended: its code at once, attach only or with `create` (the id is taken).
        assert_eq!(round_trip(&host, 2, &[attach_call(id, None)]), [Record::Exit(Exit::Code(3))]);
        assert_eq!(round_trip(&host, 3, &[attach_call(id, Some(sh("exit 9")))]), [Record::Exit(Exit::Code(3))]);
        // Unknown: `no-session`; bad params: `bad-request`.
        let got = round_trip(&host, 4, &[attach_call("1790000000-ffff", None)]);
        assert!(matches!(&got[..], [r] if is_error(r, code::NO_SESSION)), "{got:?}");
        let got = round_trip(&host, 5, &[attach_call("../x", None)]);
        assert!(matches!(&got[..], [r] if is_error(r, code::BAD_REQUEST)), "{got:?}");
        let got = round_trip(&host, 6, &[attach_call("1790000000-0002", Some(Obj::new()))]);
        assert!(matches!(&got[..], [r] if is_error(r, code::BAD_REQUEST)), "{got:?}");
        // `session.ls` / `session.rm`.
        let ls = Record::Call(Call { method: "session.ls".into(), ..Call::default() });
        let got = round_trip(&host, 7, &[ls]);
        let [Record::Result(o)] = &got[..] else { panic!("{got:?}") };
        let lines = o.strs("sessions").unwrap();
        assert!(lines.len() == 1 && lines[0].starts_with(&format!("{id} exited 3 - ")), "{lines:?}");
        let rm = |id: &str| Record::Call(Call { method: "session.rm".into(), params: Obj::new().with("id", id), ..Call::default() });
        assert_eq!(round_trip(&host, 8, &[rm(id)]), [Record::Result(Obj::new())]);
        assert!(!root.join(id).exists());
        let got = round_trip(&host, 9, &[rm(id)]);
        assert!(matches!(&got[..], [r] if is_error(r, code::FAILED)), "{got:?}");

        // A non-root daemon offers none of them.
        let got = round_trip(&user, 1, &[attach_call(id, Some(sh("true")))]);
        assert!(matches!(&got[..], [r] if is_error(r, code::UNKNOWN_METHOD)), "{got:?}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_later_attach_takes_over() {
        let root = temp_root("rpc-takeover");
        let host = host(&root, true);
        let id = "1790000000-0001";
        let mut a = Caller::open(&host, 1, attach_call(id, Some(sh("read l; echo got:$l; exit 5"))));
        wait_for("the first attach", || root.join(id).join("client").exists());
        let mut b = Caller::open(&host, 2, attach_call(id, None));
        let last = a.finish();
        assert!(is_error(&last, code::DETACHED), "{last:?}");
        b.stdin("x\n");
        assert_eq!(b.finish(), Record::Exit(Exit::Code(5)));
        assert!(b.stdout().contains("got:x"), "{:?}", b.stdout());
        assert!(!a.stdout().contains("got:x"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn resize_reaches_the_program() {
        let root = temp_root("rpc-size");
        let host = host(&root, true);
        let id = "1790000000-0001";
        let mut c = Caller::open(&host, 1, attach_call(id, Some(sh("read a; stty size; read b; stty size"))));
        c.send(Record::Resize { cols: 100, rows: 30 });
        std::thread::sleep(sessions::SIZE_POLL * 3);
        c.stdin("\n");
        c.wait_output("30 100");
        c.send(Record::Resize { cols: 120, rows: 40 });
        std::thread::sleep(sessions::SIZE_POLL * 3);
        c.stdin("\n");
        c.wait_output("40 120");
        assert_eq!(c.finish(), Record::Exit(Exit::Code(0)));
        let _ = fs::remove_dir_all(&root);
    }

    /// The caller's `Close` mid-attach detaches: the session runs on, a later
    /// attach gets it. Its `Eof` alone doesn't.
    #[test]
    fn a_hang_up_detaches_and_the_session_runs_on() {
        let root = temp_root("rpc-hangup");
        let host = host(&root, true);
        let id = "1790000000-0001";
        let dir = root.join(id);
        let a = Caller::open(&host, 1, attach_call(id, Some(sh("read l; echo got:$l; exit 4"))));
        wait_for("the attach", || dir.join("client").exists());
        a.s.shutdown(Shutdown::Write).unwrap();
        std::thread::sleep(HANGUP_POLL * 3);
        assert!(dir.join("client").exists(), "an Eof is no hang-up");
        assert!(host.close_stream((1 << 31) | 1, "cancelled"));
        wait_for("the detach", || !dir.join("client").exists());
        assert!(meta(&root, id).ended.is_none(), "still running");
        let mut b = Caller::open(&host, 2, attach_call(id, None));
        b.stdin("x\n");
        assert_eq!(b.finish(), Record::Exit(Exit::Code(4)));
        assert!(b.stdout().contains("got:x"), "{:?}", b.stdout());
        let _ = fs::remove_dir_all(&root);
    }

    /// `create` with `user`: the session's dir, and its command, are that
    /// user's. Needs root to switch (and a `nobody` in passwd); skipped
    /// otherwise.
    #[test]
    fn create_as_user_runs_the_session_as_them() {
        use std::os::unix::fs::MetadataExt;
        let is_root = fs::metadata("/proc/self").map(|m| m.uid() == 0).unwrap_or(false);
        let Some((a, _)) = lookup("nobody").filter(|_| is_root) else { return };
        let root = temp_root("rpc-user");
        let host = host(&root, true);
        let id = "1790000000-0001";
        // Waits for a line: a command that exits before the attach connects
        // leaves only its code.
        let script = "read l; id -u; echo home:$HOME x:$X; exit 0";
        let create = sh(script).with("user", "nobody").with("env", vec![("X".to_string(), "1".to_string())]);
        let mut c = Caller::open(&host, 1, attach_call(id, Some(create)));
        c.stdin("\n");
        c.send(Record::StdinEof);
        assert_eq!(c.finish(), Record::Exit(Exit::Code(0)), "{:?}", c.recs);
        assert!(c.stdout().contains(&format!("{}\r\n", a.uid)), "{:?}", c.stdout());
        assert!(c.stdout().contains(&format!("home:{} x:1", a.home)), "{:?}", c.stdout());
        assert_eq!(fs::metadata(root.join(id)).unwrap().uid(), a.uid);
        let got = round_trip(&host, 2, &[attach_call("1790000000-0002", Some(sh("true").with("user", "no-such-user-x")))]);
        assert!(matches!(&got[..], [r] if is_error(r, code::FAILED)), "{got:?}");
        let _ = fs::remove_dir_all(&root);
    }
}
