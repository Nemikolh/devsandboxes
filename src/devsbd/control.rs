//! Control requests and responses: what `devsbd ensure|ls|stop|rm` sends from
//! a dispatcher's container to the host, and what comes back
//! (docs/automations.md, "Dispatchers"). One file shared by both crates
//! (devsbd includes it via `#[path]`) so both ends can't drift; std-only and
//! hand-parsed, like `notify.rs`.
//!
//! Both are line-based UTF-8, one directive per line, `<key> <value>`, values
//! escaped as in `escape.rs` (`\\`, `\n`, `\0`). Request:
//!
//! ```text
//! op ensure             required, once: ensure|ls|stop|rm
//! sandbox web           optional, at most once
//! key pr-123            optional, at most once
//! branch feat/x         optional, at most once
//! env FOO=bar           optional, repeatable, each `K=V` (see `parse_env`)
//! ```
//!
//! Which fields an op needs is the host handler's call (`commands::dispatch`),
//! not the codec's. Response:
//!
//! ```text
//! status ok             required, once: ok|denied|failed|no-host|usage
//! body web-pr-123       required, once: instance name (ensure), JSON (ls),
//!                       else a short message; may be empty
//! ```
//!
//! Unknown keys, repeats of a non-repeatable key, a missing required key, or
//! a bad escape are decode errors.

use super::escape::{escape, unescape};

/// `devsbd` control-command exit codes (sysexits where one fits).
pub const EXIT_OK: i32 = 0;
pub const EXIT_FAILED: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
/// No host serving control is attached (`EX_TEMPFAIL`): retry later.
pub const EXIT_NO_HOST: i32 = 75;
/// The host refused the request (`EX_NOPERM`).
pub const EXIT_DENIED: i32 = 77;

/// Longest encoded request any end accepts: the daemon sends it as one `Data`
/// frame (well under `proto::MAX_PAYLOAD`), and the cap bounds a misbehaving
/// peer's buffer on the daemon and host alike.
pub const MAX_REQUEST: usize = 64 * 1024;
/// Longest encoded response the daemon and CLI accept (`ls` JSON is the big one).
pub const MAX_RESPONSE: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Ensure,
    Ls,
    Stop,
    Rm,
}

impl Op {
    pub fn parse(s: &str) -> Option<Op> {
        match s {
            "ensure" => Some(Op::Ensure),
            "ls" => Some(Op::Ls),
            "stop" => Some(Op::Stop),
            "rm" => Some(Op::Rm),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Op::Ensure => "ensure",
            Op::Ls => "ls",
            Op::Stop => "stop",
            Op::Rm => "rm",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub op: Op,
    pub sandbox: Option<String>,
    pub key: Option<String>,
    pub branch: Option<String>,
    pub env: Vec<(String, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    /// Not authorized: no `dispatcher` declaration, sandbox not in `spawn`,
    /// not the child's owner, cap reached, name taken by a foreign instance.
    Denied,
    /// The operation ran and failed (or its target doesn't exist).
    Failed,
    /// No host is serving control; answered by the daemon, never the host.
    NoHost,
    /// Malformed request: missing field, bad key, unexpected field for the op.
    Usage,
}

impl Status {
    pub fn parse(s: &str) -> Option<Status> {
        match s {
            "ok" => Some(Status::Ok),
            "denied" => Some(Status::Denied),
            "failed" => Some(Status::Failed),
            "no-host" => Some(Status::NoHost),
            "usage" => Some(Status::Usage),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Denied => "denied",
            Status::Failed => "failed",
            Status::NoHost => "no-host",
            Status::Usage => "usage",
        }
    }

    /// The `devsbd` exit code reporting this status.
    pub fn exit_code(self) -> i32 {
        match self {
            Status::Ok => EXIT_OK,
            Status::Denied => EXIT_DENIED,
            Status::Failed => EXIT_FAILED,
            Status::NoHost => EXIT_NO_HOST,
            Status::Usage => EXIT_USAGE,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: Status,
    pub body: String,
}

impl Response {
    pub fn new(status: Status, body: impl Into<String>) -> Response {
        Response { status, body: body.into() }
    }
}

/// Parse one `K=V` env assignment: `K` a portable env name
/// (`[A-Za-z_][A-Za-z0-9_]*`), `V` anything but NUL (can't reach an argv).
/// Shared by the host's hidden `run --env` flag and the request decoder.
pub fn parse_env(s: &str) -> Result<(String, String), String> {
    let (k, v) = s.split_once('=').ok_or_else(|| format!("`{s}`: expected K=V"))?;
    let mut chars = k.chars();
    let head_ok = chars.next().is_some_and(|c| c == '_' || c.is_ascii_alphabetic());
    if !head_ok || !chars.all(|c| c == '_' || c.is_ascii_alphanumeric()) {
        return Err(format!("`{k}`: not a valid env var name"));
    }
    if v.contains('\0') {
        return Err(format!("`{k}`: value contains NUL"));
    }
    Ok((k.to_string(), v.to_string()))
}

fn line(out: &mut String, key: &str, value: &str) {
    out.push_str(key);
    out.push(' ');
    escape(value, out);
    out.push('\n');
}

/// Split `text` into `(line number, key, unescaped value)` directives.
fn directives(text: &str) -> impl Iterator<Item = Result<(usize, &str, String), String>> {
    text.split('\n').enumerate().filter(|(_, l)| !l.is_empty()).map(|(n, l)| {
        let (name, rest) = l.split_once(' ').unwrap_or((l, ""));
        let value = unescape(rest).map_err(|e| format!("line {}: {e}", n + 1))?;
        Ok((n + 1, name, value))
    })
}

fn set_once(slot: &mut Option<String>, value: String, n: usize, name: &str) -> Result<(), String> {
    match slot.replace(value) {
        Some(_) => Err(format!("line {n}: repeated `{name}`")),
        None => Ok(()),
    }
}

pub fn encode_request(r: &Request) -> String {
    let mut out = String::new();
    line(&mut out, "op", r.op.as_str());
    for (key, value) in [("sandbox", &r.sandbox), ("key", &r.key), ("branch", &r.branch)] {
        if let Some(value) = value {
            line(&mut out, key, value);
        }
    }
    for (k, v) in &r.env {
        line(&mut out, "env", &format!("{k}={v}"));
    }
    out
}

pub fn decode_request(text: &str) -> Result<Request, String> {
    let (mut op, mut sandbox, mut key, mut branch) = (None, None, None, None);
    let mut env = Vec::new();
    for d in directives(text) {
        let (n, name, value) = d?;
        match name {
            "op" => {
                let o = Op::parse(&value).ok_or_else(|| format!("line {n}: bad op `{value}`"))?;
                if op.replace(o).is_some() {
                    return Err(format!("line {n}: repeated `op`"));
                }
            }
            "sandbox" => set_once(&mut sandbox, value, n, name)?,
            "key" => set_once(&mut key, value, n, name)?,
            "branch" => set_once(&mut branch, value, n, name)?,
            "env" => env.push(parse_env(&value).map_err(|e| format!("line {n}: {e}"))?),
            other => return Err(format!("line {n}: unknown key `{other}`")),
        }
    }
    Ok(Request { op: op.ok_or("missing `op`")?, sandbox, key, branch, env })
}

pub fn encode_response(r: &Response) -> String {
    let mut out = String::new();
    line(&mut out, "status", r.status.as_str());
    line(&mut out, "body", &r.body);
    out
}

pub fn decode_response(text: &str) -> Result<Response, String> {
    let (mut status, mut body) = (None, None);
    for d in directives(text) {
        let (n, name, value) = d?;
        match name {
            "status" => {
                let s = Status::parse(&value)
                    .ok_or_else(|| format!("line {n}: bad status `{value}`"))?;
                if status.replace(s).is_some() {
                    return Err(format!("line {n}: repeated `status`"));
                }
            }
            "body" => set_once(&mut body, value, n, name)?,
            other => return Err(format!("line {n}: unknown key `{other}`")),
        }
    }
    Ok(Response {
        status: status.ok_or("missing `status`")?,
        body: body.ok_or("missing `body`")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Checked from both crates (this module is compiled into each), so the
    /// helper's encoder and the host's decoder agree on the exact bytes.
    const REQUEST: &str = "op ensure\n\
sandbox web\n\
key pr-123\n\
branch feat/x\n\
env A=1\n\
env B=x=y\\nz\n";

    fn request() -> Request {
        Request {
            op: Op::Ensure,
            sandbox: Some("web".into()),
            key: Some("pr-123".into()),
            branch: Some("feat/x".into()),
            env: vec![("A".into(), "1".into()), ("B".into(), "x=y\nz".into())],
        }
    }

    #[test]
    fn request_fixture_round_trips() {
        assert_eq!(encode_request(&request()), REQUEST);
        assert_eq!(decode_request(REQUEST), Ok(request()));
    }

    #[test]
    fn minimal_request_and_every_op() {
        for op in [Op::Ensure, Op::Ls, Op::Stop, Op::Rm] {
            let r = Request { op, sandbox: None, key: None, branch: None, env: vec![] };
            let text = encode_request(&r);
            assert_eq!(text, format!("op {}\n", op.as_str()));
            assert_eq!(decode_request(&text), Ok(r));
            assert_eq!(Op::parse(op.as_str()), Some(op));
        }
    }

    #[test]
    fn response_round_trips_json_and_empty_bodies() {
        let json = Response::new(Status::Ok, "[\n  {\"name\": \"a\\\\b\"}\n]");
        let text = encode_response(&json);
        assert_eq!(text.lines().count(), 2, "one line per directive");
        assert_eq!(decode_response(&text), Ok(json));
        let empty = Response::new(Status::Failed, "");
        assert_eq!(encode_response(&empty), "status failed\nbody \n");
        assert_eq!(decode_response("body\nstatus failed\n"), Ok(empty));
    }

    #[test]
    fn statuses_parse_print_and_map_to_exit_codes() {
        let all = [Status::Ok, Status::Denied, Status::Failed, Status::NoHost, Status::Usage];
        for s in all {
            assert_eq!(Status::parse(s.as_str()), Some(s));
        }
        let codes: Vec<i32> = all.iter().map(|s| s.exit_code()).collect();
        assert_eq!(codes, [0, 77, 1, 75, 2]);
        assert_eq!(Status::parse("OK"), None);
    }

    #[test]
    fn request_rejects_malformed() {
        assert!(decode_request("").unwrap_err().contains("missing `op`"));
        assert!(decode_request("op boot\n").unwrap_err().contains("bad op"));
        assert!(decode_request("op ls\nop rm\n").unwrap_err().contains("repeated `op`"));
        let err = decode_request("op ls\nkey a\nkey b\n").unwrap_err();
        assert!(err.starts_with("line 3: repeated `key`"), "{err}");
        assert!(decode_request("op ls\nsandbox a\nsandbox a\n").is_err());
        assert!(decode_request("op ls\nbranch a\nbranch b\n").is_err());
        assert!(decode_request("op ls\nuser root\n").unwrap_err().contains("unknown key `user`"));
        assert!(decode_request("op ls\nenv NOEQ\n").unwrap_err().contains("expected K=V"));
        assert!(decode_request("op ls\nenv 1A=x\n").unwrap_err().contains("env var name"));
        assert!(decode_request("op ls\nkey a\\tb\n").unwrap_err().contains("bad escape"));
        assert!(decode_request("\u{1}\u{2}garbage").is_err());
    }

    #[test]
    fn response_rejects_malformed() {
        assert!(decode_response("body x\n").unwrap_err().contains("missing `status`"));
        assert!(decode_response("status ok\n").unwrap_err().contains("missing `body`"));
        assert!(decode_response("status meh\nbody\n").unwrap_err().contains("bad status"));
        assert!(decode_response("status ok\nbody\nbody\n").unwrap_err().contains("repeated"));
        assert!(decode_response("status ok\nstatus ok\nbody\n").is_err());
        assert!(decode_response("status ok\nbody\nextra 1\n").unwrap_err().contains("unknown"));
    }

    #[test]
    fn env_pairs() {
        assert_eq!(parse_env("A=1"), Ok(("A".into(), "1".into())));
        assert_eq!(parse_env("_x9=a=b"), Ok(("_x9".into(), "a=b".into())));
        assert_eq!(parse_env("E="), Ok(("E".into(), String::new())));
        for bad in ["", "A", "=1", "9A=1", "A-B=1", "A B=1", "A=\0"] {
            assert!(parse_env(bad).is_err(), "{bad:?}");
        }
    }
}
