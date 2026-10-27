//! The daemon's end of host → daemon RPC (docs/architecture/helper/protocol.md,
//! _RPC_): each host-opened `channel::RPC` stream gets a socket pair from
//! [`accept`] (the mux's `on_host_open` hook) and a thread of its own here,
//! which reads the `call`, dispatches it through [`METHODS`] and writes the
//! reply records (`rpc.rs`). A handler writes its intermediate records
//! (`stdout`, `event`) through [`Out`] as it goes and returns the last one, so
//! a streaming method (`run.logs` with `follow`, `run.wait`) is one held
//! stream; [`Out::gone`] tells it the caller closed the stream. A method that
//! takes the caller's input records (`session.attach`) reads them from
//! [`Out::take_input`].
//!
//! What a daemon offers depends on who it runs as: methods that must act as
//! another user (`needs_root`) are listed and served only when the euid is 0,
//! since `devsbd boot` starts the daemon as the container command's user. A
//! method not offered answers like an unknown one.

use std::io::{self, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::mux::Conn;
use crate::proto::channel;
use crate::rpc::{self, code, Call, Obj, Record};
use crate::rpc_runs;
use crate::rpc_sessions;
use crate::runs;
use crate::sessions;

/// One RPC method. `version` bumps when its params or reply change shape;
/// the host picks per container from `rpc.methods` (`name@version`).
pub struct Method {
    pub name: &'static str,
    pub version: u32,
    /// Acts as another user (e.g. starts a process as the container's user):
    /// offered only by a root daemon.
    pub needs_root: bool,
    /// Serves a call: intermediate records through [`Out`], the final one
    /// (`result` or `error`) returned.
    pub(crate) handler: Handler,
}

pub type Handler = fn(&Ctx, &Call, &mut Out) -> Record;

/// What a handler may know about the daemon serving it.
pub struct Ctx {
    pub root: bool,
    pub table: &'static [Method],
    /// The runs dir (`runs::DIR`; a temp dir in tests).
    pub runs: PathBuf,
    /// The supervisor command for run `id` under `runs`
    /// (`runs::supervisor_cmd` of this binary; the test binary in tests).
    pub supervisor: fn(&Path, &str) -> Command,
    /// The sessions dir (`sessions::DIR`; a temp dir in tests).
    pub sessions: PathBuf,
    /// The supervisor command for session `id` under `sessions`
    /// (`sessions::supervisor_cmd` of this binary; the test binary in tests).
    pub session_supervisor: fn(&Path, &str) -> Command,
}

/// A call's reply stream, before its last record.
pub struct Out<'a> {
    w: &'a mut dyn Write,
    gone: &'a dyn Fn() -> bool,
    input: Option<UnixStream>,
}

impl<'a> Out<'a> {
    pub fn new(w: &'a mut dyn Write, gone: &'a dyn Fn() -> bool) -> Out<'a> {
        Out { w, gone, input: None }
    }

    /// Over a stream whose caller's records after the `call` a handler may
    /// read ([`Out::take_input`]).
    fn with_input(self, input: Option<UnixStream>) -> Out<'a> {
        Out { input, ..self }
    }

    /// The stream itself, once: the caller's records after the `call` are
    /// read from it, and the handler may write intermediate records to it
    /// from other threads, as long as none still writes when it returns (the
    /// last record follows). `None` in-process (tests) or after the first
    /// take. Whatever input the handler leaves is drained after its reply.
    pub fn take_input(&mut self) -> Option<UnixStream> {
        self.input.take()
    }

    /// Write one intermediate record; an error means the caller is gone.
    pub fn send(&mut self, rec: &Record) -> io::Result<()> {
        rpc::write_record(&mut self.w, rec)
    }

    /// The caller closed the stream (cancelled, or its bridge went): a
    /// waiting handler stops instead of waiting for nobody.
    pub fn gone(&self) -> bool {
        (self.gone)()
    }
}

/// Every method this helper knows. `run.start` starts the supervisor as the
/// run's user; `run.rm`/`run.prune` delete dirs (and kill groups) that
/// belong to that user in the sticky runs dir, which only root (or that
/// user, whom a non-root daemon may not be) can: all three root only, the
/// host execs them as the user otherwise. The reads work as any user. The
/// `session.*` methods start a session's supervisor as its user and attach to
/// or delete sessions of any user: root only too.
pub const METHODS: &[Method] = &[
    Method { name: "rpc.methods", version: 1, needs_root: false, handler: list_methods },
    Method { name: "run.start", version: 1, needs_root: true, handler: rpc_runs::start },
    Method { name: "run.ls", version: 1, needs_root: false, handler: rpc_runs::ls },
    Method { name: "run.logs", version: 1, needs_root: false, handler: rpc_runs::logs },
    Method { name: "run.wait", version: 1, needs_root: false, handler: rpc_runs::wait },
    Method { name: "run.rm", version: 1, needs_root: true, handler: rpc_runs::rm },
    Method { name: "run.prune", version: 1, needs_root: true, handler: rpc_runs::prune },
    Method { name: "session.attach", version: 1, needs_root: true, handler: rpc_sessions::attach },
    Method { name: "session.ls", version: 1, needs_root: true, handler: rpc_sessions::ls },
    Method { name: "session.rm", version: 1, needs_root: true, handler: rpc_sessions::rm },
];

/// Stack for a stream's thread: it holds one record at a time on the heap.
const STACK: usize = 128 * 1024;

/// The mux's `on_host_open` hook: an `RPC` stream gets a socket pair, one end
/// served on its own thread; any other channel is refused.
pub fn accept(channel: u8) -> Option<Conn> {
    let ctx = Ctx {
        root: is_root(),
        table: METHODS,
        runs: runs::DIR.into(),
        supervisor,
        sessions: sessions::DIR.into(),
        session_supervisor,
    };
    accept_with(channel, ctx)
}

fn own_exe() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| "/proc/self/exe".into())
}

/// This binary's `run supervise <id>`.
fn supervisor(_: &Path, id: &str) -> Command {
    runs::supervisor_cmd(&own_exe(), id)
}

/// This binary's `session supervise <id>`.
fn session_supervisor(_: &Path, id: &str) -> Command {
    sessions::supervisor_cmd(&own_exe(), id)
}

fn accept_with(channel: u8, ctx: Ctx) -> Option<Conn> {
    if channel != channel::RPC {
        return None;
    }
    let (ours, theirs) = UnixStream::pair().ok()?;
    std::thread::Builder::new()
        .name("rpc".into())
        .stack_size(STACK)
        .spawn(move || serve(theirs, &ctx))
        .ok()?;
    Some(ours.into())
}

/// One stream: the first record must be a `call`; its reply is the last
/// record we write. Then our side is half-closed and the caller's input
/// drained until its `Eof` (or `Close`), so the mux tears the stream down
/// cleanly instead of failing a write into a closed socket.
fn serve(stream: UnixStream, ctx: &Ctx) {
    let mut r = &stream;
    let mut w = &stream;
    let gone = || hung_up(&stream);
    let reply = match rpc::read_record(&mut r) {
        Ok(Some(Record::Call(call))) => {
            dispatch(ctx, &call, &mut Out::new(&mut w, &gone).with_input(stream.try_clone().ok()))
        }
        Ok(Some(_)) => Record::error(code::BAD_REQUEST, "the first record must be a call"),
        Ok(None) => return,
        Err(e) => Record::error(code::BAD_REQUEST, e.to_string()),
    };
    if rpc::write_record(&mut w, &reply).is_err() {
        return;
    }
    let _ = stream.shutdown(Shutdown::Write);
    let _ = io::copy(&mut r, &mut io::sink());
}

/// Run `call` against the methods `ctx` offers.
fn dispatch(ctx: &Ctx, call: &Call, out: &mut Out) -> Record {
    match offered(ctx).find(|m| m.name == call.method) {
        Some(m) => (m.handler)(ctx, call, out),
        None => Record::error(code::UNKNOWN_METHOD, format!("no method `{}`", call.method)),
    }
}

fn offered(ctx: &Ctx) -> impl Iterator<Item = &'static Method> + '_ {
    ctx.table.iter().filter(|m| ctx.root || !m.needs_root)
}

/// `rpc.methods`: `{"methods": ["name@version", …]}`, in table order.
fn list_methods(ctx: &Ctx, _: &Call, _: &mut Out) -> Record {
    let names: Vec<String> = offered(ctx).map(|m| format!("{}@{}", m.name, m.version)).collect();
    Record::Result(Obj::new().with("methods", names))
}

/// Whether the mux shut our stream's peer end down both ways: a `Close`
/// (cancel) or a dead bridge. The caller's `Eof` only shuts its write side,
/// which reads as end of input, not as a hang-up. `poll` with no wait.
pub(crate) fn hung_up(s: &UnixStream) -> bool {
    use std::os::fd::AsRawFd;
    #[repr(C)]
    struct PollFd {
        fd: i32,
        events: i16,
        revents: i16,
    }
    #[cfg(target_os = "linux")]
    type Nfds = std::ffi::c_ulong;
    #[cfg(not(target_os = "linux"))]
    type Nfds = std::ffi::c_uint;
    unsafe extern "C" {
        fn poll(fds: *mut PollFd, nfds: Nfds, timeout: i32) -> i32;
    }
    const POLLERR: i16 = 0x8;
    const POLLHUP: i16 = 0x10;
    let mut fd = PollFd { fd: s.as_raw_fd(), events: 0, revents: 0 };
    // SAFETY: one valid pollfd for the call's duration; no wait.
    let n = unsafe { poll(&mut fd, 1, 0) };
    n > 0 && fd.revents & (POLLHUP | POLLERR) != 0
}

fn is_root() -> bool {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: no arguments, no memory shared; always succeeds.
    unsafe { geteuid() == 0 }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::mux::{HostOpen, Mux};
    use crate::rpc::{Exit, Json};
    use std::sync::Arc;
    use std::time::Duration;

    fn echo(_: &Ctx, call: &Call, _: &mut Out) -> Record {
        Record::Result(call.params.clone())
    }

    fn no_supervisor(_: &Path, _: &str) -> Command {
        Command::new("/nonexistent")
    }

    pub(crate) fn ctx(root: bool, table: &'static [Method]) -> Ctx {
        Ctx {
            root,
            table,
            runs: "/nonexistent/runs".into(),
            supervisor: no_supervisor,
            sessions: "/nonexistent/sessions".into(),
            session_supervisor: no_supervisor,
        }
    }

    /// `dispatch` with intermediate records dropped.
    fn dispatch(ctx: &Ctx, call: &Call) -> Record {
        let mut sink = io::sink();
        super::dispatch(ctx, call, &mut Out::new(&mut sink, &|| false))
    }

    const TABLE: &[Method] = &[
        Method { name: "rpc.methods", version: 1, needs_root: false, handler: list_methods },
        Method { name: "echo", version: 2, needs_root: false, handler: echo },
        Method { name: "as.user", version: 1, needs_root: true, handler: echo },
    ];

    fn call(method: &str) -> Call {
        Call { method: method.into(), ..Call::default() }
    }

    fn listed(rec: Record) -> Vec<String> {
        let Record::Result(o) = rec else { panic!("not a result: {rec:?}") };
        o.strs("methods").unwrap().to_vec()
    }

    #[test]
    fn root_only_methods_are_listed_and_served_only_as_root() {
        let user = ctx(false, TABLE);
        let root = ctx(true, TABLE);
        assert_eq!(listed(dispatch(&user, &call("rpc.methods"))), ["rpc.methods@1", "echo@2"]);
        assert_eq!(listed(dispatch(&root, &call("rpc.methods"))), ["rpc.methods@1", "echo@2", "as.user@1"]);
        assert!(matches!(dispatch(&user, &call("as.user")), Record::Error { code: c, .. } if c == code::UNKNOWN_METHOD));
        assert!(matches!(dispatch(&root, &call("as.user")), Record::Result(_)));
    }

    #[test]
    fn the_real_table_lists_rpc_methods() {
        let all = [
            "rpc.methods@1",
            "run.start@1",
            "run.ls@1",
            "run.logs@1",
            "run.wait@1",
            "run.rm@1",
            "run.prune@1",
            "session.attach@1",
            "session.ls@1",
            "session.rm@1",
        ];
        assert_eq!(listed(dispatch(&ctx(true, METHODS), &call("rpc.methods"))), all);
        // A non-root daemon can't act as the run's (or session's) user: the
        // host execs those.
        let user = ["rpc.methods@1", "run.ls@1", "run.logs@1", "run.wait@1"];
        assert_eq!(listed(dispatch(&ctx(false, METHODS), &call("rpc.methods"))), user);
        assert!(accept(channel::CONTROL).is_none(), "only RPC streams are served");
    }

    /// A peer's `Eof` (write side shut) isn't a hang-up; its `Close` (both
    /// sides) or dropping its end is.
    #[test]
    fn hang_up_is_both_sides_shut() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        assert!(!hung_up(&ours));
        theirs.shutdown(Shutdown::Write).unwrap();
        assert!(!hung_up(&ours), "eof only");
        theirs.shutdown(Shutdown::Both).unwrap();
        assert!(hung_up(&ours));
        let (ours, theirs) = UnixStream::pair().unwrap();
        drop(theirs);
        assert!(hung_up(&ours));
    }

    /// A host and a daemon mux over pipes, the daemon serving host `Open`s
    /// with `accept_with(TABLE)`, as `serve_bridge` does with `accept`.
    pub(crate) fn pair(root: bool) -> Arc<Mux> {
        pair_with(move || ctx(root, TABLE))
    }

    /// [`pair`] serving each stream with a fresh `make()`.
    pub(crate) fn pair_with(make: impl Fn() -> Ctx + Send + Sync + 'static) -> Arc<Mux> {
        let (d_r, h_w) = std::io::pipe().unwrap();
        let (h_r, d_w) = std::io::pipe().unwrap();
        let daemon = Mux::new(d_w);
        let host = Mux::new(h_w);
        let hook: HostOpen = Arc::new(move |_, ch| accept_with(ch, make()));
        daemon.on_host_open(hook);
        std::thread::spawn(move || daemon.serve(d_r, |_, _| None));
        let h = Arc::clone(&host);
        std::thread::spawn(move || h.serve(h_r, |_, _| None));
        host
    }

    /// Open host stream `n` on `RPC`, send `records` then `Eof`, and collect
    /// every record until the daemon's end.
    pub(crate) fn round_trip(host: &Arc<Mux>, n: u32, records: &[Record]) -> Vec<Record> {
        let (mut caller, theirs) = UnixStream::pair().unwrap();
        caller.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        host.attach((1 << 31) | n, theirs, Some(channel::RPC)).unwrap();
        for rec in records {
            rpc::write_record(&mut caller, rec).unwrap();
        }
        caller.shutdown(Shutdown::Write).unwrap();
        let mut got = Vec::new();
        while let Some(rec) = rpc::read_record(&mut caller).unwrap() {
            got.push(rec);
        }
        got
    }

    #[test]
    fn calls_round_trip_over_a_mux_pair() {
        let host = pair(false);
        let got = round_trip(&host, 1, &[Record::Call(call("rpc.methods"))]);
        assert_eq!(got.into_iter().map(listed).collect::<Vec<_>>(), [vec!["rpc.methods@1", "echo@2"]]);

        let params = Obj::new().with("argv", vec!["a".to_string()]).with("n", Json::Int(7));
        let echo = Call { method: "echo".into(), params: params.clone(), ..Call::default() };
        assert_eq!(round_trip(&host, 2, &[Record::Call(echo)]), [Record::Result(params)]);

        let unknown = round_trip(&host, 3, &[Record::Call(call("as.user"))]);
        assert_eq!(unknown, [Record::error(code::UNKNOWN_METHOD, "no method `as.user`")]);

        let bad = round_trip(&host, 4, &[Record::Exit(Exit::Code(0))]);
        assert_eq!(bad, [Record::error(code::BAD_REQUEST, "the first record must be a call")]);

        // Extra input after the call is drained, not an error.
        let extra = round_trip(&host, 5, &[Record::Call(call("echo")), Record::Stdin(b"x".to_vec()), Record::StdinEof]);
        assert_eq!(extra, [Record::Result(Obj::new())]);
    }

    /// Streams are served concurrently: one caller that never sends its call
    /// doesn't hold up another.
    #[test]
    fn a_stalled_stream_does_not_block_others() {
        let host = pair(true);
        let (_stalled, theirs) = UnixStream::pair().unwrap();
        host.attach((1 << 31) | 1, theirs, Some(channel::RPC)).unwrap();
        let got = round_trip(&host, 2, &[Record::Call(call("rpc.methods"))]);
        assert_eq!(got.len(), 1);
    }
}
