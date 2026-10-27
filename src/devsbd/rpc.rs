//! The RPC envelope: records inside one `channel::RPC` stream
//! (docs/architecture/helper/protocol.md, _RPC_), shared by the host and the
//! helper (devsbd includes it via `#[path]`; std only, like `control.rs`).
//!
//! A stream is a byte pipe (`Data` frame boundaries mean nothing), so it
//! carries length-prefixed records `u8 tag | u32 len | payload` (little
//! endian, like the frame header). The caller's first record is a `call`;
//! the daemon's last one is a `result`, an `error` or an `exit`.
//!
//! JSON payloads use a small subset with an encoder and decoder of their own
//! ([`Json`]), since the helper has no serde: objects, strings, integers,
//! bools, `null` and arrays of strings, objects nested at most
//! [`MAX_DEPTH`] deep. Enough for a call's `params` (argv, cwd, env, user);
//! anything bigger travels as raw-byte records instead.

use std::io::{self, Read, Write};

/// Record tags. Host and helper ship together, so an unknown tag is a bug
/// and a decode error, not something to skip.
pub mod tag {
    /// → first record: `{ "method", "params", "meta" }`.
    pub const CALL: u8 = 1;
    /// → raw bytes for the callee's stdin.
    pub const STDIN: u8 = 2;
    /// → no more stdin; empty payload.
    pub const STDIN_EOF: u8 = 3;
    /// → `u16 cols | u16 rows`.
    pub const RESIZE: u8 = 4;
    /// → `u8 signo`.
    pub const SIGNAL: u8 = 5;
    /// ← raw bytes.
    pub const STDOUT: u8 = 6;
    /// ← raw bytes.
    pub const STDERR: u8 = 7;
    /// ← JSON object: subscriptions and progress.
    pub const EVENT: u8 = 8;
    /// ← JSON object, last record on success.
    pub const RESULT: u8 = 9;
    /// ← JSON `{ "code", "message" }`, last record on failure.
    pub const ERROR: u8 = 10;
    /// ← `u8 kind (0 code, 1 signal) | i32 value`, last record of a process.
    pub const EXIT: u8 = 11;
}

/// Largest record payload, the frame's `MAX_PAYLOAD`: a stray length can't
/// become a huge allocation, and a sender chunks bigger output.
pub const MAX_RECORD: u32 = 1 << 20;

/// Nesting cap for JSON objects (a call → its `params` → `env` is 3).
pub const MAX_DEPTH: usize = 4;

/// `error` codes every method can answer.
pub mod code {
    /// No such method (or not offered by this daemon, see `rpc.methods`).
    pub const UNKNOWN_METHOD: &str = "unknown-method";
    /// The stream didn't start with a well-formed `call`, or its params are
    /// wrong for the method.
    pub const BAD_REQUEST: &str = "bad-request";
    /// The method ran and failed; `message` says why (e.g. `no run `x``).
    pub const FAILED: &str = "failed";
}

/// A `call` record.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Call {
    pub method: String,
    pub params: Obj,
    /// `caller`, `deadline`, `idempotency` (docs/plans/active/helper-rpc.md).
    pub meta: Obj,
}

/// How a process ended, in an `exit` record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Code(i32),
    Signal(i32),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Record {
    Call(Call),
    Stdin(Vec<u8>),
    StdinEof,
    Resize { cols: u16, rows: u16 },
    Signal(u8),
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Event(Obj),
    Result(Obj),
    Error { code: String, message: String },
    Exit(Exit),
}

impl Record {
    /// `error {code, message}`.
    pub fn error(code: &str, message: impl Into<String>) -> Record {
        Record::Error { code: code.to_string(), message: message.into() }
    }

    fn tag_and_payload(&self) -> (u8, Vec<u8>) {
        match self {
            Record::Call(c) => {
                let obj = Obj::new()
                    .with("method", c.method.as_str())
                    .with("params", c.params.clone())
                    .with("meta", c.meta.clone());
                (tag::CALL, encode_json(&Json::Obj(obj)).into_bytes())
            }
            Record::Stdin(b) => (tag::STDIN, b.clone()),
            Record::StdinEof => (tag::STDIN_EOF, Vec::new()),
            Record::Resize { cols, rows } => {
                let mut p = cols.to_le_bytes().to_vec();
                p.extend_from_slice(&rows.to_le_bytes());
                (tag::RESIZE, p)
            }
            Record::Signal(n) => (tag::SIGNAL, vec![*n]),
            Record::Stdout(b) => (tag::STDOUT, b.clone()),
            Record::Stderr(b) => (tag::STDERR, b.clone()),
            Record::Event(o) => (tag::EVENT, encode_json(&Json::Obj(o.clone())).into_bytes()),
            Record::Result(o) => (tag::RESULT, encode_json(&Json::Obj(o.clone())).into_bytes()),
            Record::Error { code, message } => {
                let obj = Obj::new().with("code", code.as_str()).with("message", message.as_str());
                (tag::ERROR, encode_json(&Json::Obj(obj)).into_bytes())
            }
            Record::Exit(e) => {
                let (kind, v) = match e {
                    Exit::Code(c) => (0u8, *c),
                    Exit::Signal(s) => (1u8, *s),
                };
                let mut p = vec![kind];
                p.extend_from_slice(&v.to_le_bytes());
                (tag::EXIT, p)
            }
        }
    }

    /// The record on the wire: header + payload.
    pub fn encode(&self) -> Vec<u8> {
        let (tag, payload) = self.tag_and_payload();
        let mut out = Vec::with_capacity(5 + payload.len());
        out.push(tag);
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&payload);
        out
    }

    /// Decode one record's payload. The error is one line.
    pub fn decode(tag: u8, payload: &[u8]) -> Result<Record, String> {
        let json = || -> Result<Obj, String> {
            let text = std::str::from_utf8(payload).map_err(|_| "payload is not UTF-8".to_string())?;
            match decode_json(text)? {
                Json::Obj(o) => Ok(o),
                _ => Err("expected a JSON object".into()),
            }
        };
        let fixed = |n: usize| -> Result<(), String> {
            if payload.len() == n { Ok(()) } else { Err(format!("tag {tag}: payload is {} bytes, want {n}", payload.len())) }
        };
        Ok(match tag {
            tag::CALL => {
                let o = json()?;
                let method = o.str("method").ok_or("call: missing method")?.to_string();
                let sub = |k: &str| match o.get(k) {
                    None | Some(Json::Null) => Ok(Obj::new()),
                    Some(Json::Obj(v)) => Ok(v.clone()),
                    Some(_) => Err(format!("call: {k} is not an object")),
                };
                Record::Call(Call { method, params: sub("params")?, meta: sub("meta")? })
            }
            tag::STDIN => Record::Stdin(payload.to_vec()),
            tag::STDIN_EOF => {
                fixed(0)?;
                Record::StdinEof
            }
            tag::RESIZE => {
                fixed(4)?;
                Record::Resize {
                    cols: u16::from_le_bytes([payload[0], payload[1]]),
                    rows: u16::from_le_bytes([payload[2], payload[3]]),
                }
            }
            tag::SIGNAL => {
                fixed(1)?;
                Record::Signal(payload[0])
            }
            tag::STDOUT => Record::Stdout(payload.to_vec()),
            tag::STDERR => Record::Stderr(payload.to_vec()),
            tag::EVENT => Record::Event(json()?),
            tag::RESULT => Record::Result(json()?),
            tag::ERROR => {
                let o = json()?;
                let code = o.str("code").ok_or("error: missing code")?.to_string();
                let message = o.str("message").unwrap_or_default().to_string();
                Record::Error { code, message }
            }
            tag::EXIT => {
                fixed(5)?;
                let v = i32::from_le_bytes([payload[1], payload[2], payload[3], payload[4]]);
                match payload[0] {
                    0 => Record::Exit(Exit::Code(v)),
                    1 => Record::Exit(Exit::Signal(v)),
                    k => return Err(format!("exit: unknown kind {k}")),
                }
            }
            t => return Err(format!("unknown record tag {t}")),
        })
    }
}

/// Write one record.
pub fn write_record(w: &mut impl Write, rec: &Record) -> io::Result<()> {
    let bytes = rec.encode();
    if bytes.len() - 5 > MAX_RECORD as usize {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "rpc record larger than MAX_RECORD"));
    }
    w.write_all(&bytes)?;
    w.flush()
}

/// Read one record; `None` on a clean end of stream (before a tag byte).
/// A truncated record is `UnexpectedEof`, a malformed one `InvalidData`.
pub fn read_record(r: &mut impl Read) -> io::Result<Option<Record>> {
    let mut tag = [0u8; 1];
    loop {
        match r.read(&mut tag) {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len);
    let bad = |msg: String| io::Error::new(io::ErrorKind::InvalidData, msg);
    if len > MAX_RECORD {
        return Err(bad(format!("rpc record of {len} bytes exceeds MAX_RECORD")));
    }
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload)?;
    Record::decode(tag[0], &payload).map(Some).map_err(bad)
}

// ---- JSON subset ----

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Int(i64),
    Str(String),
    /// Arrays hold strings only.
    Strs(Vec<String>),
    Obj(Obj),
}

/// A JSON object, members in order (the encoder is deterministic). Keys are
/// unique: the decoder rejects a repeat.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Obj(pub Vec<(String, Json)>);

impl Obj {
    pub fn new() -> Obj {
        Obj(Vec::new())
    }

    /// Builder: append (or replace) `key`.
    pub fn with(mut self, key: &str, value: impl Into<Json>) -> Obj {
        let value = value.into();
        match self.0.iter_mut().find(|(k, _)| k == key) {
            Some(slot) => slot.1 = value,
            None => self.0.push((key.to_string(), value)),
        }
        self
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn str(&self, key: &str) -> Option<&str> {
        match self.get(key) {
            Some(Json::Str(s)) => Some(s),
            _ => None,
        }
    }

    pub fn int(&self, key: &str) -> Option<i64> {
        match self.get(key) {
            Some(Json::Int(n)) => Some(*n),
            _ => None,
        }
    }

    pub fn bool(&self, key: &str) -> Option<bool> {
        match self.get(key) {
            Some(Json::Bool(b)) => Some(*b),
            _ => None,
        }
    }

    pub fn strs(&self, key: &str) -> Option<&[String]> {
        match self.get(key) {
            Some(Json::Strs(v)) => Some(v),
            _ => None,
        }
    }

    pub fn obj(&self, key: &str) -> Option<&Obj> {
        match self.get(key) {
            Some(Json::Obj(o)) => Some(o),
            _ => None,
        }
    }

    /// `key` as a string → string map (`env`); `None` when absent, not an
    /// object, or any value isn't a string.
    pub fn map(&self, key: &str) -> Option<Vec<(String, String)>> {
        self.obj(key)?
            .0
            .iter()
            .map(|(k, v)| match v {
                Json::Str(s) => Some((k.clone(), s.clone())),
                _ => None,
            })
            .collect()
    }
}

impl From<&str> for Json {
    fn from(s: &str) -> Json {
        Json::Str(s.to_string())
    }
}
impl From<String> for Json {
    fn from(s: String) -> Json {
        Json::Str(s)
    }
}
impl From<i64> for Json {
    fn from(n: i64) -> Json {
        Json::Int(n)
    }
}
impl From<bool> for Json {
    fn from(b: bool) -> Json {
        Json::Bool(b)
    }
}
impl From<Vec<String>> for Json {
    fn from(v: Vec<String>) -> Json {
        Json::Strs(v)
    }
}
impl From<Obj> for Json {
    fn from(o: Obj) -> Json {
        Json::Obj(o)
    }
}
/// A string map as an object (`env`).
impl From<Vec<(String, String)>> for Json {
    fn from(m: Vec<(String, String)>) -> Json {
        Json::Obj(Obj(m.into_iter().map(|(k, v)| (k, Json::Str(v))).collect()))
    }
}

/// Compact JSON text for `v`.
pub fn encode_json(v: &Json) -> String {
    let mut out = String::new();
    write_json(&mut out, v);
    out
}

fn write_json(out: &mut String, v: &Json) {
    match v {
        Json::Null => out.push_str("null"),
        Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Json::Int(n) => out.push_str(&n.to_string()),
        Json::Str(s) => write_str(out, s),
        Json::Strs(items) => {
            out.push('[');
            for (i, s) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_str(out, s);
            }
            out.push(']');
        }
        Json::Obj(o) => {
            out.push('{');
            for (i, (k, v)) in o.0.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_str(out, k);
                out.push(':');
                write_json(out, v);
            }
            out.push('}');
        }
    }
}

fn write_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Parse `text` as exactly one value of the subset. The error is one line
/// with the byte offset.
pub fn decode_json(text: &str) -> Result<Json, String> {
    let mut p = Parser { b: text.as_bytes(), i: 0 };
    p.ws();
    let v = p.value(0)?;
    p.ws();
    if p.i != p.b.len() {
        return Err(p.err("trailing characters"));
    }
    Ok(v)
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn err(&self, msg: &str) -> String {
        format!("json: {msg} at byte {}", self.i)
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }

    fn eat(&mut self, c: u8) -> Result<(), String> {
        if self.peek() == Some(c) {
            self.i += 1;
            Ok(())
        } else {
            Err(self.err(&format!("expected `{}`", c as char)))
        }
    }

    fn lit(&mut self, word: &str, v: Json) -> Result<Json, String> {
        if self.b[self.i..].starts_with(word.as_bytes()) {
            self.i += word.len();
            Ok(v)
        } else {
            Err(self.err("unexpected token"))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Json, String> {
        match self.peek() {
            Some(b'{') => self.object(depth + 1).map(Json::Obj),
            Some(b'[') => self.array(),
            Some(b'"') => self.string().map(Json::Str),
            Some(b't') => self.lit("true", Json::Bool(true)),
            Some(b'f') => self.lit("false", Json::Bool(false)),
            Some(b'n') => self.lit("null", Json::Null),
            Some(b'-' | b'0'..=b'9') => self.int(),
            Some(_) => Err(self.err("unexpected token")),
            None => Err(self.err("unexpected end")),
        }
    }

    fn object(&mut self, depth: usize) -> Result<Obj, String> {
        if depth > MAX_DEPTH {
            return Err(self.err("nested too deep"));
        }
        self.eat(b'{')?;
        let mut obj = Obj::new();
        self.ws();
        if self.peek() == Some(b'}') {
            self.i += 1;
            return Ok(obj);
        }
        loop {
            self.ws();
            let at = self.i;
            let key = self.string()?;
            if obj.get(&key).is_some() {
                self.i = at;
                return Err(self.err(&format!("repeated key `{key}`")));
            }
            self.ws();
            self.eat(b':')?;
            self.ws();
            let v = self.value(depth)?;
            obj.0.push((key, v));
            self.ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    return Ok(obj);
                }
                _ => return Err(self.err("expected `,` or `}`")),
            }
        }
    }

    fn array(&mut self) -> Result<Json, String> {
        self.eat(b'[')?;
        let mut items = Vec::new();
        self.ws();
        if self.peek() == Some(b']') {
            self.i += 1;
            return Ok(Json::Strs(items));
        }
        loop {
            self.ws();
            if self.peek() != Some(b'"') {
                return Err(self.err("arrays hold strings only"));
            }
            items.push(self.string()?);
            self.ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    return Ok(Json::Strs(items));
                }
                _ => return Err(self.err("expected `,` or `]`")),
            }
        }
    }

    fn int(&mut self) -> Result<Json, String> {
        let start = self.i;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        let digits = self.i;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.i += 1;
        }
        let n = self.i - digits;
        if n == 0 || (n > 1 && self.b[digits] == b'0') {
            return Err(self.err("bad number"));
        }
        if matches!(self.peek(), Some(b'.' | b'e' | b'E')) {
            return Err(self.err("only integers are supported"));
        }
        // ASCII digits with an optional sign: valid UTF-8 by construction.
        let text = std::str::from_utf8(&self.b[start..self.i]).unwrap();
        text.parse::<i64>().map(Json::Int).map_err(|_| {
            self.i = start;
            self.err("integer out of range")
        })
    }

    fn string(&mut self) -> Result<String, String> {
        self.eat(b'"')?;
        let mut out = String::new();
        loop {
            let start = self.i;
            while let Some(c) = self.peek() {
                if c == b'"' || c == b'\\' || c < 0x20 {
                    break;
                }
                self.i += 1;
            }
            // Splits only at ASCII bytes, so the run stays valid UTF-8.
            out.push_str(std::str::from_utf8(&self.b[start..self.i]).unwrap());
            match self.peek() {
                Some(b'"') => {
                    self.i += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.i += 1;
                    let Some(e) = self.peek() else { return Err(self.err("unexpected end")) };
                    self.i += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => out.push(self.unicode()?),
                        _ => return Err(self.err("bad escape")),
                    }
                }
                Some(_) => return Err(self.err("control character in string")),
                None => return Err(self.err("unterminated string")),
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let hex = self.b.get(self.i..self.i + 4).ok_or_else(|| self.err("short \\u escape"))?;
        let s = std::str::from_utf8(hex).map_err(|_| self.err("bad \\u escape"))?;
        let v = u32::from_str_radix(s, 16).map_err(|_| self.err("bad \\u escape"))?;
        self.i += 4;
        Ok(v)
    }

    /// After `\u`: one code point, joining a surrogate pair.
    fn unicode(&mut self) -> Result<char, String> {
        let hi = self.hex4()?;
        let cp = if (0xD800..0xDC00).contains(&hi) {
            if !self.b[self.i..].starts_with(b"\\u") {
                return Err(self.err("lone surrogate"));
            }
            self.i += 2;
            let lo = self.hex4()?;
            if !(0xDC00..0xE000).contains(&lo) {
                return Err(self.err("lone surrogate"));
            }
            0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
        } else {
            hi
        };
        char::from_u32(cp).ok_or_else(|| self.err("lone surrogate"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Checked from both crates (this module is compiled into each), so the
    /// host's and the helper's codecs agree on the exact bytes.
    const CALL_JSON: &str = r#"{"method":"run.start","params":{"argv":["sh","-c","echo \"hi\"\n"],"cwd":"/workspaces/repo","env":{"FOO":"bar","EMPTY":""},"user":"vscode","detach":true,"timeout":-5,"resume":null},"meta":{"caller":"web-1"}}"#;
    const EVENT_JSON: &str = r#"{"state":"running"}"#;
    const RESULT_JSON: &str = r#"{"methods":["rpc.methods@1"]}"#;
    const ERROR_JSON: &str = r#"{"code":"unknown-method","message":"no method x.y"}"#;

    fn with_header(tag: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(payload);
        out
    }

    fn fixture_bytes() -> Vec<u8> {
        let mut wire = with_header(1, CALL_JSON.as_bytes());
        wire.extend_from_slice(b"\x02\x02\x00\x00\x00ab");
        wire.extend_from_slice(b"\x03\x00\x00\x00\x00");
        wire.extend_from_slice(b"\x04\x04\x00\x00\x00\x50\x00\x18\x00");
        wire.extend_from_slice(b"\x05\x01\x00\x00\x00\x0f");
        wire.extend_from_slice(b"\x06\x03\x00\x00\x00out");
        wire.extend_from_slice(b"\x07\x01\x00\x00\x00\xff");
        wire.extend_from_slice(&with_header(8, EVENT_JSON.as_bytes()));
        wire.extend_from_slice(&with_header(9, RESULT_JSON.as_bytes()));
        wire.extend_from_slice(&with_header(10, ERROR_JSON.as_bytes()));
        wire.extend_from_slice(b"\x0b\x05\x00\x00\x00\x00\x03\x00\x00\x00");
        wire.extend_from_slice(b"\x0b\x05\x00\x00\x00\x01\x09\x00\x00\x00");
        wire
    }

    fn fixture_records() -> Vec<Record> {
        let params = Obj::new()
            .with("argv", vec!["sh".to_string(), "-c".into(), "echo \"hi\"\n".into()])
            .with("cwd", "/workspaces/repo")
            .with("env", vec![("FOO".to_string(), "bar".to_string()), ("EMPTY".into(), "".into())])
            .with("user", "vscode")
            .with("detach", true)
            .with("timeout", -5i64)
            .with("resume", Json::Null);
        vec![
            Record::Call(Call { method: "run.start".into(), params, meta: Obj::new().with("caller", "web-1") }),
            Record::Stdin(b"ab".to_vec()),
            Record::StdinEof,
            Record::Resize { cols: 80, rows: 24 },
            Record::Signal(15),
            Record::Stdout(b"out".to_vec()),
            Record::Stderr(vec![0xff]),
            Record::Event(Obj::new().with("state", "running")),
            Record::Result(Obj::new().with("methods", vec!["rpc.methods@1".to_string()])),
            Record::error(code::UNKNOWN_METHOD, "no method x.y"),
            Record::Exit(Exit::Code(3)),
            Record::Exit(Exit::Signal(9)),
        ]
    }

    #[test]
    fn fixture_round_trips() {
        let mut wire = Vec::new();
        for rec in fixture_records() {
            write_record(&mut wire, &rec).unwrap();
        }
        assert!(wire == fixture_bytes(), "encoded records differ from the fixture");
        let mut r = io::Cursor::new(fixture_bytes());
        let mut got = Vec::new();
        while let Some(rec) = read_record(&mut r).unwrap() {
            got.push(rec);
        }
        assert_eq!(got, fixture_records());
    }

    #[test]
    fn call_params_read_back() {
        let Record::Call(call) = fixture_records().remove(0) else { unreachable!() };
        let p = &call.params;
        assert_eq!(p.strs("argv").unwrap(), ["sh", "-c", "echo \"hi\"\n"]);
        assert_eq!(p.str("cwd"), Some("/workspaces/repo"));
        assert_eq!(p.map("env").unwrap(), [("FOO".to_string(), "bar".to_string()), ("EMPTY".into(), "".into())]);
        assert_eq!(p.bool("detach"), Some(true));
        assert_eq!(p.int("timeout"), Some(-5));
        assert_eq!(p.get("resume"), Some(&Json::Null));
        assert_eq!(p.map("argv"), None, "not an object");
        assert_eq!(call.meta.str("caller"), Some("web-1"));
    }

    #[test]
    fn call_without_params_or_meta() {
        let rec = Record::decode(tag::CALL, br#"{"method":"rpc.methods"}"#).unwrap();
        assert_eq!(rec, Record::Call(Call { method: "rpc.methods".into(), ..Call::default() }));
        assert!(Record::decode(tag::CALL, br#"{"params":{}}"#).unwrap_err().contains("missing method"));
        assert!(Record::decode(tag::CALL, br#"{"method":"m","params":[]}"#).unwrap_err().contains("not an object"));
    }

    #[test]
    fn malformed_records_are_errors() {
        assert!(Record::decode(99, b"").unwrap_err().contains("unknown record tag 99"));
        assert!(Record::decode(tag::RESIZE, b"\x01\x00").is_err());
        assert!(Record::decode(tag::STDIN_EOF, b"x").is_err());
        assert!(Record::decode(tag::EXIT, b"\x02\0\0\0\0").unwrap_err().contains("unknown kind"));
        assert!(Record::decode(tag::RESULT, b"[]").is_err(), "results are objects");
        assert!(Record::decode(tag::RESULT, b"\xff").unwrap_err().contains("UTF-8"));
        // Truncated header / payload; oversized length.
        assert_eq!(read_record(&mut &b"\x06\x05\x00"[..]).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(read_record(&mut &b"\x06\x05\x00\x00\x00ab"[..]).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        let huge = [&[tag::STDOUT][..], &(MAX_RECORD + 1).to_le_bytes()].concat();
        assert_eq!(read_record(&mut &huge[..]).unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert!(read_record(&mut &b""[..]).unwrap().is_none(), "clean end");
    }

    #[test]
    fn json_strings_escape_and_unescape() {
        let s = "q\" b\\ nl\n cr\r tab\t bell\u{7} é 😀";
        let text = encode_json(&Json::Str(s.into()));
        assert_eq!(text, r#""q\" b\\ nl\n cr\r tab\t bell\u0007 é 😀""#);
        assert_eq!(decode_json(&text), Ok(Json::Str(s.into())));
        assert_eq!(decode_json(r#""\u00e9\ud83d\ude00\/""#), Ok(Json::Str("é😀/".into())));
    }

    #[test]
    fn json_outside_the_subset_is_rejected() {
        for (text, why) in [
            ("1.5", "only integers"),
            ("1e3", "only integers"),
            ("01", "bad number"),
            ("-", "bad number"),
            ("99999999999999999999", "out of range"),
            ("[1]", "strings only"),
            ("[[]]", "strings only"),
            (r#"{"a":1,"a":2}"#, "repeated key"),
            (r#"{"a":{"b":{"c":{"d":{}}}}}"#, "nested too deep"),
            ("\"a\nb\"", "control character"),
            (r#""\x""#, "bad escape"),
            (r#""\ud83d""#, "lone surrogate"),
            ("{} x", "trailing"),
            ("{", "expected"),
            ("", "unexpected end"),
        ] {
            let err = decode_json(text).unwrap_err();
            assert!(err.contains(why), "{text:?}: {err}");
        }
        assert!(decode_json(r#"{"a":{"b":{"c":{}}}}"#).is_ok(), "depth 4 is fine");
        assert_eq!(decode_json(" { } "), Ok(Json::Obj(Obj::new())));
        assert_eq!(decode_json("[]"), Ok(Json::Strs(Vec::new())));
        assert_eq!(decode_json(&i64::MIN.to_string()), Ok(Json::Int(i64::MIN)));
    }

    #[test]
    fn oversized_records_are_refused_before_the_wire() {
        let mut out = Vec::new();
        let rec = Record::Stdout(vec![0; MAX_RECORD as usize + 1]);
        assert_eq!(write_record(&mut out, &rec).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        assert!(out.is_empty());
    }
}
