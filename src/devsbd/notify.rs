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
//! level warn               required, once: info|warn|error
//! at 1790000000            required, once: unix seconds when queued
//! key pr-123               optional, at most once: dedupe key
//! link https://…           optional, at most once
//! msg PR 123\nneeds you    required, once
//! ```
//!
//! Unknown keys, repeats, a missing required key, or a bad escape are parse
//! errors (the daemon moves such a file aside so it can't block the queue).
//! The host answers each stream with [`REPLY_OK`] once it has the record.

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

pub fn encode(r: &Record) -> String {
    let mut out = String::new();
    let mut line = |key: &str, value: &str| {
        out.push_str(key);
        out.push(' ');
        escape(value, &mut out);
        out.push('\n');
    };
    line("level", r.level.as_str());
    line("at", &r.at.to_string());
    if let Some(key) = &r.key {
        line("key", key);
    }
    if let Some(link) = &r.link {
        line("link", link);
    }
    line("msg", &r.msg);
    out
}

pub fn decode(text: &str) -> Result<Record, String> {
    let (mut level, mut at, mut key, mut link, mut msg) = (None, None, None, None, None);
    for (n, line) in text.split('\n').enumerate() {
        if line.is_empty() {
            continue;
        }
        let err = |e: String| format!("line {}: {e}", n + 1);
        let (name, rest) = line.split_once(' ').unwrap_or((line, ""));
        let value = unescape(rest).map_err(err)?;
        let repeated = || err(format!("repeated `{name}`"));
        match name {
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
            "key" | "link" | "msg" => {
                let slot = match name {
                    "key" => &mut key,
                    "link" => &mut link,
                    _ => &mut msg,
                };
                if slot.replace(value).is_some() {
                    return Err(repeated());
                }
            }
            other => return Err(err(format!("unknown key `{other}`"))),
        }
    }
    let missing = |k: &str| format!("missing `{k}`");
    Ok(Record {
        level: level.ok_or_else(|| missing("level"))?,
        at: at.ok_or_else(|| missing("at"))?,
        msg: msg.ok_or_else(|| missing("msg"))?,
        key,
        link,
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

    fn fixture() -> Record {
        Record {
            level: Level::Warn,
            key: Some("pr-123".into()),
            link: Some("https://example.com/pr/123".into()),
            msg: "PR 123\nneeds you".into(),
            at: 1_790_000_000,
        }
    }

    #[test]
    fn fixture_round_trips() {
        assert_eq!(encode(&fixture()), FIXTURE);
        assert_eq!(decode(FIXTURE), Ok(fixture()));
    }

    #[test]
    fn newlines_nuls_and_backslashes_round_trip() {
        let r = Record {
            level: Level::Error,
            key: Some("k\0\\".into()),
            link: None,
            msg: "a\nb\0c\\n\n".into(),
            at: 0,
        };
        let text = encode(&r);
        assert_eq!(text.lines().count(), 4, "one line per directive");
        assert!(!text.contains('\0'));
        assert_eq!(decode(&text), Ok(r));
    }

    #[test]
    fn optional_fields_and_empty_msg() {
        let r = Record { level: Level::Info, key: None, link: None, msg: String::new(), at: 5 };
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
        assert!(decode("").unwrap_err().contains("missing `level`"));
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
}
