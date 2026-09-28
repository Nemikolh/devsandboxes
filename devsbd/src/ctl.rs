//! `devsbd ensure|ls|stop|rm`: a dispatcher's control commands
//! (docs/automations.md, "Control API"). Each sends one encoded
//! [`control::Request`] to the daemon over `daemon::API_SOCK`, which relays it
//! to a host serving `CONTROL` (or answers `NoHost` itself), and exits with the
//! response's [`control::Status::exit_code`].

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;

use crate::control::{self, Op, Request, Response, Status};
use crate::daemon;

const USAGE: &str = "usage: devsbd ensure <sandbox> --key <key> [--branch B] [--env K=V]...\n\
       devsbd ls\n\
       devsbd stop <key> [--sandbox S]\n\
       devsbd rm <key> [--sandbox S]";

/// Run control verb `verb` with its argv (after the verb); returns the exit code.
pub fn run(verb: &str, args: &[String]) -> i32 {
    let req = match parse_args(verb, args) {
        Ok(req) => req,
        Err(e) => {
            eprintln!("devsbd {verb}: {e}\n{USAGE}");
            return control::EXIT_USAGE;
        }
    };
    let resp = request(daemon::API_SOCK, &req);
    if resp.status == Status::Ok {
        if !resp.body.is_empty() {
            println!("{}", resp.body);
        }
    } else if resp.body.is_empty() {
        eprintln!("devsbd: {}", resp.status.as_str());
    } else {
        eprintln!("devsbd: {}", resp.body);
    }
    resp.status.exit_code()
}

/// Verb + argv → the request, checking only its shape (which fields each op
/// takes, env syntax); the host validates the rest (key charset, authorization).
/// Flags take `--flag value` or `--flag=value`; `--env` repeats.
fn parse_args(verb: &str, args: &[String]) -> Result<Request, String> {
    let op = Op::parse(verb).ok_or_else(|| format!("unknown command `{verb}`"))?;
    let mut req = Request { op, sandbox: None, key: None, branch: None, env: Vec::new() };
    let mut positional: Vec<String> = Vec::new();
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
            (Op::Stop | Op::Rm, "--sandbox") => true,
            _ => false,
        };
        match flag {
            f if f.starts_with('-') && f.len() > 1 && !allowed => {
                return Err(format!("unknown option `{f}`"));
            }
            "--key" => set_once(&mut req.key, value()?, flag)?,
            "--branch" => set_once(&mut req.branch, value()?, flag)?,
            "--sandbox" => set_once(&mut req.sandbox, value()?, flag)?,
            "--env" => req.env.push(control::parse_env(&value()?)?),
            _ => positional.push(arg.clone()),
        }
    }
    let wanted = if op == Op::Ls { 0 } else { 1 };
    if positional.len() != wanted {
        let what = match op {
            Op::Ls => "no arguments",
            Op::Ensure => "exactly one <sandbox>",
            Op::Stop | Op::Rm => "exactly one <key>",
        };
        return Err(format!("takes {what}"));
    }
    match op {
        Op::Ensure => {
            req.sandbox = positional.pop();
            if req.key.is_none() {
                return Err("--key is required".into());
            }
        }
        Op::Stop | Op::Rm => req.key = positional.pop(),
        Op::Ls => {}
    }
    Ok(req)
}

fn set_once(slot: &mut Option<String>, value: String, flag: &str) -> Result<(), String> {
    match slot.replace(value) {
        Some(_) => Err(format!("{flag} given twice")),
        None => Ok(()),
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

    fn parse(verb: &str, a: &[&str]) -> Result<Request, String> {
        parse_args(verb, &a.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    fn req(op: Op, sandbox: Option<&str>, key: Option<&str>) -> Request {
        Request { op, sandbox: sandbox.map(Into::into), key: key.map(Into::into), branch: None, env: vec![] }
    }

    #[test]
    fn parses_each_verb() {
        let r = parse("ensure", &["web", "--key", "pr-1", "--branch=feat/x", "--env", "A=1", "--env=B=x=y"]).unwrap();
        assert_eq!(
            r,
            Request {
                op: Op::Ensure,
                sandbox: Some("web".into()),
                key: Some("pr-1".into()),
                branch: Some("feat/x".into()),
                env: vec![("A".into(), "1".into()), ("B".into(), "x=y".into())],
            }
        );
        // Flags before the positional work too.
        assert_eq!(parse("ensure", &["--key=k", "web"]).unwrap(), req(Op::Ensure, Some("web"), Some("k")));
        assert_eq!(parse("ls", &[]).unwrap(), req(Op::Ls, None, None));
        assert_eq!(parse("stop", &["pr-1"]).unwrap(), req(Op::Stop, None, Some("pr-1")));
        assert_eq!(parse("rm", &["pr-1", "--sandbox", "web"]).unwrap(), req(Op::Rm, Some("web"), Some("pr-1")));
    }

    #[test]
    fn parsed_requests_round_trip_through_the_codec() {
        let r = parse("ensure", &["web", "--key", "pr-1", "--env", "M=a\nb"]).unwrap();
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
        assert_eq!(parse("rm", &["k", "--sandbox="]).unwrap_err(), "--sandbox needs a value");
        assert!(parse("exec", &[]).unwrap_err().contains("unknown command"));
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
