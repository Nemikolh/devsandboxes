//! The daemon's end of host → daemon RPC (docs/architecture/helper/protocol.md,
//! _RPC_): each host-opened `channel::RPC` stream gets a socket pair from
//! [`accept`] (the mux's `on_host_open` hook) and a thread of its own here,
//! which reads the `call`, dispatches it through [`METHODS`] and writes the
//! reply records (`rpc.rs`).
//!
//! What a daemon offers depends on who it runs as: methods that must act as
//! another user (`needs_root`) are listed and served only when the euid is 0,
//! since `devsbd boot` starts the daemon as the container command's user. A
//! method not offered answers like an unknown one.

use std::io;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;

use crate::mux::Conn;
use crate::proto::channel;
use crate::rpc::{self, code, Call, Obj, Record};

/// One RPC method. `version` bumps when its params or reply change shape;
/// the host picks per container from `rpc.methods` (`name@version`).
pub struct Method {
    pub name: &'static str,
    pub version: u32,
    /// Acts as another user (e.g. starts a process as the container's user):
    /// offered only by a root daemon.
    pub needs_root: bool,
    /// The final record (`result` or `error`) for a call.
    handler: fn(&Ctx, &Call) -> Record,
}

/// What a handler may know about the daemon serving it.
pub struct Ctx {
    pub root: bool,
    pub table: &'static [Method],
}

/// Every method this helper knows.
pub const METHODS: &[Method] = &[Method { name: "rpc.methods", version: 1, needs_root: false, handler: list_methods }];

/// Stack for a stream's thread: it holds one record at a time on the heap.
const STACK: usize = 128 * 1024;

/// The mux's `on_host_open` hook: an `RPC` stream gets a socket pair, one end
/// served on its own thread; any other channel is refused.
pub fn accept(channel: u8) -> Option<Conn> {
    accept_with(channel, Ctx { root: is_root(), table: METHODS })
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
    let reply = match rpc::read_record(&mut r) {
        Ok(Some(Record::Call(call))) => dispatch(ctx, &call),
        Ok(Some(_)) => Record::error(code::BAD_REQUEST, "the first record must be a call"),
        Ok(None) => return,
        Err(e) => Record::error(code::BAD_REQUEST, e.to_string()),
    };
    let mut w = &stream;
    if rpc::write_record(&mut w, &reply).is_err() {
        return;
    }
    let _ = stream.shutdown(Shutdown::Write);
    let _ = io::copy(&mut r, &mut io::sink());
}

/// Run `call` against the methods `ctx` offers.
fn dispatch(ctx: &Ctx, call: &Call) -> Record {
    match offered(ctx).find(|m| m.name == call.method) {
        Some(m) => (m.handler)(ctx, call),
        None => Record::error(code::UNKNOWN_METHOD, format!("no method `{}`", call.method)),
    }
}

fn offered(ctx: &Ctx) -> impl Iterator<Item = &'static Method> + '_ {
    ctx.table.iter().filter(|m| ctx.root || !m.needs_root)
}

/// `rpc.methods`: `{"methods": ["name@version", …]}`, in table order.
fn list_methods(ctx: &Ctx, _: &Call) -> Record {
    let names: Vec<String> = offered(ctx).map(|m| format!("{}@{}", m.name, m.version)).collect();
    Record::Result(Obj::new().with("methods", names))
}

fn is_root() -> bool {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: no arguments, no memory shared; always succeeds.
    unsafe { geteuid() == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mux::{HostOpen, Mux};
    use crate::rpc::{Exit, Json};
    use std::sync::Arc;
    use std::time::Duration;

    fn echo(_: &Ctx, call: &Call) -> Record {
        Record::Result(call.params.clone())
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
        let user = Ctx { root: false, table: TABLE };
        let root = Ctx { root: true, table: TABLE };
        assert_eq!(listed(dispatch(&user, &call("rpc.methods"))), ["rpc.methods@1", "echo@2"]);
        assert_eq!(listed(dispatch(&root, &call("rpc.methods"))), ["rpc.methods@1", "echo@2", "as.user@1"]);
        assert!(matches!(dispatch(&user, &call("as.user")), Record::Error { code: c, .. } if c == code::UNKNOWN_METHOD));
        assert!(matches!(dispatch(&root, &call("as.user")), Record::Result(_)));
    }

    #[test]
    fn the_real_table_lists_rpc_methods() {
        for root in [false, true] {
            let ctx = Ctx { root, table: METHODS };
            assert!(listed(dispatch(&ctx, &call("rpc.methods"))).contains(&"rpc.methods@1".to_string()));
        }
        assert!(accept(channel::CONTROL).is_none(), "only RPC streams are served");
    }

    /// A host and a daemon mux over pipes, the daemon serving host `Open`s
    /// with `accept_with(TABLE)`, as `serve_bridge` does with `accept`.
    fn pair(root: bool) -> Arc<Mux> {
        let (d_r, h_w) = std::io::pipe().unwrap();
        let (h_r, d_w) = std::io::pipe().unwrap();
        let daemon = Mux::new(d_w);
        let host = Mux::new(h_w);
        let hook: HostOpen = Arc::new(move |_, ch| accept_with(ch, Ctx { root, table: TABLE }));
        daemon.on_host_open(hook);
        std::thread::spawn(move || daemon.serve(d_r, |_, _| None));
        let h = Arc::clone(&host);
        std::thread::spawn(move || h.serve(h_r, |_, _| None));
        host
    }

    /// Open host stream `n` on `RPC`, send `records` then `Eof`, and collect
    /// every record until the daemon's end.
    fn round_trip(host: &Arc<Mux>, n: u32, records: &[Record]) -> Vec<Record> {
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
