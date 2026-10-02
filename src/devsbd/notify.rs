//! A notification record: what `devsbd notify` queues in the container's
//! outbox and the daemon sends, one per `Open(NOTIFY)` stream, to a host that
//! advertises `caps::NOTIFY` (docs/automations.md, "`devsbd notify`"). One
//! file shared by both crates (devsbd includes it via `#[path]`) so the writer
//! and the reader can't drift; std-only and hand-parsed, like `bootfile.rs`.
//!
//! Format, line-based UTF-8, one directive per line, `<key> <value>`, values
//! escaped as in `escape.rs` (`\\`, `\n`, `\0`):
//!
//! ```text
//! kind thread-put          optional, at most once: thread-put|thread-rm.
//!                          Absent = a plain notify record, which is why old
//!                          helpers' records still decode byte for byte.
//! at 1790000000            required, once: unix seconds when queued
//! level warn               notify only, required: info|warn|error
//! key pr-123               notify: optional dedupe key; thread-*: required
//! link https://…           notify only, optional
//! msg PR 123\nneeds you    notify only, required
//! body {"key":"pr-123",…}  thread-put only, required: the thread JSON. The
//!                          helper only syntax-checks it (docs/inbox-threads.md,
//!                          "Decisions"); the host owns the schema.
//! ```
//!
//! A thread record repeats the `key` the helper pulled out of `body`, so the
//! outbox can coalesce same-key puts and the host can route and report a
//! rejected body without parsing JSON twice.
//!
//! Unknown keys, repeats, a missing required key, a directive on the wrong
//! kind, or a bad escape are parse errors (the daemon moves such a file aside
//! so it can't block the queue). The host answers each stream with
//! [`REPLY_OK`] once it has the record.

use super::escape::{escape, unescape};

/// The outbox directory. Survives container restarts (not under `/run`);
/// mode 1777 because `devsbd notify` runs as the sandbox's `remoteUser`.
pub const OUTBOX: &str = "/var/lib/devsandbox/outbox";

/// Cap on an encoded record, checked when queuing and when flushing: a
/// notification is a line of text, and the record travels as one `Data` frame
/// (well under `proto::MAX_PAYLOAD`).
pub const MAX_RECORD: usize = 64 * 1024;

/// What the host writes back on a notify stream once it has taken the record;
/// the daemon deletes the outbox file only after reading it.
pub const REPLY_OK: &[u8] = b"ok";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

impl Level {
    pub fn parse(s: &str) -> Option<Level> {
        match s {
            "info" => Some(Level::Info),
            "warn" => Some(Level::Warn),
            "error" => Some(Level::Error),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub level: Level,
    pub key: Option<String>,
    pub link: Option<String>,
    pub msg: String,
    /// Unix seconds when `devsbd notify` queued it.
    pub at: u64,
}

/// What one outbox file holds: the original `devsbd notify` record, or one of
/// the `devsbd thread` verbs (docs/inbox-threads.md). They share the queue,
/// the transport and the `ok` handshake; only the directives differ.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Notify(Record),
    /// `devsbd thread put`: the whole thread state as JSON, opaque here.
    ThreadPut { at: u64, key: String, body: String },
    /// `devsbd thread rm <key>`.
    ThreadRm { at: u64, key: String },
}

impl Message {
    /// Unix seconds when the helper queued it.
    pub fn at(&self) -> u64 {
        match self {
            Message::Notify(r) => r.at,
            Message::ThreadPut { at, .. } | Message::ThreadRm { at, .. } => *at,
        }
    }

    /// The thread (or dedupe) key, when the message carries one.
    pub fn key(&self) -> Option<&str> {
        match self {
            Message::Notify(r) => r.key.as_deref(),
            Message::ThreadPut { key, .. } | Message::ThreadRm { key, .. } => Some(key),
        }
    }

    /// A thread verb for `key`: what the outbox coalesces on. Plain notify
    /// records are a log and are never dropped, even with the same key.
    pub fn is_thread_op_for(&self, key: &str) -> bool {
        matches!(self, Message::ThreadPut { key: k, .. } | Message::ThreadRm { key: k, .. } if k == key)
    }
}

impl From<Record> for Message {
    fn from(r: Record) -> Message {
        Message::Notify(r)
    }
}

/// The `kind` directive's values; absent means [`Message::Notify`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    ThreadPut,
    ThreadRm,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::ThreadPut => "thread-put",
            Kind::ThreadRm => "thread-rm",
        }
    }
}

pub fn encode(m: &Message) -> String {
    let mut out = String::new();
    let mut line = |key: &str, value: &str| {
        out.push_str(key);
        out.push(' ');
        escape(value, &mut out);
        out.push('\n');
    };
    match m {
        // Unchanged from before `kind` existed, so a host that predates threads
        // still reads what this helper queues.
        Message::Notify(r) => {
            line("level", r.level.as_str());
            line("at", &r.at.to_string());
            if let Some(key) = &r.key {
                line("key", key);
            }
            if let Some(link) = &r.link {
                line("link", link);
            }
            line("msg", &r.msg);
        }
        Message::ThreadPut { at, key, body } => {
            line("kind", Kind::ThreadPut.as_str());
            line("at", &at.to_string());
            line("key", key);
            line("body", body);
        }
        Message::ThreadRm { at, key } => {
            line("kind", Kind::ThreadRm.as_str());
            line("at", &at.to_string());
            line("key", key);
        }
    }
    out
}

pub fn decode(text: &str) -> Result<Message, String> {
    let (mut kind, mut level, mut at) = (None, None, None);
    let (mut key, mut link, mut msg, mut body) = (None, None, None, None);
    for (n, line) in text.split('\n').enumerate() {
        if line.is_empty() {
            continue;
        }
        let err = |e: String| format!("line {}: {e}", n + 1);
        let (name, rest) = line.split_once(' ').unwrap_or((line, ""));
        let value = unescape(rest).map_err(err)?;
        let repeated = || err(format!("repeated `{name}`"));
        match name {
            "kind" => {
                let k = match value.as_str() {
                    "thread-put" => Kind::ThreadPut,
                    "thread-rm" => Kind::ThreadRm,
                    _ => return Err(err(format!("bad kind `{value}`"))),
                };
                if kind.replace(k).is_some() {
                    return Err(repeated());
                }
            }
            "level" => {
                let l = Level::parse(&value).ok_or_else(|| err(format!("bad level `{value}`")))?;
                if level.replace(l).is_some() {
                    return Err(repeated());
                }
            }
            "at" => {
                let t = value.parse::<u64>().map_err(|_| err(format!("bad time `{value}`")))?;
                if at.replace(t).is_some() {
                    return Err(repeated());
                }
            }
            "key" | "link" | "msg" | "body" => {
                let slot = match name {
                    "key" => &mut key,
                    "link" => &mut link,
                    "msg" => &mut msg,
                    _ => &mut body,
                };
                if slot.replace(value).is_some() {
                    return Err(repeated());
                }
            }
            other => return Err(err(format!("unknown key `{other}`"))),
        }
    }
    let missing = |k: &str| format!("missing `{k}`");
    let at = at.ok_or_else(|| missing("at"))?;
    let Some(kind) = kind else {
        if body.is_some() {
            return Err("`body` needs `kind thread-put`".into());
        }
        return Ok(Message::Notify(Record {
            level: level.ok_or_else(|| missing("level"))?,
            at,
            msg: msg.ok_or_else(|| missing("msg"))?,
            key,
            link,
        }));
    };
    // The notify-only directives would be silently dropped otherwise, hiding a
    // sender that mixed the two forms.
    for (name, present) in [("level", level.is_some()), ("link", link.is_some()), ("msg", msg.is_some())] {
        if present {
            return Err(format!("`{name}` is not allowed on a `{}` record", kind.as_str()));
        }
    }
    let key = key.ok_or_else(|| missing("key"))?;
    match kind {
        Kind::ThreadPut => Ok(Message::ThreadPut { at, key, body: body.ok_or_else(|| missing("body"))? }),
        Kind::ThreadRm if body.is_some() => Err("`body` is not allowed on a `thread-rm` record".into()),
        Kind::ThreadRm => Ok(Message::ThreadRm { at, key }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Checked from both crates (this module is compiled into each), so the
    /// helper's encoder and the host's decoder agree on the exact bytes.
    const FIXTURE: &str = "level warn\n\
at 1790000000\n\
key pr-123\n\
link https://example.com/pr/123\n\
msg PR 123\\nneeds you\n";

    /// The thread verbs, checked from both crates for the same reason.
    const PUT_FIXTURE: &str = "kind thread-put\n\
at 1790000000\n\
key pr-123\n\
body {\"key\":\"pr-123\",\"title\":\"a\\nb\"}\n";

    const RM_FIXTURE: &str = "kind thread-rm\nat 1790000000\nkey pr-123\n";

    fn fixture() -> Message {
        Message::Notify(Record {
            level: Level::Warn,
            key: Some("pr-123".into()),
            link: Some("https://example.com/pr/123".into()),
            msg: "PR 123\nneeds you".into(),
            at: 1_790_000_000,
        })
    }

    fn put_fixture() -> Message {
        Message::ThreadPut {
            at: 1_790_000_000,
            key: "pr-123".into(),
            // A newline inside the JSON body: escaped on the wire, not a line break.
            body: "{\"key\":\"pr-123\",\"title\":\"a\nb\"}".into(),
        }
    }

    #[test]
    fn fixture_round_trips() {
        assert_eq!(encode(&fixture()), FIXTURE);
        assert_eq!(decode(FIXTURE), Ok(fixture()));
    }

    #[test]
    fn thread_fixtures_round_trip() {
        assert_eq!(encode(&put_fixture()), PUT_FIXTURE);
        assert_eq!(decode(PUT_FIXTURE), Ok(put_fixture()));
        let rm = Message::ThreadRm { at: 1_790_000_000, key: "pr-123".into() };
        assert_eq!(encode(&rm), RM_FIXTURE);
        assert_eq!(decode(RM_FIXTURE), Ok(rm.clone()));
        // Accessors the outbox and the host route on.
        assert_eq!((put_fixture().at(), put_fixture().key()), (1_790_000_000, Some("pr-123")));
        assert!(put_fixture().is_thread_op_for("pr-123"));
        assert!(rm.is_thread_op_for("pr-123"));
        assert!(!rm.is_thread_op_for("pr-124"));
        // A notify record with the same key is a log entry, never coalesced.
        assert!(!fixture().is_thread_op_for("pr-123"));
    }

    #[test]
    fn newlines_nuls_and_backslashes_round_trip() {
        let r = Message::Notify(Record {
            level: Level::Error,
            key: Some("k\0\\".into()),
            link: None,
            msg: "a\nb\0c\\n\n".into(),
            at: 0,
        });
        let text = encode(&r);
        assert_eq!(text.lines().count(), 4, "one line per directive");
        assert!(!text.contains('\0'));
        assert_eq!(decode(&text), Ok(r));
    }

    #[test]
    fn optional_fields_and_empty_msg() {
        let r =
            Message::Notify(Record { level: Level::Info, key: None, link: None, msg: String::new(), at: 5 });
        assert_eq!(encode(&r), "level info\nat 5\nmsg \n");
        assert_eq!(decode("level info\nat 5\nmsg \n"), Ok(r.clone()));
        // A bare `msg` (no space) is an empty value too; order doesn't matter.
        assert_eq!(decode("msg\nat 5\nlevel info\n"), Ok(r));
    }

    #[test]
    fn levels_parse_and_print() {
        for l in [Level::Info, Level::Warn, Level::Error] {
            assert_eq!(Level::parse(l.as_str()), Some(l));
        }
        assert_eq!(Level::parse("debug"), None);
        assert_eq!(Level::parse("WARN"), None);
    }

    #[test]
    fn rejects_malformed() {
        let ok = "level info\nat 1\nmsg hi\n";
        assert!(decode(ok).is_ok());
        assert!(decode("").unwrap_err().contains("missing `at`"));
        assert!(decode("at 1\nmsg hi\n").unwrap_err().contains("missing `level`"));
        assert!(decode("level info\nmsg hi\n").unwrap_err().contains("missing `at`"));
        assert!(decode("level info\nat 1\n").unwrap_err().contains("missing `msg`"));
        assert!(decode("level loud\nat 1\nmsg hi\n").unwrap_err().contains("bad level"));
        assert!(decode("level info\nat -1\nmsg hi\n").unwrap_err().contains("bad time"));
        assert!(decode(&format!("{ok}msg again\n")).unwrap_err().starts_with("line 4: repeated `msg`"));
        assert!(decode(&format!("{ok}level warn\n")).unwrap_err().contains("repeated `level`"));
        assert!(decode(&format!("{ok}bogus x\n")).unwrap_err().contains("unknown key `bogus`"));
        assert!(decode("level info\nat 1\nmsg a\\tb\n").unwrap_err().contains("bad escape"));
        // Binary junk (e.g. a truncated or foreign file) is rejected, not guessed at.
        assert!(decode("\u{1}\u{2}garbage").is_err());
    }

    #[test]
    fn rejects_malformed_thread_records() {
        assert!(decode(PUT_FIXTURE).is_ok());
        assert!(decode("kind thread-ls\nat 1\nkey k\n").unwrap_err().contains("bad kind"));
        assert!(decode("kind thread-put\nat 1\nkey k\n").unwrap_err().contains("missing `body`"));
        assert!(decode("kind thread-put\nat 1\nbody {}\n").unwrap_err().contains("missing `key`"));
        assert!(decode("kind thread-rm\nkey k\n").unwrap_err().contains("missing `at`"));
        assert!(decode("kind thread-rm\nat 1\n").unwrap_err().contains("missing `key`"));
        // Directives from the other form are a mistake, not something to drop.
        assert!(decode("kind thread-rm\nat 1\nkey k\nbody {}\n").unwrap_err().contains("`body` is not allowed"));
        for bad in ["level info", "link https://x", "msg hi"] {
            let err = decode(&format!("kind thread-put\nat 1\nkey k\nbody {{}}\n{bad}\n")).unwrap_err();
            assert!(err.contains("is not allowed on a `thread-put` record"), "{bad}: {err}");
        }
        assert!(decode("level info\nat 1\nmsg hi\nbody {}\n").unwrap_err().contains("`body` needs"));
        assert!(decode(&format!("{PUT_FIXTURE}kind thread-rm\n")).unwrap_err().contains("repeated `kind`"));
        assert!(decode(&format!("{PUT_FIXTURE}body x\n")).unwrap_err().contains("repeated `body`"));
    }
}
