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
//! kind thread-put          optional, at most once: thread-put|thread-rm|
//!                          thread-send|thread-withdraw. Absent = a plain
//!                          notify record, which is why old helpers' records
//!                          still decode byte for byte.
//! at 1790000000            required, once: unix seconds when queued
//! level warn               notify only, required: info|warn|error
//! key pr-123               notify: optional dedupe key; thread-*: required
//!                          (send/withdraw: the thread the message is in)
//! id run-1791277117        thread-send|thread-withdraw only, required: the
//!                          message id
//! link https://…           notify only, optional
//! msg PR 123\nneeds you    notify only, required
//! body {"key":"pr-123",…}  thread-put|thread-send only, required: the JSON
//!                          body. The helper only syntax-checks it
//!                          (docs/inbox-threads.md, "Decisions"); the host
//!                          owns the schema.
//! ```
//!
//! A thread record repeats the `key` (and a message's `id`) the helper pulled
//! out of `body`, so the outbox can coalesce ([`Message::supersedes`]) and the
//! host can route and report a rejected body without parsing JSON twice.
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

/// Cap on a `thread send` body (docs/inbox-redesign.md, *Messages*): leaves
/// room under [`MAX_RECORD`] for the directives and the escaping. Checked by
/// the helper before queuing and by the host's schema.
pub const MAX_SEND_BODY: usize = 48 * 1024;

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
    /// `devsbd thread send`: one message (`id`) in thread `key`, its JSON
    /// body opaque here.
    ThreadSend { at: u64, key: String, id: String, body: String },
    /// `devsbd thread withdraw <key> <id>`.
    ThreadWithdraw { at: u64, key: String, id: String },
}

impl Message {
    /// Unix seconds when the helper queued it.
    pub fn at(&self) -> u64 {
        match self {
            Message::Notify(r) => r.at,
            Message::ThreadPut { at, .. }
            | Message::ThreadRm { at, .. }
            | Message::ThreadSend { at, .. }
            | Message::ThreadWithdraw { at, .. } => *at,
        }
    }

    /// The thread (or dedupe) key, when the message carries one.
    #[allow(dead_code)] // host-only: the helper coalesces on `supersedes`
    pub fn key(&self) -> Option<&str> {
        match self {
            Message::Notify(r) => r.key.as_deref(),
            Message::ThreadPut { key, .. }
            | Message::ThreadRm { key, .. }
            | Message::ThreadSend { key, .. }
            | Message::ThreadWithdraw { key, .. } => Some(key),
        }
    }

    /// Whether queuing `self` makes the still-queued `older` pointless, so
    /// the outbox can drop it (docs/automations.md, *Inbox threads*):
    ///
    /// - a put replaces a queued put for its thread (the newest header wins);
    /// - an rm replaces a queued put or rm for its thread, and every queued
    ///   send/withdraw in it (they'd only hit a removed thread);
    /// - a send or withdraw replaces a queued send or withdraw of the same
    ///   `(thread, id)` (the newest body, or the withdrawal, wins).
    ///
    /// A put never replaces a queued rm: rm then put is a fresh thread with an
    /// empty feed, not the old one re-headed. Plain notify records are a log
    /// and are never dropped, even with the same key.
    pub fn supersedes(&self, older: &Message) -> bool {
        use Message::*;
        match (self, older) {
            (ThreadPut { key: a, .. }, ThreadPut { key: b, .. }) => a == b,
            (ThreadRm { key: a, .. }, ThreadPut { key: b, .. } | ThreadRm { key: b, .. }) => a == b,
            (ThreadRm { key: a, .. }, ThreadSend { key: b, .. } | ThreadWithdraw { key: b, .. }) => a == b,
            (
                ThreadSend { key: a, id: i, .. } | ThreadWithdraw { key: a, id: i, .. },
                ThreadSend { key: b, id: j, .. } | ThreadWithdraw { key: b, id: j, .. },
            ) => a == b && i == j,
            _ => false,
        }
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
    ThreadSend,
    ThreadWithdraw,
}

impl Kind {
    const ALL: [Kind; 4] = [Kind::ThreadPut, Kind::ThreadRm, Kind::ThreadSend, Kind::ThreadWithdraw];

    fn as_str(self) -> &'static str {
        match self {
            Kind::ThreadPut => "thread-put",
            Kind::ThreadRm => "thread-rm",
            Kind::ThreadSend => "thread-send",
            Kind::ThreadWithdraw => "thread-withdraw",
        }
    }

    fn has_body(self) -> bool {
        matches!(self, Kind::ThreadPut | Kind::ThreadSend)
    }

    fn has_id(self) -> bool {
        matches!(self, Kind::ThreadSend | Kind::ThreadWithdraw)
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
        Message::ThreadSend { at, key, id, body } => {
            line("kind", Kind::ThreadSend.as_str());
            line("at", &at.to_string());
            line("key", key);
            line("id", id);
            line("body", body);
        }
        Message::ThreadWithdraw { at, key, id } => {
            line("kind", Kind::ThreadWithdraw.as_str());
            line("at", &at.to_string());
            line("key", key);
            line("id", id);
        }
    }
    out
}

pub fn decode(text: &str) -> Result<Message, String> {
    let (mut kind, mut level, mut at) = (None, None, None);
    let (mut key, mut link, mut msg, mut body, mut id) = (None, None, None, None, None);
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
                let k = Kind::ALL
                    .into_iter()
                    .find(|k| k.as_str() == value)
                    .ok_or_else(|| err(format!("bad kind `{value}`")))?;
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
            "key" | "link" | "msg" | "body" | "id" => {
                let slot = match name {
                    "key" => &mut key,
                    "link" => &mut link,
                    "msg" => &mut msg,
                    "id" => &mut id,
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
            return Err("`body` needs `kind thread-put|thread-send`".into());
        }
        if id.is_some() {
            return Err("`id` needs `kind thread-send|thread-withdraw`".into());
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
    for (name, present, allowed) in [("body", body.is_some(), kind.has_body()), ("id", id.is_some(), kind.has_id())] {
        if present && !allowed {
            return Err(format!("`{name}` is not allowed on a `{}` record", kind.as_str()));
        }
    }
    let key = key.ok_or_else(|| missing("key"))?;
    let body = || body.ok_or_else(|| missing("body"));
    let id = || id.clone().ok_or_else(|| missing("id"));
    Ok(match kind {
        Kind::ThreadPut => Message::ThreadPut { at, key, body: body()? },
        Kind::ThreadRm => Message::ThreadRm { at, key },
        Kind::ThreadSend => {
            let id = id()?;
            Message::ThreadSend { at, key, id, body: body()? }
        }
        Kind::ThreadWithdraw => Message::ThreadWithdraw { at, key, id: id()? },
    })
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
        assert!(put_fixture().supersedes(&put_fixture()));
        assert!(rm.supersedes(&put_fixture()));
        assert!(rm.supersedes(&rm));
        assert!(!Message::ThreadRm { at: 1, key: "pr-124".into() }.supersedes(&put_fixture()));
        // rm then put is a fresh thread: the put keeps the rm.
        assert!(!put_fixture().supersedes(&rm));
        // A notify record with the same key is a log entry, never coalesced.
        assert!(!put_fixture().supersedes(&fixture()));
        assert!(!rm.supersedes(&fixture()));
        assert!(!fixture().supersedes(&fixture()));
    }

    const SEND_FIXTURE: &str = "kind thread-send\n\
at 1790000000\n\
key pr-123\n\
id run-1\n\
body {\"thread\":\"pr-123\",\"id\":\"run-1\",\"blocks\":[]}\n";

    const WITHDRAW_FIXTURE: &str = "kind thread-withdraw\nat 1790000000\nkey pr-123\nid run-1\n";

    fn send(key: &str, id: &str) -> Message {
        Message::ThreadSend {
            at: 1_790_000_000,
            key: key.into(),
            id: id.into(),
            body: format!("{{\"thread\":\"{key}\",\"id\":\"{id}\",\"blocks\":[]}}"),
        }
    }

    fn withdraw(key: &str, id: &str) -> Message {
        Message::ThreadWithdraw { at: 1_790_000_000, key: key.into(), id: id.into() }
    }

    #[test]
    fn message_fixtures_round_trip() {
        assert_eq!(encode(&send("pr-123", "run-1")), SEND_FIXTURE);
        assert_eq!(decode(SEND_FIXTURE), Ok(send("pr-123", "run-1")));
        assert_eq!(encode(&withdraw("pr-123", "run-1")), WITHDRAW_FIXTURE);
        assert_eq!(decode(WITHDRAW_FIXTURE), Ok(withdraw("pr-123", "run-1")));
        assert_eq!((send("pr-123", "a").at(), send("pr-123", "a").key()), (1_790_000_000, Some("pr-123")));
        assert_eq!(withdraw("pr-9", "a").key(), Some("pr-9"));
    }

    #[test]
    fn messages_coalesce_per_thread_and_id() {
        let (s, w) = (send("t", "a"), withdraw("t", "a"));
        // Same (thread, id): the newer send or withdraw wins, either way round.
        assert!(s.supersedes(&send("t", "a")));
        assert!(w.supersedes(&s));
        assert!(s.supersedes(&w));
        assert!(w.supersedes(&w));
        // Another id or another thread: both kept, in order.
        assert!(!s.supersedes(&send("t", "b")));
        assert!(!s.supersedes(&send("u", "a")));
        assert!(!w.supersedes(&send("t", "b")));
        // Headers and messages don't replace each other, except an rm,
        // which takes the thread's messages with it.
        let put = Message::ThreadPut { at: 1, key: "t".into(), body: "{}".into() };
        let rm = Message::ThreadRm { at: 1, key: "t".into() };
        assert!(!put.supersedes(&s));
        assert!(!s.supersedes(&put));
        assert!(!s.supersedes(&rm));
        assert!(rm.supersedes(&s));
        assert!(rm.supersedes(&w));
        assert!(!Message::ThreadRm { at: 1, key: "u".into() }.supersedes(&s));
    }

    #[test]
    fn rejects_malformed_message_records() {
        assert!(decode("kind thread-send\nat 1\nkey k\nbody {}\n").unwrap_err().contains("missing `id`"));
        assert!(decode("kind thread-send\nat 1\nkey k\nid a\n").unwrap_err().contains("missing `body`"));
        assert!(decode("kind thread-withdraw\nat 1\nkey k\n").unwrap_err().contains("missing `id`"));
        assert!(decode("kind thread-withdraw\nat 1\nid a\n").unwrap_err().contains("missing `key`"));
        let err = decode("kind thread-withdraw\nat 1\nkey k\nid a\nbody {}\n").unwrap_err();
        assert!(err.contains("`body` is not allowed on a `thread-withdraw` record"), "{err}");
        let err = decode("kind thread-put\nat 1\nkey k\nid a\nbody {}\n").unwrap_err();
        assert!(err.contains("`id` is not allowed on a `thread-put` record"), "{err}");
        assert!(decode("kind thread-rm\nat 1\nkey k\nid a\n").unwrap_err().contains("`id` is not allowed"));
        assert!(decode("level info\nat 1\nmsg hi\nid a\n").unwrap_err().contains("`id` needs"));
        assert!(decode(&format!("{SEND_FIXTURE}id b\n")).unwrap_err().contains("repeated `id`"));
        for bad in ["level info", "link https://x", "msg hi"] {
            let err = decode(&format!("{WITHDRAW_FIXTURE}{bad}\n")).unwrap_err();
            assert!(err.contains("is not allowed on a `thread-withdraw` record"), "{bad}: {err}");
        }
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
