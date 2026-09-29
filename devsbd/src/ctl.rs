//! `devsbd ensure|ls|stop|rm|exec` and `devsbd run ls|logs|wait|rm|prune <key> …`: a
//! dispatcher's control commands (docs/automations.md, "Control API",
//! "Runs"). Each request is one encoded [`control::Request`] sent to the
//! daemon over `daemon::API_SOCK`, which relays it to a host serving
//! `CONTROL` (or answers `NoHost` itself); the command exits with the
//! response's [`control::Status::exit_code`]. `exec` without `--detach`,
//! `run logs --follow`, and `run wait` loop over short requests (the host
//! answers each from the child's `devsbd run logs|wait`, `runs.rs`) instead of
//! holding one stream open for a run's whole life.

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use crate::control::{self, Op, Request, Response, Status};
use crate::daemon;

const USAGE: &str = "usage: devsbd ensure <sandbox> --key <key> [--branch B] [--env K=V]...\n\
       devsbd ls\n\
       devsbd stop <key> [--sandbox S]\n\
       devsbd rm <key> [--sandbox S]\n\
       devsbd exec <key> [--sandbox S] [--detach] -- <cmd>...\n\
       devsbd run ls <key> [--sandbox S]\n\
       devsbd run logs <key> <id> [--sandbox S] [--follow]\n\
       devsbd run wait <key> <id> [--sandbox S] [--timeout SECS]\n\
       devsbd run rm <key> <id> [--sandbox S] [--force]\n\
       devsbd run prune <key> [--sandbox S] [--keep N]";

/// `devsbd run <sub>` forms that name a child (`run_remote`); each is the
/// [`Op`] `run-<sub>`.
const REMOTE_RUN_SUBS: [&str; 5] = ["ls", "logs", "wait", "rm", "prune"];

/// Per-request wait while following a run: bounds how long each host
/// round trip holds a bridge thread and a `docker exec`, and the latency of
/// output that arrives while a wait is in flight.
const FOLLOW_WAIT: u64 = 5;
/// Per-request wait for `run wait` (the host caps it too).
const WAIT_STEP: u64 = 60;

/// Run control verb `verb` with its argv (after the verb); returns the exit
/// code. `verb` is an [`Op`] name: `run ls` arrives as `run-ls`.
pub fn run(verb: &str, args: &[String]) -> i32 {
    let cmd = match parse_args(verb, args) {
        Ok(cmd) => cmd,
        Err(e) => {
            let shown = verb.replacen("run-", "run ", 1);
            eprintln!("devsbd {shown}: {e}\n{USAGE}");
            return control::EXIT_USAGE;
        }
    };
    let mut send = |req: &Request| request(daemon::API_SOCK, req);
    let mut stdout = io::stdout();
    match execute(&cmd, &mut send, &mut stdout, WAIT_STEP) {
        Ok(code) => code,
        Err(resp) => report(&resp),
    }
}

/// `devsbd run <sub> <key> …` (the dispatcher form; `runs::is_local` picks).
pub fn run_remote(args: &[String]) -> i32 {
    match args.split_first() {
        Some((sub, rest)) if REMOTE_RUN_SUBS.contains(&sub.as_str()) => {
            run(&format!("run-{sub}"), rest)
        }
        _ => {
            eprintln!("devsbd run: unknown subcommand\n{USAGE}");
            control::EXIT_USAGE
        }
    }
}

/// Print a failed response's message; its exit code.
fn report(resp: &Response) -> i32 {
    if resp.body.is_empty() {
        eprintln!("devsbd: {}", resp.status.as_str());
    } else {
        eprintln!("devsbd: {}", resp.body);
    }
    resp.status.exit_code()
}

/// A parsed command line: the request plus the client-side behavior flags
/// that never reach the host.
#[derive(Debug, PartialEq, Eq)]
struct Cmd {
    req: Request,
    /// `exec --detach`: print the run id and return.
    detach: bool,
    /// `run logs --follow`: stream until the run ends.
    follow: bool,
    /// `run wait --timeout`: overall seconds (none = until the run ends).
    deadline: Option<u64>,
}

/// Verb + argv → the command, checking only its shape (which fields each op
/// takes, env syntax); the host validates the rest (key charset,
/// authorization). Flags take `--flag value` or `--flag=value`; `--env`
/// repeats; `exec`'s command follows `--`.
fn parse_args(verb: &str, args: &[String]) -> Result<Cmd, String> {
    let op = Op::parse(verb).ok_or_else(|| format!("unknown command `{verb}`"))?;
    let mut cmd = Cmd { req: Request::new(op), detach: false, follow: false, deadline: None };
    let req = &mut cmd.req;
    let mut positional: Vec<String> = Vec::new();
    let mut dashdash = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") && f.len() > 2 => (f, Some(v.to_string())),
            _ => (arg.as_str(), None),
        };
        let mut value = || match inline.clone().or_else(|| it.next().cloned()) {
            Some(v) if !v.is_empty() => Ok(v),
            _ => Err(format!("{flag} needs a value")),
        };
        let allowed = match (op, flag) {
            (Op::Ensure, "--key" | "--branch" | "--env") => true,
            (Op::Ls | Op::Ensure, _) => false,
            (_, "--sandbox") => true,
            (Op::Exec, "--detach" | "--") => true,
            (Op::RunLogs, "--follow") => true,
            (Op::RunWait, "--timeout") => true,
            (Op::RunRm, "--force") => true,
            (Op::RunPrune, "--keep") => true,
            _ => false,
        };
        match flag {
            f if f.starts_with('-') && f.len() > 1 && !allowed => {
                return Err(format!("unknown option `{f}`"));
            }
            "--" => {
                req.argv = it.by_ref().cloned().collect();
                dashdash = true;
                break;
            }
            "--detach" | "--follow" | "--force" if inline.is_some() => {
                return Err(format!("{flag} takes no value"));
            }
            "--detach" => cmd.detach = true,
            "--follow" => cmd.follow = true,
            "--force" => req.force = true,
            "--keep" => {
                let v = value()?;
                let n = v.parse().map_err(|_| format!("--keep: bad number `{v}`"))?;
                set_once(&mut req.keep, n, flag)?;
            }
            "--timeout" => {
                let v = value()?;
                let secs = v.parse().map_err(|_| format!("--timeout: bad number `{v}`"))?;
                set_once(&mut cmd.deadline, secs, flag)?;
            }
            "--key" => set_once(&mut req.key, value()?, flag)?,
            "--branch" => {
                let b = value()?;
                if !control::valid_branch(&b) {
                    return Err(format!("bad --branch: use {}", control::BRANCH_RULES));
                }
                set_once(&mut req.branch, b, flag)?
            }
            "--sandbox" => set_once(&mut req.sandbox, value()?, flag)?,
            "--env" => req.env.push(control::parse_env(&value()?)?),
            _ => positional.push(arg.clone()),
        }
    }
    let (wanted, what) = match op {
        Op::Ls => (0, "no arguments"),
        Op::Ensure => (1, "exactly one <sandbox>"),
        Op::Stop | Op::Rm | Op::Exec | Op::RunLs | Op::RunPrune => (1, "exactly one <key>"),
        Op::RunLogs | Op::RunWait | Op::RunRm => (2, "<key> <id>"),
    };
    if positional.len() != wanted {
        return Err(format!("takes {what}"));
    }
    let mut positional = positional.into_iter();
    match op {
        Op::Ensure => {
            req.sandbox = positional.next();
            if req.key.is_none() {
                return Err("--key is required".into());
            }
        }
        Op::Ls => {}
        _ => req.key = positional.next(),
    }
    if let Some(id) = positional.next() {
        if !control::valid_run_id(&id) {
            return Err(format!("bad run id `{id}`"));
        }
        req.id = Some(id);
    }
    if op == Op::Exec && (!dashdash || req.argv.is_empty()) {
        return Err("missing `-- <cmd>...`".into());
    }
    Ok(cmd)
}

fn set_once<T>(slot: &mut Option<T>, value: T, flag: &str) -> Result<(), String> {
    match slot.replace(value) {
        Some(_) => Err(format!("{flag} given twice")),
        None => Ok(()),
    }
}

/// Carry out `cmd` over `send` (one request → one response), writing output
/// to `out`; `Ok` is the exit code, `Err` a failed response to report.
/// `wait_step` is `run wait`'s per-request timeout (short in tests).
fn execute(
    cmd: &Cmd,
    send: &mut dyn FnMut(&Request) -> Response,
    out: &mut dyn Write,
    wait_step: u64,
) -> Result<i32, Response> {
    let req = &cmd.req;
    let ok = |resp: Response| if resp.status == Status::Ok { Ok(resp.body) } else { Err(resp) };
    let io_err = |e: io::Error| Response::new(Status::Failed, format!("stdout: {e}"));
    match req.op {
        Op::Exec => {
            let id = ok(send(req))?;
            if cmd.detach {
                writeln!(out, "{id}").map_err(io_err)?;
                return Ok(control::EXIT_OK);
            }
            eprintln!("devsbd: run {id}");
            let state = follow(req, &id, send, out)?;
            Ok(exit_code_of(&state))
        }
        Op::RunLogs => {
            let id = req.id.clone().unwrap_or_default();
            if cmd.follow {
                follow(req, &id, send, out)?;
            } else {
                drain(req, &id, &mut 0, send, out)?;
            }
            Ok(control::EXIT_OK)
        }
        Op::RunWait => {
            let id = req.id.clone().unwrap_or_default();
            let deadline = cmd.deadline.map(|s| Instant::now() + Duration::from_secs(s));
            let state = loop {
                let left = deadline.map(|d| d.saturating_duration_since(Instant::now()).as_secs());
                let step = left.map_or(wait_step, |l| l.min(wait_step));
                let state = wait_once(req, &id, step, send)?;
                if state != "running" || left.is_some_and(|l| l <= step) {
                    break state;
                }
            };
            writeln!(out, "{state}").map_err(io_err)?;
            Ok(control::EXIT_OK)
        }
        _ => {
            let body = ok(send(req))?;
            if !body.is_empty() {
                writeln!(out, "{body}").map_err(io_err)?;
            }
            Ok(control::EXIT_OK)
        }
    }
}

/// `base` retargeted at run `id` of the same child, as `op`.
fn run_req(base: &Request, op: Op, id: &str) -> Request {
    Request { sandbox: base.sandbox.clone(), key: base.key.clone(), id: Some(id.into()), ..Request::new(op) }
}

fn wait_once(
    base: &Request,
    id: &str,
    secs: u64,
    send: &mut dyn FnMut(&Request) -> Response,
) -> Result<String, Response> {
    let req = Request { timeout: Some(secs), ..run_req(base, Op::RunWait, id) };
    let resp = send(&req);
    if resp.status != Status::Ok {
        return Err(resp);
    }
    Ok(resp.body.trim().to_string())
}

/// Write the run's output from `*offset` until the host has no more,
/// advancing `*offset` (bytes of the log).
fn drain(
    base: &Request,
    id: &str,
    offset: &mut u64,
    send: &mut dyn FnMut(&Request) -> Response,
    out: &mut dyn Write,
) -> Result<(), Response> {
    loop {
        let req = Request { offset: Some(*offset), ..run_req(base, Op::RunLogs, id) };
        let resp = send(&req);
        if resp.status != Status::Ok {
            return Err(resp);
        }
        let (next, text) = control::parse_logs_body(&resp.body)
            .map_err(|e| Response::new(Status::Failed, format!("bad logs reply: {e}")))?;
        if next <= *offset {
            return Ok(());
        }
        out.write_all(text.as_bytes())
            .and_then(|()| out.flush())
            .map_err(|e| Response::new(Status::Failed, format!("stdout: {e}")))?;
        *offset = next;
    }
}

/// Stream run `id`'s output until it ends; its final state (`exited N`, …).
/// Drains once more after the end so the tail isn't lost.
fn follow(
    base: &Request,
    id: &str,
    send: &mut dyn FnMut(&Request) -> Response,
    out: &mut dyn Write,
) -> Result<String, Response> {
    let mut offset = 0;
    let mut done = None;
    loop {
        drain(base, id, &mut offset, send, out)?;
        if let Some(state) = done {
            return Ok(state);
        }
        let state = wait_once(base, id, FOLLOW_WAIT, send)?;
        if state != "running" {
            done = Some(state);
        }
    }
}

/// A run's exit code from its state: `exited N` → N, `killed N` → 128 + N
/// (as a shell reports it), `lost` or anything else → 1 with a note.
fn exit_code_of(state: &str) -> i32 {
    let num = |s: &str| s.parse::<i32>().ok();
    match state.split_once(' ') {
        Some(("exited", n)) if num(n).is_some() => num(n).unwrap_or(1),
        Some(("killed", n)) if num(n).is_some() => 128 + num(n).unwrap_or(0),
        _ => {
            eprintln!("devsbd: run {state}");
            control::EXIT_FAILED
        }
    }
}

/// One round trip over the API socket at `sock`. Never fails: a missing
/// daemon is `NoHost`, anything else unexpected `Failed`.
fn request(sock: &str, req: &Request) -> Response {
    let Ok(conn) = UnixStream::connect(sock) else {
        return Response::new(Status::NoHost, "no host connected (devsbd daemon not running)");
    };
    match exchange(conn, req) {
        Ok(resp) => resp,
        Err(e) => Response::new(Status::Failed, e),
    }
}

fn exchange(mut conn: UnixStream, req: &Request) -> Result<Response, String> {
    let mut msg = vec![daemon::API_CONTROL];
    msg.extend_from_slice(control::encode_request(req).as_bytes());
    let io_err = |e: io::Error| format!("daemon connection: {e}");
    conn.write_all(&msg).map_err(io_err)?;
    conn.shutdown(Shutdown::Write).map_err(io_err)?;
    let mut buf = Vec::new();
    (&conn).take(control::MAX_RESPONSE as u64 + 1).read_to_end(&mut buf).map_err(io_err)?;
    if buf.is_empty() {
        // A daemon predating the control API drops the unknown verb.
        return Err("daemon closed the connection without a reply (outdated helper? restart the instance)".into());
    }
    if buf.len() > control::MAX_RESPONSE {
        return Err("response too large".into());
    }
    let text = std::str::from_utf8(&buf).map_err(|_| "bad response: not UTF-8".to_string())?;
    control::decode_response(text).map_err(|e| format!("bad response: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "1790000000-a1b2";

    fn parse(verb: &str, a: &[&str]) -> Result<Request, String> {
        parse_cmd(verb, a).map(|c| c.req)
    }

    fn parse_cmd(verb: &str, a: &[&str]) -> Result<Cmd, String> {
        parse_args(verb, &a.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    fn req(op: Op, sandbox: Option<&str>, key: Option<&str>) -> Request {
        Request { sandbox: sandbox.map(Into::into), key: key.map(Into::into), ..Request::new(op) }
    }

    #[test]
    fn parses_each_verb() {
        let r = parse("ensure", &["web", "--key", "pr-1", "--branch=feat/x", "--env", "A=1", "--env=B=x=y"]).unwrap();
        assert_eq!(
            r,
            Request {
                sandbox: Some("web".into()),
                key: Some("pr-1".into()),
                branch: Some("feat/x".into()),
                env: vec![("A".into(), "1".into()), ("B".into(), "x=y".into())],
                ..Request::new(Op::Ensure)
            }
        );
        // Flags before the positional work too.
        assert_eq!(parse("ensure", &["--key=k", "web"]).unwrap(), req(Op::Ensure, Some("web"), Some("k")));
        assert_eq!(parse("ls", &[]).unwrap(), req(Op::Ls, None, None));
        assert_eq!(parse("stop", &["pr-1"]).unwrap(), req(Op::Stop, None, Some("pr-1")));
        assert_eq!(parse("rm", &["pr-1", "--sandbox", "web"]).unwrap(), req(Op::Rm, Some("web"), Some("pr-1")));
    }

    #[test]
    fn parses_run_verbs() {
        let c = parse_cmd("exec", &["pr-1", "--sandbox=web", "--", "zidane", "-p", "--sandbox x"]).unwrap();
        assert!(!c.detach);
        assert_eq!(
            c.req,
            Request { argv: vec!["zidane".into(), "-p".into(), "--sandbox x".into()], ..req(Op::Exec, Some("web"), Some("pr-1")) }
        );
        let c = parse_cmd("exec", &["--detach", "pr-1", "--", "true"]).unwrap();
        assert!(c.detach);
        assert_eq!(parse("run-ls", &["pr-1"]).unwrap(), req(Op::RunLs, None, Some("pr-1")));
        let c = parse_cmd("run-logs", &["pr-1", ID, "--follow"]).unwrap();
        assert!(c.follow);
        assert_eq!(c.req, Request { id: Some(ID.into()), ..req(Op::RunLogs, None, Some("pr-1")) });
        let c = parse_cmd("run-wait", &["pr-1", ID, "--timeout", "30"]).unwrap();
        assert_eq!(c.deadline, Some(30));
        assert_eq!(c.req.timeout, None, "the overall timeout stays client-side");
        assert_eq!(parse("run-rm", &["pr-1", ID]).unwrap(), Request { id: Some(ID.into()), ..req(Op::RunRm, None, Some("pr-1")) });
        assert_eq!(
            parse("run-rm", &["--force", "pr-1", "--sandbox=web", ID]).unwrap(),
            Request { id: Some(ID.into()), force: true, ..req(Op::RunRm, Some("web"), Some("pr-1")) }
        );
        assert_eq!(parse("run-prune", &["pr-1"]).unwrap(), req(Op::RunPrune, None, Some("pr-1")));
        assert_eq!(
            parse("run-prune", &["pr-1", "--keep", "5", "--sandbox", "web"]).unwrap(),
            Request { keep: Some(5), ..req(Op::RunPrune, Some("web"), Some("pr-1")) }
        );
        assert_eq!(parse("run-prune", &["--keep=0", "pr-1"]).unwrap().keep, Some(0));
        let r = parse("run-rm", &["k", ID, "--force"]).unwrap();
        assert_eq!(control::decode_request(&control::encode_request(&r)), Ok(r));
    }

    /// Every `devsbd run <sub> <key> …` form maps to an op `run_remote` can
    /// send, and `main`'s local/remote split sends the child forms here.
    #[test]
    fn remote_run_subcommands_are_ops() {
        for sub in REMOTE_RUN_SUBS {
            assert!(Op::parse(&format!("run-{sub}")).is_some(), "{sub}");
        }
        let s = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        for remote in [
            &["rm", "pr-1", ID][..],
            &["rm", "pr-1", ID, "--force", "--sandbox", "web"],
            &["prune", "pr-1"],
            &["prune", "pr-1", "--keep", "5"],
            &["prune", "--keep", "5", "--sandbox", "web", "pr-1"],
        ] {
            let args = s(remote);
            assert!(!crate::runs::is_local(&args), "{remote:?}");
            let verb = format!("run-{}", args[0]);
            assert!(parse_args(&verb, &args[1..]).is_ok(), "{remote:?}");
        }
    }

    #[test]
    fn parsed_requests_round_trip_through_the_codec() {
        let r = parse("ensure", &["web", "--key", "pr-1", "--env", "M=a\nb"]).unwrap();
        assert_eq!(control::decode_request(&control::encode_request(&r)), Ok(r));
        let r = parse("exec", &["k", "--", "sh", "-c", "a\nb", ""]).unwrap();
        assert_eq!(control::decode_request(&control::encode_request(&r)), Ok(r));
    }

    #[test]
    fn usage_errors() {
        assert!(parse("ensure", &["web"]).unwrap_err().contains("--key is required"));
        assert!(parse("ensure", &["--key", "k"]).unwrap_err().contains("exactly one <sandbox>"));
        assert!(parse("ensure", &["a", "b", "--key", "k"]).unwrap_err().contains("exactly one"));
        assert_eq!(parse("ensure", &["web", "--key"]).unwrap_err(), "--key needs a value");
        assert_eq!(parse("ensure", &["web", "--key", "a", "--key", "b"]).unwrap_err(), "--key given twice");
        assert!(parse("ensure", &["web", "--key", "k", "--env", "1A=x"]).unwrap_err().contains("env var name"));
        assert!(parse("ensure", &["web", "--key", "k", "--env", "NOEQ"]).unwrap_err().contains("K=V"));
        assert_eq!(parse("ensure", &["web", "--key", "k", "--sandbox", "x"]).unwrap_err(), "unknown option `--sandbox`");
        assert_eq!(parse("ls", &["x"]).unwrap_err(), "takes no arguments");
        assert_eq!(parse("ls", &["--key", "k"]).unwrap_err(), "unknown option `--key`");
        assert!(parse("stop", &[]).unwrap_err().contains("exactly one <key>"));
        assert_eq!(parse("rm", &["k", "--branch", "b"]).unwrap_err(), "unknown option `--branch`");
        for bad in ["x-${localEnv:GITHUB_TOKEN}", "a..b", "x.lock", "-x"] {
            let err = parse("ensure", &["web", "--key", "k", &format!("--branch={bad}")]).unwrap_err();
            assert!(err.starts_with("bad --branch: use "), "{bad:?}: {err}");
        }
        assert_eq!(parse("rm", &["k", "--sandbox="]).unwrap_err(), "--sandbox needs a value");
        assert!(parse("run", &[]).unwrap_err().contains("unknown command"));
        // Run verbs.
        assert!(parse("exec", &["k"]).unwrap_err().contains("missing `-- <cmd>"));
        assert!(parse("exec", &["k", "--"]).unwrap_err().contains("missing"));
        assert!(parse("exec", &["--", "true"]).unwrap_err().contains("exactly one <key>"));
        assert!(parse("exec", &["k", "true"]).unwrap_err().contains("exactly one <key>"));
        assert_eq!(parse("exec", &["k", "--detach=1", "--", "x"]).unwrap_err(), "--detach takes no value");
        assert_eq!(parse("exec", &["k", "--follow", "--", "x"]).unwrap_err(), "unknown option `--follow`");
        assert_eq!(parse("stop", &["k", "--", "x"]).unwrap_err(), "unknown option `--`");
        assert!(parse("run-logs", &["k"]).unwrap_err().contains("<key> <id>"));
        assert_eq!(parse("run-logs", &["k", "nope"]).unwrap_err(), "bad run id `nope`");
        assert_eq!(parse("run-logs", &["k", ID, "--timeout", "1"]).unwrap_err(), "unknown option `--timeout`");
        assert!(parse("run-wait", &["k", ID, "--timeout", "x"]).unwrap_err().contains("bad number"));
        assert_eq!(parse("run-ls", &["k", ID]).unwrap_err(), "takes exactly one <key>");
        assert!(parse("run-rm", &["k"]).unwrap_err().contains("<key> <id>"));
        assert_eq!(parse("run-rm", &["k", "nope"]).unwrap_err(), "bad run id `nope`");
        assert_eq!(parse("run-rm", &["k", ID, "--force=1"]).unwrap_err(), "--force takes no value");
        assert_eq!(parse("run-rm", &["k", ID, "--keep", "1"]).unwrap_err(), "unknown option `--keep`");
        assert_eq!(parse("run-prune", &["k", ID]).unwrap_err(), "takes exactly one <key>");
        assert_eq!(parse("run-prune", &[]).unwrap_err(), "takes exactly one <key>");
        assert_eq!(parse("run-prune", &["k", "--force"]).unwrap_err(), "unknown option `--force`");
        assert!(parse("run-prune", &["k", "--keep", "-1"]).unwrap_err().contains("bad number"));
        assert_eq!(parse("run-prune", &["k", "--keep"]).unwrap_err(), "--keep needs a value");
        assert_eq!(parse("run-prune", &["k", "--keep", "1", "--keep=2"]).unwrap_err(), "--keep given twice");
        assert_eq!(parse("stop", &["k", "--force"]).unwrap_err(), "unknown option `--force`");
    }

    /// A fake host over a scripted run: `log` is the whole output, `states`
    /// the successive `run-wait` answers; records every request.
    struct FakeHost {
        log: Vec<u8>,
        /// How much of `log` is "written" so far; grows per wait.
        visible: usize,
        states: Vec<&'static str>,
        seen: Vec<Request>,
        chunk: usize,
    }

    impl FakeHost {
        fn send(&mut self, r: &Request) -> Response {
            self.seen.push(r.clone());
            match r.op {
                Op::Exec => Response::new(Status::Ok, ID),
                Op::RunLogs => {
                    let from = r.offset.unwrap_or(0) as usize;
                    let to = self.visible.min(from + self.chunk).max(from);
                    Response::new(Status::Ok, control::logs_body(from as u64, &self.log[from..to]))
                }
                Op::RunWait => {
                    self.visible = (self.visible + 4).min(self.log.len());
                    let s = if self.states.len() > 1 { self.states.remove(0) } else { self.states[0] };
                    Response::new(Status::Ok, s)
                }
                _ => Response::new(Status::Ok, "ls-body"),
            }
        }
    }

    fn host(log: &str, states: Vec<&'static str>) -> FakeHost {
        FakeHost { log: log.as_bytes().to_vec(), visible: 0, states, seen: Vec::new(), chunk: 3 }
    }

    fn exec_with(fake: &mut FakeHost, cmd: &Cmd) -> (Result<i32, Response>, String) {
        let mut out = Vec::new();
        let r = execute(cmd, &mut |r| fake.send(r), &mut out, 1);
        (r, String::from_utf8(out).unwrap())
    }

    #[test]
    fn exec_streams_output_and_exits_with_the_runs_code() {
        let mut fake = host("line one\nline two\n", vec!["running", "running", "exited 4"]);
        let cmd = parse_cmd("exec", &["pr-1", "--sandbox", "web", "--", "sh"]).unwrap();
        let (code, out) = exec_with(&mut fake, &cmd);
        assert_eq!(code, Ok(4));
        // Everything visible by the end, including what arrived after the
        // last `running`, in order and exactly once.
        assert_eq!(out, "line one\nlin");
        // Every follow-up targets the same child and run.
        assert!(fake.seen[1..].iter().all(|r| r.key.as_deref() == Some("pr-1")
            && r.sandbox.as_deref() == Some("web")
            && r.id.as_deref() == Some(ID)));
        assert!(fake.seen.iter().any(|r| r.op == Op::RunWait && r.timeout == Some(FOLLOW_WAIT)));

        let mut fake = host("", vec!["killed 9"]);
        assert_eq!(exec_with(&mut fake, &cmd).0, Ok(137));
        let mut fake = host("", vec!["lost"]);
        assert_eq!(exec_with(&mut fake, &cmd).0, Ok(1));

        let detach = parse_cmd("exec", &["pr-1", "--detach", "--", "sh"]).unwrap();
        let mut fake = host("x", vec!["running"]);
        assert_eq!(exec_with(&mut fake, &detach), (Ok(0), format!("{ID}\n")));
        assert_eq!(fake.seen.len(), 1, "detach sends only the exec");
    }

    #[test]
    fn logs_drain_or_follow() {
        let mut fake = host("abcdefghij", vec!["running", "exited 0"]);
        fake.visible = 5;
        let cmd = parse_cmd("run-logs", &["k", ID]).unwrap();
        assert_eq!(exec_with(&mut fake, &cmd), (Ok(0), "abcde".into()));
        assert!(fake.seen.iter().all(|r| r.op == Op::RunLogs), "no wait without --follow");
        let offsets: Vec<_> = fake.seen.iter().map(|r| r.offset).collect();
        assert_eq!(offsets, [Some(0), Some(3), Some(5)]);

        let mut fake = host("abcdefghij", vec!["running", "exited 3"]);
        let cmd = parse_cmd("run-logs", &["k", ID, "--follow"]).unwrap();
        assert_eq!(exec_with(&mut fake, &cmd), (Ok(0), "abcdefgh".into()), "exit 0, not the run's");
    }

    #[test]
    fn wait_loops_until_done_or_deadline() {
        let mut fake = host("", vec!["running", "running", "exited 2"]);
        let cmd = parse_cmd("run-wait", &["k", ID]).unwrap();
        assert_eq!(exec_with(&mut fake, &cmd), (Ok(0), "exited 2\n".into()));
        assert_eq!(fake.seen.len(), 3);
        let mut fake = host("", vec!["running"]);
        let cmd = parse_cmd("run-wait", &["k", ID, "--timeout", "0"]).unwrap();
        assert_eq!(exec_with(&mut fake, &cmd), (Ok(0), "running\n".into()));
        assert_eq!(fake.seen[0].timeout, Some(0));
    }

    #[test]
    fn failures_stop_the_loop_with_their_status() {
        let cmd = parse_cmd("exec", &["k", "--", "sh"]).unwrap();
        let mut n = 0;
        let mut send = |r: &Request| {
            n += 1;
            match r.op {
                Op::Exec => Response::new(Status::Ok, ID),
                _ => Response::new(Status::NoHost, "no host connected"),
            }
        };
        let r = execute(&cmd, &mut send, &mut Vec::new(), 1);
        assert_eq!(r.unwrap_err().status, Status::NoHost);
        assert_eq!(n, 2);
        let mut send = |_: &Request| Response::new(Status::Ok, "garbage");
        let cmd = parse_cmd("run-logs", &["k", ID]).unwrap();
        let err = execute(&cmd, &mut send, &mut Vec::new(), 1).unwrap_err();
        assert!(err.body.contains("bad logs reply"), "{err:?}");
    }

    #[test]
    fn exit_codes_from_states() {
        assert_eq!(exit_code_of("exited 0"), 0);
        assert_eq!(exit_code_of("exited 4"), 4);
        assert_eq!(exit_code_of("killed 15"), 143);
        assert_eq!(exit_code_of("lost"), 1);
        assert_eq!(exit_code_of("exited x"), 1);
    }

    #[test]
    fn missing_daemon_is_no_host() {
        let resp = request("/nonexistent/devsbd-api.sock", &req(Op::Ls, None, None));
        assert_eq!(resp.status, Status::NoHost);
        assert_eq!(resp.status.exit_code(), control::EXIT_NO_HOST);
        assert!(resp.body.contains("no host connected"), "{}", resp.body);
    }

    /// Against a fake daemon: the verb byte + encoded request go out, the
    /// reply comes back decoded; an empty reply (old daemon) is `Failed`.
    #[test]
    fn exchange_sends_verb_and_request_and_decodes_the_reply() {
        let (client, mut daemon) = UnixStream::pair().unwrap();
        let fake = std::thread::spawn(move || {
            let mut got = Vec::new();
            daemon.read_to_end(&mut got).unwrap();
            daemon.write_all(b"status ok\nbody web-pr-1\n").unwrap();
            got
        });
        let r = req(Op::Ensure, Some("web"), Some("pr-1"));
        assert_eq!(exchange(client, &r), Ok(Response::new(Status::Ok, "web-pr-1")));
        let got = fake.join().unwrap();
        assert_eq!(got[0], daemon::API_CONTROL);
        assert_eq!(control::decode_request(std::str::from_utf8(&got[1..]).unwrap()), Ok(r));

        let (client, mut daemon) = UnixStream::pair().unwrap();
        let old = std::thread::spawn(move || daemon.read_to_end(&mut Vec::new()));
        assert!(exchange(client, &req(Op::Ls, None, None)).unwrap_err().contains("without a reply"));
        let _ = old.join();
    }
}
