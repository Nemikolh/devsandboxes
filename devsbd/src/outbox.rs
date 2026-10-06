//! `devsbd notify` / `devsbd thread put|send|withdraw|rm` and the outbox they
//! write (docs/automations.md, "`devsbd notify`", *Inbox threads*). A message
//! is queued as one file per record in `notify::OUTBOX`, so it survives a
//! missing host and container restarts; the daemon's flusher (`daemon.rs`)
//! sends and deletes them. Files are named `<secs:010>-<nanos:09>-<pid>` so a
//! plain name sort is queue order; a record that replaces a queued one takes
//! its place as `<that name's base>+<n:06>` ([`enqueue_thread`]).

use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::control;
use crate::daemon;
use crate::json;
use crate::notify::{self, Level, Message, Record};

/// Suffix of a file the flusher couldn't parse, moved aside so it can't block
/// the queue; left for a human to inspect.
pub const BAD_SUFFIX: &str = ".bad";

const USAGE: &str = "usage: devsbd notify [--level info|warn|error] [--link URL] [--key K] [--] <msg>...";

pub fn run(args: &[String]) -> io::Result<()> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let record = match parse_args(args, now.as_secs()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("devsbd notify: {e}\n{USAGE}");
            std::process::exit(2);
        }
    };
    enqueue(Path::new(notify::OUTBOX), &Message::Notify(record), now.subsec_nanos())?;
    poke();
    Ok(())
}

const THREAD_USAGE: &str = "usage: devsbd thread put [--json '<json>']    (reads stdin without --json)\n       devsbd thread send [--json '<json>']   (reads stdin without --json)\n       devsbd thread withdraw <thread> <id>\n       devsbd thread rm <key>\n       devsbd thread ls [--feed]";

/// What `devsbd thread <verb>` was asked to do. `Put`'s and `Send`'s payload
/// is `None` until stdin has been read, which [`thread`] does outside the
/// parser.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ThreadVerb {
    Put(Option<String>),
    Send(Option<String>),
    Withdraw { thread: String, id: String },
    Rm(String),
}

/// `devsbd thread put|send|withdraw|rm` (docs/inbox-threads.md,
/// docs/inbox-redesign.md): queue a thread message in the same durable outbox
/// `notify` uses, so it lands even with no dashboard open.
pub fn thread(args: &[String]) -> io::Result<()> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let bail = |e: String| -> ! {
        eprintln!("devsbd thread: {e}\n{THREAD_USAGE}");
        std::process::exit(2);
    };
    let stdin = || -> io::Result<Option<String>> {
        let mut json = String::new();
        io::stdin().read_to_string(&mut json)?;
        Ok(Some(json))
    };
    let verb = match parse_thread_args(args) {
        Ok(ThreadVerb::Put(None)) => ThreadVerb::Put(stdin()?),
        Ok(ThreadVerb::Send(None)) => ThreadVerb::Send(stdin()?),
        Ok(verb) => verb,
        Err(e) => bail(e),
    };
    let message = match thread_message(verb, now.as_secs()) {
        Ok(m) => m,
        Err(e) => bail(e),
    };
    enqueue_thread(Path::new(notify::OUTBOX), &message, now.subsec_nanos())?;
    poke();
    Ok(())
}

/// argv after `thread` → the verb. The JSON body is taken verbatim, including
/// a leading `-`, so only `--json` itself is parsed as a flag.
fn parse_thread_args(args: &[String]) -> Result<ThreadVerb, String> {
    let mut it = args.iter();
    let verb = it.next().ok_or("no verb (put|send|withdraw|rm)")?;
    match verb.as_str() {
        "put" | "send" => {
            let mut json = None;
            while let Some(arg) = it.next() {
                let (flag, inline) = match arg.split_once('=') {
                    Some((f, v)) if f == "--json" => (f, Some(v.to_string())),
                    _ => (arg.as_str(), None),
                };
                if flag != "--json" {
                    return Err(format!("unknown argument `{arg}`"));
                }
                let value = inline.or_else(|| it.next().cloned()).ok_or("--json needs a value")?;
                if json.replace(value).is_some() {
                    return Err("--json given twice".into());
                }
            }
            Ok(if verb == "put" { ThreadVerb::Put(json) } else { ThreadVerb::Send(json) })
        }
        "withdraw" => {
            let usage = "withdraw takes exactly <thread> <id>";
            let thread = it.next().ok_or(usage)?.clone();
            let id = it.next().ok_or(usage)?.clone();
            if it.next().is_some() {
                return Err(usage.into());
            }
            Ok(ThreadVerb::Withdraw { thread, id })
        }
        "rm" => {
            let key = it.next().ok_or("rm takes exactly one <key>")?;
            if it.next().is_some() {
                return Err("rm takes exactly one <key>".into());
            }
            Ok(ThreadVerb::Rm(key.clone()))
        }
        other => Err(format!("unknown verb `{other}` (put|send|withdraw|rm)")),
    }
}

/// The record to queue for `verb`. The body is only syntax-checked here and
/// travels opaque (docs/inbox-threads.md, *Decisions*); the key (and a
/// message's thread and id) are pulled out so the outbox can coalesce and the
/// host can report a rejected body against the right thread.
fn thread_message(verb: ThreadVerb, at: u64) -> Result<Message, String> {
    let bad_key = |key: &str| {
        format!(
            "bad key `{key}`: lowercase letters, digits and `-`, starting with a letter or digit, \
             at most {} chars",
            control::MAX_KEY
        )
    };
    let check_message = |thread: &str, id: &str| {
        if !control::valid_key(thread) {
            return Err(bad_key(thread).replacen("bad key", "bad thread", 1));
        }
        if !control::valid_message_id(id) {
            return Err(format!(
                "bad id `{id}`: lowercase letters, digits and `-`, 1-{} chars",
                control::MAX_MESSAGE_ID
            ));
        }
        Ok(())
    };
    let message = match verb {
        ThreadVerb::Put(body) => {
            let body = body.unwrap_or_default();
            let key = json::parse_key(&body)?;
            if !control::valid_key(&key) {
                return Err(bad_key(&key));
            }
            Message::ThreadPut { at, key, body }
        }
        ThreadVerb::Send(body) => {
            let body = body.unwrap_or_default();
            let [thread, id] = json::parse_fields(&body, ["thread", "id"])?;
            check_message(&thread, &id)?;
            // The plan's per-message budget, so a message always fits a
            // record with room to spare; the host checks it again.
            if body.len() > notify::MAX_SEND_BODY {
                return Err(format!("message longer than {} bytes", notify::MAX_SEND_BODY));
            }
            Message::ThreadSend { at, key: thread, id, body }
        }
        ThreadVerb::Withdraw { thread, id } => {
            check_message(&thread, &id)?;
            Message::ThreadWithdraw { at, key: thread, id }
        }
        ThreadVerb::Rm(key) => {
            if !control::valid_key(&key) {
                return Err(bad_key(&key));
            }
            Message::ThreadRm { at, key }
        }
    };
    if notify::encode(&message).len() > notify::MAX_RECORD {
        return Err(format!("thread longer than {} bytes", notify::MAX_RECORD));
    }
    Ok(message)
}

/// Queue `message`, dropping every pending record it makes pointless
/// ([`Message::supersedes`]): a put carries the whole header, a send the
/// whole message, so only the newest matters, and a dispatcher that
/// re-asserts every pass with no dashboard open must not pile up files.
/// Notify records are a log and are never coalesced, even keyed.
///
/// **In place.** The new record takes the queue position of the oldest one it
/// replaces, not the tail: the host inserts a new message at the end of the
/// feed when it first sees it, so moving `send a` behind a later `send b`
/// would swap them in the feed, and moving a put behind the sends that need
/// its thread would get them rejected. It is written as `<base>+<n:06>`, the
/// replaced name's base with the next counter: that sorts right after the
/// replaced name and before any other record (`+` sorts below the digits
/// every other name continues with).
///
/// Racing the flusher is harmless: the new file is in place before the old
/// one goes, the flusher skips a file that vanished before it could read it
/// and tolerates a vanished file after the host's `ok` (see
/// `daemon::flush`). The worst case is the host applying the older record and
/// then the newer one, which is exactly what the two mean.
pub fn enqueue_thread(dir: &Path, message: &Message, nanos: u32) -> io::Result<PathBuf> {
    let replaced: Vec<PathBuf> = pending(dir)?.into_iter().filter(|p| supersedes(p, message)).collect();
    let path = match replaced.first().and_then(|p| p.file_name()?.to_str()) {
        Some(oldest) => write_record(dir, &successor(oldest), message)?,
        None => enqueue(dir, message, nanos)?,
    };
    for old in replaced {
        let _ = fs::remove_file(old);
    }
    Ok(path)
}

/// The name a record replacing queued file `name` is written under:
/// `<base>+<n + 1:06>` for `<base>+<n>`, else `<name>+000001`.
fn successor(name: &str) -> String {
    let (base, n) = match name.split_once('+') {
        Some((base, n)) => (base, n.parse::<u64>().unwrap_or(0)),
        None => (name, 0),
    };
    format!("{base}+{:06}", n + 1)
}

/// Whether `message` replaces the queued file at `path`. Unreadable or
/// unparsable files are left alone: the flusher moves those aside, and
/// guessing at them could drop an unrelated record.
fn supersedes(path: &Path, message: &Message) -> bool {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| notify::decode(&text).ok())
        .is_some_and(|queued| message.supersedes(&queued))
}

/// argv after `notify` → the record to queue. Flags are recognized anywhere
/// before `--` (so `notify "done" --level warn` doesn't silently fold the flag
/// into the message); after `--`, everything is message. The message is the
/// remaining words joined by spaces and must be non-empty.
fn parse_args(args: &[String], at: u64) -> Result<Record, String> {
    let mut record = Record { level: Level::Info, key: None, link: None, msg: String::new(), at };
    let mut words: Vec<&str> = Vec::new();
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
        match flag {
            "--" => {
                words.extend(it.by_ref().map(String::as_str));
                break;
            }
            "--level" => {
                let v = value()?;
                record.level = Level::parse(&v).ok_or_else(|| format!("bad level `{v}` (info|warn|error)"))?;
            }
            "--link" => record.link = Some(value()?),
            "--key" => record.key = Some(value()?),
            f if f.starts_with('-') && f.len() > 1 => return Err(format!("unknown option `{f}`")),
            _ => words.push(arg),
        }
    }
    record.msg = words.join(" ");
    if record.msg.is_empty() {
        return Err("no message".into());
    }
    if notify::encode(&Message::Notify(record.clone())).len() > notify::MAX_RECORD {
        return Err(format!("notification longer than {} bytes", notify::MAX_RECORD));
    }
    Ok(record)
}

/// Create the outbox if missing, world-writable + sticky (`notify` runs as
/// the sandbox user; only root can create it under `/var/lib`, so the
/// installer and the daemon make it too). The chmod is best-effort: as a
/// non-root user it fails on a root-owned dir that is already right.
pub fn ensure_dir(dir: &Path) -> io::Result<()> {
    if !dir.is_dir() {
        fs::create_dir_all(dir)?;
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o1777));
    }
    Ok(())
}

/// Write `message` into `dir` atomically: a dot-prefixed temp file (skipped by
/// `pending`) renamed into place, so the flusher never reads half a record.
pub fn enqueue(dir: &Path, message: &Message, nanos: u32) -> io::Result<PathBuf> {
    let name = format!("{:010}-{nanos:09}-{}", message.at(), std::process::id());
    write_record(dir, &name, message)
}

/// Write `message` atomically as `dir/name` (see [`enqueue`]).
fn write_record(dir: &Path, name: &str, message: &Message) -> io::Result<PathBuf> {
    ensure_dir(dir)?;
    let tmp = dir.join(format!(".{name}.tmp"));
    let path = dir.join(name);
    let write = || -> io::Result<()> {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(notify::encode(message).as_bytes())?;
        f.sync_all()?;
        fs::rename(&tmp, &path)
    };
    write().inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })?;
    Ok(path)
}

/// Queued records in `dir`, oldest first. Skips temp files (leading `.`) and
/// moved-aside ones (`BAD_SUFFIX`). A missing dir is an empty queue.
pub fn pending(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| !n.starts_with('.') && !n.ends_with(BAD_SUFFIX))
        .collect();
    names.sort();
    Ok(names.into_iter().map(|n| dir.join(n)).collect())
}

/// Read and validate one queued file: `Ok(Ok(bytes))` for a well-formed
/// record, `Ok(Err(reason))` when its content is bad and it must be moved
/// aside (not UTF-8, unparseable, over `MAX_RECORD`). `Err` is an I/O error:
/// the flush stops and retries later.
pub fn load(path: &Path) -> io::Result<Result<Vec<u8>, String>> {
    let bytes = fs::read(path)?;
    if bytes.len() > notify::MAX_RECORD {
        return Ok(Err(format!("longer than {} bytes", notify::MAX_RECORD)));
    }
    let verdict = match std::str::from_utf8(&bytes) {
        Ok(text) => notify::decode(text).map(drop),
        Err(_) => Err("not UTF-8".into()),
    };
    Ok(verdict.map(|()| bytes))
}

/// Rename `path` to `<path>.bad`, out of the queue.
pub fn move_aside(path: &Path) -> io::Result<()> {
    let mut bad = path.as_os_str().to_owned();
    bad.push(BAD_SUFFIX);
    fs::rename(path, bad)
}

/// Tell a running daemon there's something to flush. Best-effort: no daemon
/// means the record waits for the next one (or its periodic retry).
fn poke() {
    if let Ok(mut s) = UnixStream::connect(daemon::API_SOCK) {
        let _ = s.write_all(&[daemon::API_POKE]);
    }
}

/// A fresh, not-yet-created directory under the temp dir, for tests that
/// need a real outbox (`notify::OUTBOX` is injectable everywhere it's used).
#[cfg(test)]
pub fn test_dir(tag: &str) -> PathBuf {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!("devsbd-outbox-{tag}-{stamp}-{}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    fn parse(a: &[&str]) -> Result<Record, String> {
        parse_args(&args(a), 42)
    }

    #[test]
    fn args_parse_flags_and_join_the_message() {
        let r = parse(&["--level", "warn", "--key", "pr-1", "--link=https://x", "PR", "1", "conflicted"]).unwrap();
        assert_eq!(
            r,
            Record {
                level: Level::Warn,
                key: Some("pr-1".into()),
                link: Some("https://x".into()),
                msg: "PR 1 conflicted".into(),
                at: 42,
            }
        );
        // Defaults, and flags after words.
        let r = parse(&["done", "--level", "error"]).unwrap();
        assert_eq!((r.level, r.msg.as_str(), r.key), (Level::Error, "done", None));
        // `--` ends option parsing: dashes and `=` stay in the message.
        assert_eq!(parse(&["--", "--level", "-x", "a=b"]).unwrap().msg, "--level -x a=b");
        // A lone `-` is a word; so is a word with `=`.
        assert_eq!(parse(&["-", "k=v"]).unwrap().msg, "- k=v");
    }

    #[test]
    fn args_usage_errors() {
        assert_eq!(parse(&[]).unwrap_err(), "no message");
        assert_eq!(parse(&["--key", "k"]).unwrap_err(), "no message");
        assert_eq!(parse(&["--"]).unwrap_err(), "no message");
        assert!(parse(&["--level", "loud", "m"]).unwrap_err().contains("bad level"));
        assert_eq!(parse(&["m", "--key"]).unwrap_err(), "--key needs a value");
        assert_eq!(parse(&["--link=", "m"]).unwrap_err(), "--link needs a value");
        assert_eq!(parse(&["-v", "m"]).unwrap_err(), "unknown option `-v`");
        assert_eq!(parse(&["--nope=1", "m"]).unwrap_err(), "unknown option `--nope`");
        let long = "x".repeat(notify::MAX_RECORD);
        assert!(parse(&[&long]).unwrap_err().contains("longer than"));
    }

    fn rec(at: u64, msg: &str) -> Message {
        Message::Notify(Record { level: Level::Info, key: None, link: None, msg: msg.into(), at })
    }

    #[test]
    fn enqueue_creates_the_dir_and_pending_is_oldest_first() {
        let dir = test_dir("order").join("outbox");
        assert_eq!(pending(&dir).unwrap(), Vec::<PathBuf>::new(), "missing dir = empty queue");
        // Queued out of order; digit counts differ, so padding is what sorts them.
        let late = enqueue(&dir, &rec(1_000_000_000, "late"), 5).unwrap();
        let early = enqueue(&dir, &rec(999, "early"), 7).unwrap();
        let same_sec_first = enqueue(&dir, &rec(1_000_000_000, "first"), 1).unwrap();
        let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o1777);
        // Noise the queue must skip: temp files, moved-aside files, dirs.
        fs::write(dir.join(".0000000001-000000000-1.tmp"), "half").unwrap();
        fs::write(dir.join(format!("0000000002-000000000-1{BAD_SUFFIX}")), "junk").unwrap();
        fs::create_dir(dir.join("0000000003-sub")).unwrap();
        assert_eq!(pending(&dir).unwrap(), vec![early.clone(), same_sec_first, late]);
        assert_eq!(load(&early).unwrap(), Ok(notify::encode(&rec(999, "early")).into_bytes()));
        let _ = fs::remove_dir_all(dir.parent().unwrap());
    }

    fn put(at: u64, key: &str, title: &str) -> Message {
        Message::ThreadPut { at, key: key.into(), body: format!("{{\"key\":\"{key}\",\"t\":\"{title}\"}}") }
    }

    fn queued(dir: &Path) -> Vec<Message> {
        pending(dir)
            .unwrap()
            .iter()
            // Junk a test planted is skipped, as the coalescer skips it.
            .filter_map(|p| notify::decode(&fs::read_to_string(p).unwrap()).ok())
            .collect()
    }

    #[test]
    fn thread_args_parse() {
        let parse = |a: &[&str]| parse_thread_args(&args(a));
        assert_eq!(parse(&["put"]).unwrap(), ThreadVerb::Put(None), "no --json reads stdin");
        assert_eq!(parse(&["put", "--json", "{}"]).unwrap(), ThreadVerb::Put(Some("{}".into())));
        assert_eq!(parse(&["put", "--json={}"]).unwrap(), ThreadVerb::Put(Some("{}".into())));
        assert_eq!(parse(&["rm", "pr-1"]).unwrap(), ThreadVerb::Rm("pr-1".into()));
        assert_eq!(parse(&["send"]).unwrap(), ThreadVerb::Send(None), "no --json reads stdin");
        assert_eq!(parse(&["send", "--json={}"]).unwrap(), ThreadVerb::Send(Some("{}".into())));
        assert_eq!(
            parse(&["withdraw", "pr-1", "run-1"]).unwrap(),
            ThreadVerb::Withdraw { thread: "pr-1".into(), id: "run-1".into() }
        );
        for bad in [&["withdraw"][..], &["withdraw", "pr-1"], &["withdraw", "a", "b", "c"]] {
            assert!(parse(bad).unwrap_err().contains("exactly <thread> <id>"), "{bad:?}");
        }
        assert!(parse(&["send", "{}"]).unwrap_err().contains("unknown argument"));
        assert_eq!(parse(&[]).unwrap_err(), "no verb (put|send|withdraw|rm)");
        assert!(parse(&["ls"]).unwrap_err().contains("unknown verb `ls`"));
        assert!(parse(&["put", "{}"]).unwrap_err().contains("unknown argument"));
        assert_eq!(parse(&["put", "--json"]).unwrap_err(), "--json needs a value");
        assert_eq!(parse(&["put", "--json=a", "--json=b"]).unwrap_err(), "--json given twice");
        assert!(parse(&["rm"]).unwrap_err().contains("exactly one <key>"));
        assert!(parse(&["rm", "a", "b"]).unwrap_err().contains("exactly one <key>"));
    }

    #[test]
    fn thread_messages_check_the_json_and_the_key() {
        let put = |json: &str| thread_message(ThreadVerb::Put(Some(json.into())), 7);
        assert_eq!(
            put(r#"{"key":"pr-1","title":"x"}"#).unwrap(),
            Message::ThreadPut { at: 7, key: "pr-1".into(), body: r#"{"key":"pr-1","title":"x"}"#.into() }
        );
        assert_eq!(
            thread_message(ThreadVerb::Rm("pr-1".into()), 7).unwrap(),
            Message::ThreadRm { at: 7, key: "pr-1".into() }
        );
        // Syntax, shape and the key rules, each a one-liner.
        assert!(put("{").unwrap_err().contains("expected"));
        assert!(put("[]").unwrap_err().contains("expected a JSON object"));
        assert_eq!(put("{}").unwrap_err(), "no `key` field");
        assert_eq!(put(r#"{"key":1}"#).unwrap_err(), "`key` is not a string");
        for bad in [r#"{"key":"PR-1"}"#, r#"{"key":""}"#, r#"{"key":"a b"}"#, r#"{"key":"-a"}"#] {
            assert!(put(bad).unwrap_err().starts_with("bad key "), "{bad}");
        }
        assert!(thread_message(ThreadVerb::Rm("A".into()), 7).unwrap_err().starts_with("bad key "));
        // The record cap applies to a body as much as to a message.
        let big = format!("{{\"key\":\"k\",\"m\":\"{}\"}}", "x".repeat(notify::MAX_RECORD));
        assert!(put(&big).unwrap_err().contains("longer than"));
    }

    #[test]
    fn queuing_a_thread_message_coalesces_the_same_key_in_place() {
        let dir = test_dir("coalesce");
        // A keyed notify record for the same key: a log entry, kept.
        let note = Message::Notify(Record {
            level: Level::Info,
            key: Some("pr-1".into()),
            link: None,
            msg: "hi".into(),
            at: 1,
        });
        enqueue(&dir, &note, 0).unwrap();
        enqueue_thread(&dir, &put(2, "pr-1", "v1"), 1).unwrap();
        enqueue_thread(&dir, &put(3, "pr-2", "other"), 2).unwrap();
        enqueue_thread(&dir, &put(4, "pr-1", "v2"), 3).unwrap();
        // The newer put took the older one's place, ahead of pr-2's.
        assert_eq!(queued(&dir), [note.clone(), put(4, "pr-1", "v2"), put(3, "pr-2", "other")]);
        // An rm supersedes a pending put for its key, in its place too.
        let rm = Message::ThreadRm { at: 5, key: "pr-1".into() };
        enqueue_thread(&dir, &rm, 4).unwrap();
        assert_eq!(queued(&dir), [note.clone(), rm.clone(), put(3, "pr-2", "other")]);
        // A put after an rm is a fresh thread: both stay, rm first.
        let names = |dir: &Path| pending(dir).unwrap().iter().map(|p| p.file_name().unwrap().to_owned()).collect::<Vec<_>>();
        let junk = dir.join("0000000000-000000000-9");
        fs::write(&junk, "not a record").unwrap();
        enqueue_thread(&dir, &put(6, "pr-1", "v3"), 5).unwrap();
        assert_eq!(queued(&dir), [note, rm, put(3, "pr-2", "other"), put(6, "pr-1", "v3")]);
        // A file the flusher already moved aside, and junk, are left alone.
        assert!(junk.is_file(), "an unparsable file is not guessed at");
        assert!(names(&dir).iter().any(|n| n.to_str().unwrap().ends_with("+000002")), "{:?}", names(&dir));
        let _ = fs::remove_dir_all(&dir);
    }

    fn send(at: u64, thread: &str, id: &str, text: &str) -> Message {
        let body = format!(r#"{{"thread":"{thread}","id":"{id}","blocks":[{{"type":"markdown","text":"{text}"}}]}}"#);
        Message::ThreadSend { at, key: thread.into(), id: id.into(), body }
    }

    fn withdraw(at: u64, thread: &str, id: &str) -> Message {
        Message::ThreadWithdraw { at, key: thread.into(), id: id.into() }
    }

    #[test]
    fn sends_coalesce_per_thread_and_id_keeping_cross_id_order() {
        let dir = test_dir("coalesce-send");
        let put = put(1, "t", "h");
        enqueue_thread(&dir, &put, 0).unwrap();
        enqueue_thread(&dir, &send(2, "t", "a", "a1"), 1).unwrap();
        enqueue_thread(&dir, &send(3, "t", "b", "b1"), 2).unwrap();
        enqueue_thread(&dir, &send(4, "u", "a", "other thread"), 3).unwrap();
        // `a` re-sent: the newest body, still ahead of `b`.
        enqueue_thread(&dir, &send(5, "t", "a", "a2"), 4).unwrap();
        assert_eq!(queued(&dir), [put.clone(), send(5, "t", "a", "a2"), send(3, "t", "b", "b1"), send(4, "u", "a", "other thread")]);
        // A withdraw replaces a queued send of its id, and a send a withdraw.
        enqueue_thread(&dir, &withdraw(6, "t", "b"), 5).unwrap();
        assert_eq!(queued(&dir)[2], withdraw(6, "t", "b"));
        enqueue_thread(&dir, &send(7, "t", "b", "b2"), 6).unwrap();
        enqueue_thread(&dir, &send(8, "t", "a", "a3"), 7).unwrap();
        assert_eq!(
            queued(&dir),
            [put.clone(), send(8, "t", "a", "a3"), send(7, "t", "b", "b2"), send(4, "u", "a", "other thread")]
        );
        // A put leaves queued sends alone; an rm takes its thread's with it.
        enqueue_thread(&dir, &self::put(9, "t", "h2"), 8).unwrap();
        assert_eq!(queued(&dir).len(), 4);
        let rm = Message::ThreadRm { at: 10, key: "t".into() };
        enqueue_thread(&dir, &rm, 9).unwrap();
        assert_eq!(queued(&dir), [rm, send(4, "u", "a", "other thread")]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn successor_names_sort_right_after_the_replaced_one() {
        assert_eq!(successor("0000000001-000000002-12"), "0000000001-000000002-12+000001");
        assert_eq!(successor("0000000001-000000002-12+000009"), "0000000001-000000002-12+000010");
        let mut names = vec![
            "0000000001-000000002-123".to_string(),
            successor("0000000001-000000002-12"),
            "0000000001-000000002-12".to_string(),
            successor(&successor("0000000001-000000002-12")),
            "0000000001-000000002-13".to_string(),
        ];
        names.sort();
        assert_eq!(
            names,
            [
                "0000000001-000000002-12",
                "0000000001-000000002-12+000001",
                "0000000001-000000002-12+000002",
                "0000000001-000000002-123",
                "0000000001-000000002-13",
            ]
        );
    }

    #[test]
    fn send_and_withdraw_check_the_json_the_thread_and_the_id() {
        let send = |json: &str| thread_message(ThreadVerb::Send(Some(json.into())), 7);
        let body = r#"{"thread":"pr-1","id":"run-1","blocks":[]}"#;
        assert_eq!(
            send(body).unwrap(),
            Message::ThreadSend { at: 7, key: "pr-1".into(), id: "run-1".into(), body: body.into() }
        );
        assert!(send("{").unwrap_err().contains("expected"));
        assert_eq!(send(r#"{"id":"a"}"#).unwrap_err(), "no `thread` field");
        assert_eq!(send(r#"{"thread":"t"}"#).unwrap_err(), "no `id` field");
        assert!(send(r#"{"thread":"T","id":"a"}"#).unwrap_err().starts_with("bad thread `T`"));
        for bad in ["", "A", "a_b", &"x".repeat(61)] {
            let err = send(&format!(r#"{{"thread":"t","id":"{bad}"}}"#)).unwrap_err();
            assert!(err.starts_with("bad id "), "{bad}: {err}");
        }
        // The message budget, under the record cap.
        let pad = "x".repeat(notify::MAX_SEND_BODY);
        let err = send(&format!(r#"{{"thread":"t","id":"a","p":"{pad}"}}"#)).unwrap_err();
        assert_eq!(err, format!("message longer than {} bytes", notify::MAX_SEND_BODY));
        let withdraw = |t: &str, id: &str| thread_message(ThreadVerb::Withdraw { thread: t.into(), id: id.into() }, 7);
        assert_eq!(withdraw("pr-1", "run-1").unwrap(), self::withdraw(7, "pr-1", "run-1"));
        assert!(withdraw("-x", "a").unwrap_err().starts_with("bad thread "));
        assert!(withdraw("t", "A").unwrap_err().starts_with("bad id "));
    }

    #[test]
    fn bad_files_are_reported_and_moved_aside() {
        let dir = test_dir("bad");
        fs::create_dir_all(&dir).unwrap();
        let junk = dir.join("0000000001-000000000-1");
        fs::write(&junk, "not a record").unwrap();
        let binary = dir.join("0000000002-000000000-1");
        fs::write(&binary, [0xff, 0xfe]).unwrap();
        let big = dir.join("0000000003-000000000-1");
        fs::write(&big, "x".repeat(notify::MAX_RECORD + 1)).unwrap();
        assert!(load(&junk).unwrap().unwrap_err().contains("unknown key"));
        assert_eq!(load(&binary).unwrap(), Err("not UTF-8".into()));
        assert!(load(&big).unwrap().unwrap_err().contains("longer than"));
        move_aside(&junk).unwrap();
        assert!(dir.join(format!("0000000001-000000000-1{BAD_SUFFIX}")).is_file());
        assert_eq!(pending(&dir).unwrap(), vec![binary, big]);
        let _ = fs::remove_dir_all(&dir);
    }
}
