//! Control requests and responses: what `devsbd ensure|ls|stop|rm|exec|run`
//! sends from a dispatcher's container to the host, and what comes back
//! (docs/automations.md, "Dispatchers"). One file shared by both crates
//! (devsbd includes it via `#[path]`) so both ends can't drift; std-only and
//! hand-parsed, like `notify.rs`.
//!
//! Both are line-based UTF-8, one directive per line, `<key> <value>`, values
//! escaped as in `escape.rs` (`\\`, `\n`, `\0`). Request:
//!
//! ```text
//! op ensure             required, once: ensure|ls|stop|rm|exec|run-ls|run-logs|run-wait|run-rm|run-prune
//! sandbox web           optional, at most once
//! key pr-123            optional, at most once
//! branch feat/x         optional, at most once
//! env FOO=bar           optional, repeatable, each `K=V` (see `parse_env`)
//! arg zidane            optional, repeatable: `exec`'s command, one line per argv word
//! id 1790000000-a1b2    optional, at most once: a run id (see `valid_run_id`)
//! offset 1024           optional, at most once: decimal byte offset into a run's log
//! timeout 5             optional, at most once: decimal seconds
//! force                 optional, at most once, no value: a flag (`run-rm --force`)
//! keep 5                optional, at most once: decimal count of runs to keep
//! ```
//!
//! Which fields an op needs is the host handler's call (`commands::dispatch`),
//! not the codec's. Response:
//!
//! ```text
//! status ok             required, once: ok|denied|failed|no-host|usage
//! body web-pr-123       required, once: instance name (ensure), JSON (ls),
//!                       run id (exec), `devsbd run ls|wait` output (run-ls,
//!                       run-wait, run-prune), `<next offset>\n<log text>` (run-logs),
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
    /// Start a run in a child (`devsbd run start` there); body = run id.
    Exec,
    /// A child's runs (`devsbd run ls` there).
    RunLs,
    /// A chunk of a run's output from `offset`.
    RunLogs,
    /// Wait up to `timeout` for a run to end; body = its state.
    RunWait,
    /// Delete a run (`force`: kill it first if running).
    RunRm,
    /// Delete a child's ended runs but the newest `keep`; body = the count.
    RunPrune,
}

impl Op {
    pub const ALL: [Op; 10] = [
        Op::Ensure,
        Op::Ls,
        Op::Stop,
        Op::Rm,
        Op::Exec,
        Op::RunLs,
        Op::RunLogs,
        Op::RunWait,
        Op::RunRm,
        Op::RunPrune,
    ];

    pub fn parse(s: &str) -> Option<Op> {
        Op::ALL.into_iter().find(|op| op.as_str() == s)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Op::Ensure => "ensure",
            Op::Ls => "ls",
            Op::Stop => "stop",
            Op::Rm => "rm",
            Op::Exec => "exec",
            Op::RunLs => "run-ls",
            Op::RunLogs => "run-logs",
            Op::RunWait => "run-wait",
            Op::RunRm => "run-rm",
            Op::RunPrune => "run-prune",
        }
    }
}

/// A run id as the helper mints them: `<unix secs:010>-<4 lowercase hex>`,
/// so a name sort is start order. Checked on both ends: it becomes a path
/// segment in the child.
pub fn valid_run_id(id: &str) -> bool {
    let b = id.as_bytes();
    b.len() == 15
        && b[..10].iter().all(u8::is_ascii_digit)
        && b[10] == b'-'
        && b[11..].iter().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c))
}

/// Longest branch name [`valid_branch`] accepts.
pub const MAX_BRANCH: usize = 200;

/// Short statement of the [`valid_branch`] rules, for error messages.
pub const BRANCH_RULES: &str = "1-200 chars of [A-Za-z0-9._/-], not starting with `-`, `/` or `.`, \
not ending with `/` or `.`, no `..` or `//`, no component starting with `.` or ending in `.lock`";

/// A branch name a dispatcher may request: git check-ref-format `--branch`
/// rules, checked purely, over a stricter charset. The value is data: it
/// reaches host git as an argv word and `run --branch`, so anything that
/// could read as an option (`-x`), a template (`${…}`), a revision
/// expression (`@{`, `..`, `^`, `~`) or a path escape is refused.
pub fn valid_branch(s: &str) -> bool {
    let charset = |c: u8| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'/' | b'-');
    (1..=MAX_BRANCH).contains(&s.len())
        && s.bytes().all(charset)
        && !s.starts_with('-')
        && !s.ends_with('.')
        && !s.contains("..")
        // Empty components cover leading/trailing `/` and `//`.
        && s.split('/').all(|c| !c.is_empty() && !c.starts_with('.') && !c.ends_with(".lock"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub op: Op,
    pub sandbox: Option<String>,
    pub key: Option<String>,
    pub branch: Option<String>,
    pub env: Vec<(String, String)>,
    pub argv: Vec<String>,
    pub id: Option<String>,
    pub offset: Option<u64>,
    pub timeout: Option<u64>,
    pub force: bool,
    pub keep: Option<u64>,
}

impl Request {
    /// `op` with every field empty.
    pub fn new(op: Op) -> Request {
        Request {
            op,
            sandbox: None,
            key: None,
            branch: None,
            env: Vec::new(),
            argv: Vec::new(),
            id: None,
            offset: None,
            timeout: None,
            force: false,
            keep: None,
        }
    }
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

/// Env names a dispatcher may not set on a child ([`denied_env`]).
const DENIED_ENV: &[&str] = &[
    "PATH", "HOME", "SHELL", "USER", "ENV", "BASH_ENV", "IFS", "CDPATH", "PS4",
    "PROMPT_COMMAND", "SSH_AUTH_SOCK", "TMPDIR", "GCONV_PATH", "NODE_OPTIONS", "RUBYOPT",
];
/// Env name prefixes denied the same way.
const DENIED_ENV_PREFIXES: &[&str] = &["LD_", "DYLD_", "GIT_", "PYTHON", "PERL5"];

/// Whether a dispatcher's `--env` may not set `name`: variables that steer
/// which binary or library a process loads, or what a shell runs on its own.
/// A child's env reaches every exec in it, root ones included (the host's
/// helper install/boot/daemon execs), so e.g. `PATH=/tmp/p` plus a planted
/// binary would run as root on the next `start`. Matched case-insensitively:
/// Linux names are case-sensitive, but no legitimate use needs `Path` either.
pub fn denied_env(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    DENIED_ENV.contains(&upper.as_str()) || DENIED_ENV_PREFIXES.iter().any(|p| upper.starts_with(p))
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

fn set_once<T>(slot: &mut Option<T>, value: T, n: usize, name: &str) -> Result<(), String> {
    match slot.replace(value) {
        Some(_) => Err(format!("line {n}: repeated `{name}`")),
        None => Ok(()),
    }
}

/// Plain decimal digits only (no sign, no spaces): `u64::from_str` alone
/// accepts a leading `+`.
fn number(value: &str, n: usize, name: &str) -> Result<u64, String> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("line {n}: bad `{name}` `{value}`"));
    }
    value.parse().map_err(|_| format!("line {n}: `{name}` out of range"))
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
    for arg in &r.argv {
        line(&mut out, "arg", arg);
    }
    if let Some(id) = &r.id {
        line(&mut out, "id", id);
    }
    for (key, value) in [("offset", r.offset), ("timeout", r.timeout)] {
        if let Some(value) = value {
            line(&mut out, key, &value.to_string());
        }
    }
    if r.force {
        out.push_str("force\n");
    }
    if let Some(keep) = r.keep {
        line(&mut out, "keep", &keep.to_string());
    }
    out
}

pub fn decode_request(text: &str) -> Result<Request, String> {
    let (mut op, mut sandbox, mut key, mut branch) = (None, None, None, None);
    let mut env = Vec::new();
    let (mut argv, mut id, mut offset, mut timeout) = (Vec::new(), None, None, None);
    let (mut force, mut keep) = (None, None);
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
            "arg" => argv.push(value),
            "id" => set_once(&mut id, value, n, name)?,
            "offset" => set_once(&mut offset, number(&value, n, name)?, n, name)?,
            "timeout" => set_once(&mut timeout, number(&value, n, name)?, n, name)?,
            "force" if value.is_empty() => set_once(&mut force, (), n, name)?,
            "force" => return Err(format!("line {n}: `force` takes no value")),
            "keep" => set_once(&mut keep, number(&value, n, name)?, n, name)?,
            other => return Err(format!("line {n}: unknown key `{other}`")),
        }
    }
    Ok(Request {
        op: op.ok_or("missing `op`")?,
        sandbox,
        key,
        branch,
        env,
        argv,
        id,
        offset,
        timeout,
        force: force.is_some(),
        keep,
    })
}

/// The `run-logs` body for `chunk`, read from byte `offset` of a run's log:
/// `<next offset>\n<text>`. Bodies are UTF-8, logs are bytes: invalid
/// sequences become U+FFFD, and a multi-byte character cut off at the end of
/// the chunk is left for the next read (`next` stops before it), so a
/// character split across two reads isn't mangled. The explicit `next` is
/// what keeps the caller's offset in bytes of the log, not of the text.
pub fn logs_body(offset: u64, chunk: &[u8]) -> String {
    let len = complete_len(chunk);
    format!("{}\n{}", offset + len as u64, String::from_utf8_lossy(&chunk[..len]))
}

/// Split a [`logs_body`] into `(next offset, text)`.
pub fn parse_logs_body(body: &str) -> Result<(u64, &str), String> {
    let (next, text) = body.split_once('\n').unwrap_or((body, ""));
    let next = number(next, 1, "offset")?;
    Ok((next, text))
}

/// Length of `b` without a trailing incomplete UTF-8 sequence.
fn complete_len(b: &[u8]) -> usize {
    let n = b.len();
    for back in 1..=n.min(4) {
        let c = b[n - back];
        if c & 0xC0 == 0x80 {
            continue; // continuation byte: look further back for its lead
        }
        let need = match c {
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF7 => 4,
            _ => 1,
        };
        return if need > back { n - back } else { n };
    }
    n
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
            ..Request::new(Op::Ensure)
        }
    }

    /// Every run field; `arg`s keep order, empty and multi-line words included.
    const RUN_REQUEST: &str = "op exec\n\
key pr-1\n\
arg sh\n\
arg -c\n\
arg echo a\\nb \\\\\n\
arg \n\
id 1790000000-a1b2\n\
offset 1024\n\
timeout 5\n";

    fn run_request() -> Request {
        Request {
            key: Some("pr-1".into()),
            argv: vec!["sh".into(), "-c".into(), "echo a\nb \\".into(), String::new()],
            id: Some("1790000000-a1b2".into()),
            offset: Some(1024),
            timeout: Some(5),
            ..Request::new(Op::Exec)
        }
    }

    #[test]
    fn request_fixture_round_trips() {
        assert_eq!(encode_request(&request()), REQUEST);
        assert_eq!(decode_request(REQUEST), Ok(request()));
        assert_eq!(encode_request(&run_request()), RUN_REQUEST);
        assert_eq!(decode_request(RUN_REQUEST), Ok(run_request()));
        assert_eq!(encode_request(&clear_request()), CLEAR_REQUEST);
        assert_eq!(decode_request(CLEAR_REQUEST), Ok(clear_request()));
    }

    /// The run-clearing fields: `force` is a bare directive.
    const CLEAR_REQUEST: &str = "op run-rm\n\
key pr-1\n\
id 1790000000-a1b2\n\
force\n\
keep 5\n";

    fn clear_request() -> Request {
        Request {
            key: Some("pr-1".into()),
            id: Some("1790000000-a1b2".into()),
            force: true,
            keep: Some(5),
            ..Request::new(Op::RunRm)
        }
    }

    #[test]
    fn clear_fields_round_trip_and_reject_malformed() {
        let prune = Request { key: Some("k".into()), keep: Some(0), ..Request::new(Op::RunPrune) };
        assert_eq!(encode_request(&prune), "op run-prune\nkey k\nkeep 0\n");
        assert_eq!(decode_request(&encode_request(&prune)), Ok(prune));
        // `force ` (empty value after the space) is the same flag.
        assert_eq!(decode_request("op run-rm\nforce \n").map(|r| r.force), Ok(true));
        assert_eq!(decode_request("op run-rm\n").map(|r| r.force), Ok(false));
        assert!(decode_request("op run-rm\nforce 1\n").unwrap_err().contains("takes no value"));
        assert!(decode_request("op run-rm\nforce\nforce\n").unwrap_err().contains("repeated `force`"));
        assert!(decode_request("op run-prune\nkeep 1\nkeep 2\n").unwrap_err().contains("repeated `keep`"));
        for bad in ["", "-1", "+1", "x", "99999999999999999999"] {
            let text = format!("op run-prune\nkeep {bad}\n");
            assert!(decode_request(&text).is_err(), "keep {bad:?}");
        }
    }

    #[test]
    fn minimal_request_and_every_op() {
        for op in Op::ALL {
            let r = Request::new(op);
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
        assert!(decode_request("op run\n").unwrap_err().contains("bad op"));
        assert!(decode_request("op run-ls\nid a\nid b\n").unwrap_err().contains("repeated `id`"));
        assert!(decode_request("op run-logs\noffset 1\noffset 1\n").unwrap_err().contains("repeated"));
        assert!(decode_request("op run-wait\ntimeout 1\ntimeout 2\n").is_err());
        for bad in ["", "-1", "+1", " 1", "1s", "0x10", "99999999999999999999"] {
            let text = format!("op run-logs\noffset {bad}\n");
            assert!(decode_request(&text).is_err(), "offset {bad:?}");
            let text = format!("op run-wait\ntimeout {bad}\n");
            assert!(decode_request(&text).is_err(), "timeout {bad:?}");
        }
        assert!(decode_request("op exec\narg a\\x\n").unwrap_err().contains("bad escape"));
    }

    #[test]
    fn run_ids() {
        assert!(valid_run_id("1790000000-a1b2"));
        assert!(valid_run_id("0000000000-0000"));
        for bad in ["", "1790000000-A1B2", "1790000000-a1b", "179000000-a1b2c", "1790000000_a1b2",
                    "../../../etc-x", "1790000000-g1b2", "1790000000-a1b2 "] {
            assert!(!valid_run_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn branches() {
        let max = "a".repeat(MAX_BRANCH);
        for good in ["a", "feat/x", "joan/pr-12", "release-1.2", "a_b/c.d/e-f", "v1.2.3", "x-", max.as_str()] {
            assert!(valid_branch(good), "{good:?}");
        }
        let long = "a".repeat(MAX_BRANCH + 1);
        for bad in [
            "", long.as_str(), "${localEnv:X}", "x-${localEnv:GITHUB_TOKEN}", "${instance}",
            "--upload-pack=x", "-x", "/x", "x/", ".x", "x.", "a..b", "a//b", "a/.b", "x.lock",
            "a/x.lock/b", "a@{1}", "a b", "a~1", "a^", "a:b", "a?", "a*", "a[b", "a\\b", "é",
            "a\nb", "HEAD@{0}", ".", "..", "/",
        ] {
            assert!(!valid_branch(bad), "{bad:?}");
        }
    }

    #[test]
    fn logs_bodies_keep_offsets_in_log_bytes() {
        assert_eq!(logs_body(0, b"out\n"), "4\nout\n");
        assert_eq!(parse_logs_body("4\nout\n"), Ok((4, "out\n")));
        assert_eq!(logs_body(7, b""), "7\n");
        assert_eq!(parse_logs_body("7\n"), Ok((7, "")));
        assert_eq!(parse_logs_body("7"), Ok((7, "")));
        // A character cut at the chunk end waits for the next read ...
        let e_acute = "é".as_bytes();
        assert_eq!(logs_body(10, &[b'a', e_acute[0]]), "11\na");
        assert_eq!(logs_body(0, &[0xE2, 0x82]), "0\n");
        assert_eq!(logs_body(0, "a€".as_bytes()), "4\na€");
        assert_eq!(logs_body(0, "😀".as_bytes()), "4\n😀");
        assert_eq!(logs_body(0, &"😀".as_bytes()[..3]), "0\n");
        // ... but invalid bytes are replaced and counted.
        assert_eq!(logs_body(0, &[0xFF, b'x']), "2\n\u{FFFD}x");
        assert_eq!(logs_body(0, &[0x80]), "1\n\u{FFFD}");
        assert!(parse_logs_body("x\nout").is_err());
        assert!(parse_logs_body("").is_err());
        // Round trip through a response: newlines, NULs, backslashes survive.
        let body = logs_body(0, b"a\n\0\\\n");
        let resp = decode_response(&encode_response(&Response::new(Status::Ok, body.clone()))).unwrap();
        assert_eq!(parse_logs_body(&resp.body), Ok((5, "a\n\0\\\n")));
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
    fn denied_env_names() {
        for bad in [
            "PATH", "HOME", "SHELL", "USER", "ENV", "BASH_ENV", "IFS", "CDPATH", "PS4",
            "PROMPT_COMMAND", "SSH_AUTH_SOCK", "TMPDIR", "GCONV_PATH", "NODE_OPTIONS", "RUBYOPT",
            "LD_PRELOAD", "LD_LIBRARY_PATH", "LD_", "DYLD_INSERT_LIBRARIES", "GIT_DIR",
            "GIT_SSH_COMMAND", "PYTHONPATH", "PYTHONSTARTUP", "PERL5LIB", "PERL5OPT",
            // Case-insensitive.
            "path", "Path", "ld_preload", "Git_Dir", "pythonpath", "node_options",
        ] {
            assert!(denied_env(bad), "{bad:?}");
        }
        for good in [
            "A", "FOO", "PR_URL", "GITHUB_TOKEN", "PATHS", "MYPATH", "XPATH", "HOMEDIR", "USERNAME",
            "OLD_PATH", "LD", "GIT", "PERL", "NODE_ENV", "RUBY", "TERM", "LANG", "_",
        ] {
            assert!(!denied_env(good), "{good:?}");
        }
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
