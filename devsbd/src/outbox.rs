//! `devsbd notify` and the outbox it writes (docs/automations.md, "`devsbd
//! notify`"). A notification is queued as one file per record in
//! `notify::OUTBOX`, so it survives a missing host and container restarts;
//! the daemon's flusher (`daemon.rs`) sends and deletes them. Files are named
//! `<secs:010>-<nanos:09>-<pid>` so a plain name sort is queue order.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::daemon;
use crate::notify::{self, Level, Record};

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
    enqueue(Path::new(notify::OUTBOX), &record, now.subsec_nanos())?;
    poke();
    Ok(())
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
    if notify::encode(&record).len() > notify::MAX_RECORD {
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

/// Write `record` into `dir` atomically: a dot-prefixed temp file (skipped by
/// `pending`) renamed into place, so the flusher never reads half a record.
pub fn enqueue(dir: &Path, record: &Record, nanos: u32) -> io::Result<PathBuf> {
    ensure_dir(dir)?;
    let name = format!("{:010}-{nanos:09}-{}", record.at, std::process::id());
    let tmp = dir.join(format!(".{name}.tmp"));
    let path = dir.join(&name);
    let write = || -> io::Result<()> {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(notify::encode(record).as_bytes())?;
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

    fn rec(at: u64, msg: &str) -> Record {
        Record { level: Level::Info, key: None, link: None, msg: msg.into(), at }
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
