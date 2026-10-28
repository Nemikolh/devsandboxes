//! The worker's end of a fleet link's `PTY` stream
//! (docs/architecture/daemon/fleet.md, *PTY*): a terminal into a lent
//! instance, as a session attach through its helper's `session.attach`
//! (`HelperClient::call_with_input`, decision 13 of
//! docs/plans/active/fleet.md: no `exec -it`, no in-container client).
//!
//! The stream carries `rpc` records (`src/devsbd/rpc.rs`). The hub's first
//! is a `call` of [`METHOD`] whose params are the [`Header`]; then its
//! `stdin`, `resize` and `stdin-eof` records go into the attach, and the
//! helper's records come back unchanged: `stdout`…, then one end record,
//! `exit` or `error`. A stream refused before the attach gets one `error`
//! record ([`code`]). The hub closing the stream detaches (the call is
//! cancelled; the session runs on in the container); an attach that ends
//! without an end record is [`code::LOST`].
//!
//! [`parse_header`], [`gate`], [`offered`] and [`attach_params`] are pure
//! (`gate` canonicalizes the header's root); [`serve`] is the I/O.

use std::net::Shutdown;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::lent::{self, Scope};
use crate::commands::exec::{SHELL_FALLBACK_CMD, exec_context};
use crate::devsbd::control::valid_run_id;
use crate::devsbd::rpc::{self, Json, Obj, Record};
use crate::serve::endpoint::Stream;
use crate::serve::helper::{Call, CallError, Cancel, HelperClient, Input};
use crate::state::{Instance, State};

/// The header's method, and the helper's.
pub const METHOD: &str = "session.attach";

/// The `error` codes a `PTY` stream ends with besides the helper's own
/// (`rpc::code`: `no-session`, `detached`, `version`, `bad-request`,
/// `failed`), which are relayed as it sent them. The worker's refusals use
/// `bad-request` and `failed` too.
pub mod code {
    pub use crate::devsbd::rpc::code::{BAD_REQUEST, FAILED};
    /// The header's root is outside the fleet root, or not this hub's to
    /// reach.
    pub const DENIED: &str = "denied";
    /// No such root, or no lent instance by that name or id in it.
    pub const NOT_FOUND: &str = "not-found";
    /// The instance isn't running (no live bridge to its helper).
    pub const NOT_RUNNING: &str = "not-running";
    /// No terminal over the link for this instance: no helper, or its
    /// daemon doesn't offer `session.attach` (not root, e.g.
    /// `containerUser`; or an older helper).
    pub const UNSUPPORTED: &str = "unsupported";
    /// The attach ended without an end record (the bridge to the instance
    /// was lost): neither an exit nor a refusal; the session may live on.
    pub const LOST: &str = "lost";
}

/// How long the hub has to send the header once it opened the stream.
const HEADER_WAIT: Duration = Duration::from_secs(30);

/// What the hub asks: `{"root", "instance", "session", "create"?:
/// {"argv"?}}`.
#[derive(Debug, Clone, PartialEq)]
pub struct Header {
    /// The instance's config root (absolute, under the fleet root).
    pub root: PathBuf,
    /// An exact instance name or instance id.
    pub instance: String,
    /// The session id (`control::valid_run_id`).
    pub session: String,
    /// `None`: attach only. `Some(argv)`: create it running `argv` (empty:
    /// the login shell, `SHELL_FALLBACK_CMD`) unless it exists, then attach.
    pub create: Option<Vec<String>>,
}

fn bad(message: impl Into<String>) -> Record {
    Record::error(code::BAD_REQUEST, message)
}

/// A record's name, for a refusal.
fn kind(rec: &Record) -> &'static str {
    match rec {
        Record::Call(_) => "call",
        Record::Stdin(_) => "stdin",
        Record::StdinEof => "stdin-eof",
        Record::Resize { .. } => "resize",
        Record::Signal(_) => "signal",
        Record::Stdout(_) => "stdout",
        Record::Stderr(_) => "stderr",
        Record::Event(_) => "event",
        Record::Result(_) => "result",
        Record::Error { .. } => "error",
        Record::Exit(_) => "exit",
    }
}

/// The hub's first record as a [`Header`], or the `bad-request` to end the
/// stream with.
pub fn parse_header(rec: Record) -> Result<Header, Record> {
    let call = match rec {
        Record::Call(call) => call,
        other => return Err(bad(format!("a PTY stream starts with a `{METHOD}` call, not a `{}` record", kind(&other)))),
    };
    if call.method != METHOD {
        return Err(bad(format!("a PTY stream's call is `{METHOD}`, not `{}`", call.method)));
    }
    let p = &call.params;
    let text = |key: &str| p.str(key).filter(|s| !s.is_empty()).ok_or_else(|| bad(format!("the PTY header needs `{key}`")));
    let root = PathBuf::from(text("root")?);
    let instance = text("instance")?.to_string();
    let session = text("session")?.to_string();
    if !valid_run_id(&session) {
        return Err(bad(format!("bad session id `{session}`")));
    }
    let create = match p.get("create") {
        None | Some(Json::Null) => None,
        Some(Json::Obj(c)) => match c.get("argv") {
            None | Some(Json::Null) => Some(Vec::new()),
            Some(Json::Strs(argv)) => Some(argv.clone()),
            Some(_) => return Err(bad("`create.argv` must be an array of strings")),
        },
        Some(_) => return Err(bad("`create` must be an object")),
    };
    Ok(Header { root, instance, session, create })
}

/// The instance the header names, and its `state.toml` key: `root`
/// confined to the fleet root `fleet_root` as `lent::confine_path` does;
/// the instance by exact name, else by id, among those lent
/// (`lent::lent_instances`) whose config root is under `root`; with a
/// helper. Running isn't checked here (the bridge is, [`offered`]).
pub fn gate<'a>(h: &Header, state: &'a State, fleet_root: &Path) -> Result<(&'a str, &'a Instance), Record> {
    let root = lent::confine_path(fleet_root, &h.root, true).map_err(|e| match e.code {
        "invalid" => bad(e.message),
        code => Record::error(code, e.message),
    })?;
    let lent = lent::lent_instances(state, fleet_root);
    let by_name = state.instances.get_key_value(&h.instance);
    let found = by_name
        .or_else(|| state.instances.iter().find(|(_, i)| i.instance_id == h.instance))
        .filter(|(name, i)| lent.names.contains(*name) && i.config_dir.as_deref().is_some_and(|d| lent::root_under(d, &root)));
    let Some((name, instance)) = found else {
        return Err(Record::error(code::NOT_FOUND, format!("no instance `{}` in {}", h.instance, root.display())));
    };
    if instance.devsbd_arch.is_none() {
        return Err(Record::error(
            code::UNSUPPORTED,
            format!("`{name}` has no devsbd helper: a terminal over the fleet link needs it"),
        ));
    }
    Ok((name.as_str(), instance))
}

/// Whether instance `name`'s helper takes the attach, from
/// `HelperClient::offers`' answer.
pub fn offered(offers: Result<bool, CallError>, name: &str) -> Result<(), Record> {
    match offers {
        Ok(true) => Ok(()),
        // Decision 13: `session.attach` is root only in the helper.
        Ok(false) | Err(CallError::Unsupported(_)) => Err(Record::error(
            code::UNSUPPORTED,
            format!(
                "a terminal over the fleet link needs `{name}`'s helper daemon running as root \
                 (it doesn't offer `{METHOD}`: a `containerUser` instance, or an older helper)"
            ),
        )),
        Err(CallError::NoBridge(_)) => {
            Err(Record::error(code::NOT_RUNNING, format!("`{name}` isn't running (or its helper's bridge isn't up yet)")))
        }
        Err(e) => Err(Record::error(code::FAILED, format!("cannot ask `{name}`'s helper: {e}"))),
    }
}

/// `session.attach`'s params (devsbd's `rpc_sessions`): `{id, create?:
/// {argv, cwd, env, user?}}`, `create` as an exec into `instance` runs
/// (`exec::exec_context`: workspace, remote user, env with
/// `SSH_AUTH_SOCK`; `host_agent` as there).
pub fn attach_params(h: &Header, instance: &Instance, host_agent: bool) -> Obj {
    let params = Obj::new().with("id", h.session.as_str());
    let Some(argv) = &h.create else { return params };
    let argv = if argv.is_empty() { SHELL_FALLBACK_CMD.iter().map(|s| s.to_string()).collect() } else { argv.clone() };
    let ctx = exec_context(instance, host_agent);
    // `with` replaces a repeated name (the decoder refuses repeats), the
    // later one winning as with repeated `-e`s.
    let env = ctx.env.iter().fold(Obj::new(), |env, (k, v)| env.with(k, v.as_str()));
    let mut create = Obj::new().with("argv", argv).with("cwd", ctx.cwd).with("env", env);
    if let Some(user) = ctx.user {
        create = create.with("user", user);
    }
    params.with("create", create)
}

/// Serve one `PTY` stream, `link` the worker's end, to its end (see the
/// module doc). `state` is this host's `state.toml`, `scope` the link's;
/// `host_agent` probes for a host ssh-agent (`exec::has_host_agent`).
/// Refusals with `denied` are logged.
pub fn serve(link: Stream, helper: &HelperClient, state: &Path, scope: &Scope, host_agent: fn() -> bool, log: fn(&str)) {
    let caller = format!("fleet hub {}", scope.hub);
    let _ = link.set_read_timeout(Some(HEADER_WAIT));
    let first = rpc::read_record(&mut &link);
    let _ = link.set_read_timeout(None);
    let started = match first {
        Ok(Some(rec)) => start(rec, &link, helper, state, scope, host_agent, &caller),
        Ok(None) => return,
        Err(e) => Err(bad(format!("no PTY header: {e}"))),
    };
    match started {
        Ok((call, input)) => pump(&link, call, input),
        Err(rec) => {
            if let Record::Error { code: c, message } = &rec
                && c == code::DENIED
            {
                log(&format!("fleet: refused a PTY stream from hub {}: {message}", scope.hub));
            }
            let _ = rpc::write_record(&mut &link, &rec);
        }
    }
    let _ = link.shutdown(Shutdown::Both);
}

/// Header, gate, then the call: the attach and its input, or the `error`
/// to end the stream with.
fn start(
    first: Record,
    link: &Stream,
    helper: &HelperClient,
    state: &Path,
    scope: &Scope,
    host_agent: fn() -> bool,
    caller: &str,
) -> Result<(Call, Input), Record> {
    let h = parse_header(first)?;
    let state = State::load_at(state).map_err(|e| Record::error(code::FAILED, format!("{e:#}")))?;
    let (name, instance) = gate(&h, &state, &scope.root)?;
    offered(helper.offers(&instance.container, METHOD, caller), name)?;
    let params = attach_params(&h, instance, host_agent());
    // While it queues: whether the hub closed its end.
    let abandoned = || crate::serve::daemon::hung_up(link, Duration::ZERO);
    helper.call_with_input(&instance.container, METHOD, params, Obj::new(), caller, &abandoned).map_err(|e| match e {
        CallError::NoBridge(_) | CallError::Unsupported(_) => offered(Err(e), name).expect_err("a refusal"),
        CallError::Abandoned => Record::error(code::FAILED, "the hub hung up while the attach queued"),
        CallError::Io(e) => Record::error(code::LOST, format!("cannot reach `{name}`'s helper: {e}")),
    })
}

/// The attach's last record.
fn is_end(rec: &Record) -> bool {
    matches!(rec, Record::Exit(_) | Record::Error { .. } | Record::Result(_))
}

/// Relay until the attach ends: the hub's records into `input` on a thread
/// of their own, the call's records back to the hub, unchanged, up to and
/// including its end record. The hub closing its end cancels the call; a
/// call that ends without an end record is answered with [`code::LOST`].
fn pump(link: &Stream, mut call: Call, input: Input) {
    let done = AtomicBool::new(false);
    let gone = AtomicBool::new(false);
    let cancel = call.canceller();
    std::thread::scope(|s| {
        if let Ok(reader) = link.try_clone() {
            let (done, gone, cancel) = (&done, &gone, &cancel);
            s.spawn(move || feed(reader, input, done, gone, cancel));
        }
        let last = loop {
            match call.next() {
                Ok(Some(rec)) => {
                    let end = is_end(&rec);
                    // A failed write: the hub is gone; dropping the call
                    // (unfinished) cancels it.
                    if rpc::write_record(&mut &*link, &rec).is_err() || end {
                        break None;
                    }
                }
                Ok(None) => break Some(Record::error(code::LOST, "the attach ended without an end record")),
                Err(_) if gone.load(Ordering::SeqCst) => break None,
                Err(e) => break Some(Record::error(code::LOST, format!("the bridge to the instance was lost: {e}"))),
            }
        };
        if let Some(rec) = last {
            let _ = rpc::write_record(&mut &*link, &rec);
        }
        done.store(true, Ordering::SeqCst);
        // Ends `feed`'s read.
        let _ = link.shutdown(Shutdown::Both);
    });
}

/// The hub's records into the attach until the hub closes its end (or
/// sends something undecodable), which cancels the call unless it is over.
/// Records other than input are ignored, as the helper ignores them.
fn feed(mut reader: Stream, mut input: Input, done: &AtomicBool, gone: &AtomicBool, cancel: &Cancel) {
    loop {
        let sent = match rpc::read_record(&mut reader) {
            Ok(Some(Record::Stdin(bytes))) => input.stdin(&bytes),
            Ok(Some(Record::Resize { cols, rows })) => input.resize(cols, rows),
            Ok(Some(Record::StdinEof)) => input.stdin_eof(),
            Ok(Some(_)) => Ok(()),
            Ok(None) | Err(_) => {
                if !done.load(Ordering::SeqCst) {
                    gone.store(true, Ordering::SeqCst);
                    cancel.cancel();
                }
                return;
            }
        };
        // The call's stream is gone: it's ending, `pump` answers.
        if sent.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::os::unix::net::UnixStream;

    use super::*;
    use crate::devsbd::proto::caps;
    use crate::devsbd::rpc::{Call as RpcCall, Exit};
    use crate::serve::helper::tests::{Fake, log, logged};

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dsv-pty-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    const SESSION: &str = "1700000000-ab12";

    fn header(params: Obj) -> Record {
        Record::Call(RpcCall { method: METHOD.into(), params, meta: Obj::new() })
    }

    fn base(root: &str) -> Obj {
        Obj::new().with("root", root).with("instance", "web-1").with("session", SESSION)
    }

    #[test]
    fn the_header_parses_and_is_checked() {
        let h = parse_header(header(base("/f/cfg"))).unwrap();
        assert_eq!(h, Header { root: "/f/cfg".into(), instance: "web-1".into(), session: SESSION.into(), create: None });
        let shell = parse_header(header(base("/f").with("create", Obj::new()))).unwrap();
        assert_eq!(shell.create, Some(Vec::new()), "no argv: the login shell");
        let argv = parse_header(header(base("/f").with("create", Obj::new().with("argv", vec!["htop".to_string()])))).unwrap();
        assert_eq!(argv.create, Some(vec!["htop".to_string()]));
        assert_eq!(parse_header(header(base("/f").with("create", Json::Null))).unwrap().create, None);

        let refused = |rec: Record| match parse_header(rec) {
            Err(Record::Error { code, message }) => (code, message),
            other => panic!("{other:?}"),
        };
        let (c, m) = refused(Record::Stdin(b"x".to_vec()));
        assert_eq!((c.as_str(), m.contains("not a `stdin` record")), (code::BAD_REQUEST, true));
        let wrong = Record::Call(RpcCall { method: "run.start".into(), params: base("/f"), meta: Obj::new() });
        assert!(refused(wrong).1.contains("not `run.start`"));
        assert!(refused(header(base("/f").with("session", "nope"))).1.contains("bad session id"));
        assert!(refused(header(base("/f").with("instance", ""))).1.contains("needs `instance`"));
        assert!(refused(header(Obj::new().with("instance", "a").with("session", SESSION))).1.contains("needs `root`"));
        assert!(refused(header(base("/f").with("create", "sh"))).1.contains("`create` must be"));
        assert!(refused(header(base("/f").with("create", Obj::new().with("argv", "sh")))).1.contains("`create.argv`"));
    }

    /// `state.toml` text: `web-1` lent (under `cfg`, with a helper), `bare`
    /// lent without one, `own` outside the fleet root.
    fn state_with(base: &Path, root: &Path) -> State {
        let inst = |name: &str, dir: &Path, helper: bool| {
            format!(
                "[instance.{name}]\ninstance_id = \"id-{name}\"\nsandbox = \"web\"\ncontainer = \"devsandbox-{name}\"\n\
                 folder = \"/w\"\nworkspace = \"/workspaces/{name}\"\ncreated_unix = 1\nconfig_dir = {:?}\n{}",
                dir.to_str().unwrap(),
                if helper { "devsbd_arch = \"x86_64\"\n" } else { "" }
            )
        };
        let text = inst("web-1", &root.join("cfg"), true) + &inst("bare", &root.join("cfg"), false) + &inst("own", &base.join("mine"), true);
        let path = base.join("state.toml");
        std::fs::write(&path, text).unwrap();
        State::load_at(&path).unwrap()
    }

    fn code_of(r: Result<impl std::fmt::Debug, Record>) -> String {
        match r {
            Err(Record::Error { code, .. }) => code,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_gate_takes_lent_instances_with_a_helper() {
        let b = scratch("gate");
        let root = b.join("fleet/hub");
        std::fs::create_dir_all(root.join("cfg")).unwrap();
        std::fs::create_dir_all(root.join("other")).unwrap();
        std::fs::create_dir_all(b.join("mine")).unwrap();
        let state = state_with(&b, &root);
        let cfg = root.join("cfg");
        let h = |root: &Path, instance: &str| Header {
            root: root.to_path_buf(),
            instance: instance.into(),
            session: SESSION.into(),
            create: None,
        };
        let (name, i) = gate(&h(&cfg, "web-1"), &state, &root).unwrap();
        assert_eq!((name, i.container.as_str()), ("web-1", "devsandbox-web-1"));
        assert_eq!(gate(&h(&cfg, "id-web-1"), &state, &root).unwrap().0, "web-1", "by id");
        assert_eq!(gate(&h(&root, "web-1"), &state, &root).unwrap().0, "web-1", "the fleet root holds it too");
        assert_eq!(code_of(gate(&h(&cfg, "web"), &state, &root)), code::NOT_FOUND, "exact names only");
        assert_eq!(code_of(gate(&h(&cfg, "own"), &state, &root)), code::NOT_FOUND, "not lent");
        assert_eq!(code_of(gate(&h(&root.join("other"), "web-1"), &state, &root)), code::NOT_FOUND, "another root's");
        assert_eq!(code_of(gate(&h(&b.join("mine"), "own"), &state, &root)), code::DENIED, "outside the fleet root");
        assert_eq!(code_of(gate(&h(&root.join("gone"), "web-1"), &state, &root)), code::NOT_FOUND);
        assert_eq!(code_of(gate(&h(Path::new("rel"), "web-1"), &state, &root)), code::BAD_REQUEST);
        assert_eq!(code_of(gate(&h(&cfg, "bare"), &state, &root)), code::UNSUPPORTED, "no helper");
        let _ = std::fs::remove_dir_all(&b);
    }

    #[test]
    fn the_helper_must_offer_the_attach() {
        assert!(offered(Ok(true), "w").is_ok());
        let Err(Record::Error { code, message }) = offered(Ok(false), "w") else { panic!() };
        assert_eq!(code, code::UNSUPPORTED);
        assert!(message.contains("running as root"), "{message}");
        assert_eq!(code_of(offered(Err(CallError::NoBridge("c".into())), "w")), code::NOT_RUNNING);
        assert_eq!(code_of(offered(Err(CallError::Io(std::io::Error::other("x"))), "w")), code::FAILED);
    }

    #[test]
    fn create_runs_as_an_exec_would() {
        let b = scratch("params");
        let root = b.join("fleet/hub");
        std::fs::create_dir_all(root.join("cfg")).unwrap();
        let state = state_with(&b, &root);
        let mut inst = state.instances["web-1"].clone();
        let h = Header { root: root.join("cfg"), instance: "web-1".into(), session: SESSION.into(), create: None };
        assert_eq!(attach_params(&h, &inst, false), Obj::new().with("id", SESSION), "attach only");

        inst.remote_user = Some("vscode".into());
        inst.remote_env = BTreeMap::from([("A".into(), "1".into()), ("SSH_AUTH_SOCK".into(), "/mine".into())]);
        inst.ssh_auth_sock = Some("/ssh-agent".into());
        let create = Header { create: Some(Vec::new()), ..h.clone() };
        let p = attach_params(&create, &inst, false);
        let c = p.obj("create").unwrap();
        let shell: Vec<String> = SHELL_FALLBACK_CMD.iter().map(|s| s.to_string()).collect();
        assert_eq!(c.strs("argv"), Some(shell.as_slice()), "empty argv: the login shell");
        assert_eq!(c.str("cwd"), Some("/workspaces/web-1"));
        assert_eq!(c.str("user"), Some("vscode"));
        assert_eq!(c.map("env").unwrap(), [("A".to_string(), "1".to_string()), ("SSH_AUTH_SOCK".into(), "/ssh-agent".into())], "one each, the agent's last");
        // What the helper decodes: env sits at the RPC JSON's depth cap.
        let rec = Record::Call(RpcCall { method: METHOD.into(), params: p, meta: Obj::new() });
        assert_eq!(rpc::read_record(&mut &rec.encode()[..]).unwrap(), Some(rec));

        inst.remote_user = None;
        let argv = Header { create: Some(vec!["htop".into()]), ..h };
        let c = attach_params(&argv, &inst, false).obj("create").unwrap().clone();
        assert_eq!((c.strs("argv"), c.str("user")), (Some(&["htop".to_string()][..]), None), "no user: the container's");
        let _ = std::fs::remove_dir_all(&b);
    }

    /// A container's helper for [`serve`]: `rpc.methods` offering
    /// `session.attach`; the attach sends its params' `id` and `create`'s
    /// `cwd` as `stdout`, then `stdin` back as `stdout`, a `resize` as
    /// `stdout` `<cols>x<rows>`, `exit 7` at `stdin-eof`. Session
    /// `1700000000-0105` (lost) sends one `stdout` and closes without an end.
    fn sessions(s: UnixStream) {
        let mut r = &s;
        let Ok(Some(Record::Call(call))) = rpc::read_record(&mut r) else { return };
        let mut w = &s;
        let mut send = |rec: Record| rpc::write_record(&mut w, &rec);
        match call.method.as_str() {
            "rpc.methods" => {
                let _ = send(Record::Result(Obj::new().with("methods", vec!["session.attach@1".to_string()])));
            }
            "session.attach" => {
                let id = call.params.str("id").unwrap_or_default().to_string();
                let cwd = call.params.obj("create").and_then(|c| c.str("cwd")).unwrap_or("-").to_string();
                let _ = send(Record::Stdout(format!("{id} {cwd}\n").into_bytes()));
                if id == "1700000000-0105" {
                    let _ = s.shutdown(Shutdown::Both);
                    return;
                }
                loop {
                    let rec = match rpc::read_record(&mut r) {
                        Ok(Some(Record::Stdin(b))) => Record::Stdout(b),
                        Ok(Some(Record::Resize { cols, rows })) => Record::Stdout(format!("{cols}x{rows}").into_bytes()),
                        Ok(Some(Record::StdinEof)) => Record::Exit(Exit::Code(7)),
                        Ok(Some(_)) => continue,
                        // The caller hung up: detached.
                        _ => return,
                    };
                    let last = matches!(rec, Record::Exit(_));
                    let _ = send(rec);
                    if last {
                        break;
                    }
                }
            }
            m => {
                let _ = send(Record::error(rpc::code::UNKNOWN_METHOD, format!("no method `{m}`")));
            }
        }
        let _ = s.shutdown(Shutdown::Write);
        let _ = std::io::copy(&mut r, &mut std::io::sink());
    }

    /// A non-root helper: offers nothing of the sessions.
    fn not_root(s: UnixStream) {
        let mut r = &s;
        let Ok(Some(Record::Call(_))) = rpc::read_record(&mut r) else { return };
        let _ = rpc::write_record(&mut &s, &Record::Result(Obj::new().with("methods", vec!["rpc.methods@1".to_string()])));
    }

    struct World {
        base: PathBuf,
        root: PathBuf,
        state: PathBuf,
        helper: HelperClient,
    }

    impl Drop for World {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    fn world(tag: &str) -> World {
        let base = scratch(tag);
        let root = base.join("fleet/hub");
        std::fs::create_dir_all(root.join("cfg")).unwrap();
        state_with(&base, &root);
        let fake = Fake::default();
        fake.add("devsandbox-web-1", caps::RPC, 1, sessions);
        let helper = HelperClient::new(fake.clone(), log);
        // `bare` has no helper in `state.toml`; `own` isn't lent.
        World { state: base.join("state.toml"), base, root, helper }
    }

    /// The hub's end of a stream served on a thread, its header sent.
    fn open(w: &World, header: Record, helper: HelperClient) -> (UnixStream, std::thread::JoinHandle<()>) {
        let (hub, ours) = UnixStream::pair().unwrap();
        let scope = Scope { hub: "me@hub".into(), root: w.root.clone(), env: BTreeMap::new() };
        let state = w.state.clone();
        let t = std::thread::spawn(move || serve(ours, &helper, &state, &scope, || false, log));
        rpc::write_record(&mut &hub, &header).unwrap();
        hub.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        (hub, t)
    }

    fn rest(mut hub: &UnixStream) -> Vec<Record> {
        let mut recs = Vec::new();
        while let Ok(Some(rec)) = rpc::read_record(&mut hub) {
            recs.push(rec);
        }
        recs
    }

    fn create_header(w: &World, session: &str) -> Record {
        let cfg = w.root.join("cfg");
        header(base(cfg.to_str().unwrap()).with("session", session).with("create", Obj::new()))
    }

    #[test]
    fn a_stream_relays_input_and_output_to_the_end_record() {
        let w = world("relay");
        let (hub, t) = open(&w, create_header(&w, SESSION), w.helper.clone());
        let mut out = &hub;
        rpc::write_record(&mut out, &Record::Stdin(b"ls\n".to_vec())).unwrap();
        rpc::write_record(&mut out, &Record::Resize { cols: 120, rows: 40 }).unwrap();
        rpc::write_record(&mut out, &Record::Signal(2)).unwrap();
        rpc::write_record(&mut out, &Record::StdinEof).unwrap();
        let recs = rest(&hub);
        t.join().unwrap();
        let stdout = |s: &str| Record::Stdout(s.as_bytes().to_vec());
        assert_eq!(
            recs,
            [stdout(&format!("{SESSION} /workspaces/web-1\n")), stdout("ls\n"), stdout("120x40"), Record::Exit(Exit::Code(7))],
            "the helper's records, unchanged, then nothing"
        );
    }

    #[test]
    fn a_lost_bridge_ends_with_lost_and_a_hang_up_cancels() {
        let w = world("lost");
        let (hub, t) = open(&w, create_header(&w, "1700000000-0105"), w.helper.clone());
        let recs = rest(&hub);
        t.join().unwrap();
        assert_eq!(recs.len(), 2, "{recs:?}");
        assert!(matches!(&recs[1], Record::Error { code, .. } if code == code::LOST), "{recs:?}");

        // The hub closes mid-attach: the call is cancelled (the session
        // detaches and runs on), nothing more is written.
        let (hub, t) = open(&w, create_header(&w, "1700000000-0106"), w.helper.clone());
        assert!(matches!(rpc::read_record(&mut &hub), Ok(Some(Record::Stdout(_)))));
        drop(hub);
        t.join().unwrap();
        assert!(logged("helper devsandbox-web-1: session.attach from fleet hub me@hub: cancelled ("));
    }

    #[test]
    fn refusals_are_one_error_record() {
        let w = world("refuse");
        let refusal = |header: Record, helper: HelperClient| {
            let (hub, t) = open(&w, header, helper);
            let recs = rest(&hub);
            t.join().unwrap();
            match recs.as_slice() {
                [Record::Error { code, .. }] => code.clone(),
                other => panic!("{other:?}"),
            }
        };
        let cfg = w.root.join("cfg");
        let cfg = cfg.to_str().unwrap();
        assert_eq!(refusal(Record::StdinEof, w.helper.clone()), code::BAD_REQUEST);
        assert_eq!(refusal(header(base(cfg).with("instance", "own")), w.helper.clone()), code::NOT_FOUND);
        let mine = w.base.join("mine");
        std::fs::create_dir_all(&mine).unwrap();
        assert_eq!(refusal(header(base(mine.to_str().unwrap()).with("instance", "own")), w.helper.clone()), code::DENIED);
        assert!(logged("fleet: refused a PTY stream from hub me@hub"));
        assert_eq!(refusal(header(base(cfg).with("instance", "bare")), w.helper.clone()), code::UNSUPPORTED);

        // Not running: no bridge; not root: `session.attach` not offered.
        let none = HelperClient::new(Fake::default(), log);
        assert_eq!(refusal(header(base(cfg)), none), code::NOT_RUNNING);
        let fake = Fake::default();
        fake.add("devsandbox-web-1", caps::RPC, 1, not_root);
        assert_eq!(refusal(header(base(cfg)), HelperClient::new(fake, log)), code::UNSUPPORTED);
    }
}
