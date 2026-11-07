//! The opener: `devsbd` invoked as `xdg-open`, `sensible-browser`,
//! `x-www-browser` or `www-browser` (the links the host's install makes in
//! `/run/devsandbox/bin`, and `/usr/local/bin/xdg-open` when nothing else is
//! there), or as `devsbd open <url>`. The container has no desktop, so the
//! URL is queued in the outbox as a `kind open` record and the host opens it
//! (docs/architecture/helper/in-container.md, *Opener*;
//! docs/architecture/helper/bridges.md, *Opening URLs*). Only syntax and
//! length are checked here: the host decides which schemes it opens.

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::notify::{self, Message};
use crate::outbox;

/// The names the opener answers to.
pub const NAMES: [&str; 4] = ["xdg-open", "sensible-browser", "x-www-browser", "www-browser"];

/// `xdg-open`'s exit codes: a bad command line, and "a required tool could
/// not be found" (here: nothing can open a file), and the action failing.
const EXIT_USAGE: i32 = 1;
const EXIT_UNSUPPORTED: i32 = 3;
const EXIT_FAILED: i32 = 4;

const NOT_A_URL: &str = "devsbd: opening files isn't supported; open URLs only";

/// The opener name `argv0`'s basename is, if any.
pub fn invoked_as(argv0: &OsStr) -> Option<&'static str> {
    let base = Path::new(argv0).file_name()?;
    NAMES.into_iter().find(|n| OsStr::new(n) == base)
}

/// Queue the one URL in `args` and poke the daemon; the exit code. `invoked`
/// is what the usage line calls the command (`xdg-open`, `devsbd open`).
pub fn run(invoked: &str, args: Vec<OsString>) -> i32 {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let message = match message(&args, now.as_secs()) {
        Ok(m) => m,
        Err(Refusal::Usage(why)) => {
            eprintln!("{invoked}: {why}\nusage: {invoked} <url>");
            return EXIT_USAGE;
        }
        Err(Refusal::NotAUrl) => {
            eprintln!("{NOT_A_URL}");
            return EXIT_UNSUPPORTED;
        }
    };
    if let Err(e) = outbox::enqueue(Path::new(notify::OUTBOX), &message, now.subsec_nanos()) {
        eprintln!("devsbd: {invoked}: queueing the URL: {e}");
        return EXIT_FAILED;
    }
    outbox::poke();
    0
}

/// Why the arguments can't be queued.
#[derive(Debug, PartialEq, Eq)]
enum Refusal {
    Usage(String),
    NotAUrl,
}

/// The record for `args`: exactly one argument, a URL with a scheme that
/// fits a record.
fn message(args: &[OsString], at: u64) -> Result<Message, Refusal> {
    let [arg] = args else {
        return Err(Refusal::Usage(format!("expected exactly one URL, got {} arguments", args.len())));
    };
    let url = arg.to_str().filter(|u| has_scheme(u)).ok_or(Refusal::NotAUrl)?;
    let message = Message::Open { at, url: url.to_string() };
    if notify::encode(&message).len() > notify::MAX_RECORD {
        return Err(Refusal::Usage(format!("URL longer than {} bytes", notify::MAX_RECORD)));
    }
    Ok(message)
}

/// RFC 3986's `scheme ":"`: a letter, then letters, digits, `+`, `-` or
/// `.`, then a colon, with something after it.
fn has_scheme(s: &str) -> bool {
    let Some((scheme, rest)) = s.split_once(':') else { return false };
    let mut chars = scheme.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        && !rest.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<OsString> {
        a.iter().map(OsString::from).collect()
    }

    #[test]
    fn invoked_as_matches_the_basename() {
        assert_eq!(invoked_as(OsStr::new("/usr/local/bin/xdg-open")), Some("xdg-open"));
        assert_eq!(invoked_as(OsStr::new("/run/devsandbox/bin/sensible-browser")), Some("sensible-browser"));
        assert_eq!(invoked_as(OsStr::new("x-www-browser")), Some("x-www-browser"));
        assert_eq!(invoked_as(OsStr::new("www-browser")), Some("www-browser"));
        assert_eq!(invoked_as(OsStr::new("/run/devsandbox/bin/devsbd")), None);
        assert_eq!(invoked_as(OsStr::new("xdg-open-real")), None);
        assert_eq!(invoked_as(OsStr::new("")), None);
    }

    #[test]
    fn one_url_is_queued() {
        for url in ["https://example.test/a?b=c#d", "http://localhost:8085/cb", "mailto:x@example.test", "vscode://file/x"] {
            assert_eq!(message(&args(&[url]), 7), Ok(Message::Open { at: 7, url: url.into() }), "{url}");
        }
    }

    #[test]
    fn not_exactly_one_argument_is_usage() {
        for a in [&[][..], &["https://a.test", "https://b.test"]] {
            assert!(matches!(message(&args(a), 7), Err(Refusal::Usage(_))), "{a:?}");
        }
    }

    #[test]
    fn paths_and_schemeless_words_are_not_urls() {
        for a in ["report.pdf", "/tmp/x.html", "./a:b", "example.test", "", ":x", "1http://x", "http:", "-h", "a b:c"] {
            assert_eq!(message(&args(&[a]), 7), Err(Refusal::NotAUrl), "{a}");
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let bad = vec![OsString::from_vec(b"https://\xff".to_vec())];
            assert_eq!(message(&bad, 7), Err(Refusal::NotAUrl), "not UTF-8");
        }
    }

    #[test]
    fn an_oversized_url_is_refused() {
        let long = format!("https://example.test/{}", "x".repeat(notify::MAX_RECORD));
        let Err(Refusal::Usage(why)) = message(&args(&[&long]), 7) else { panic!("accepted") };
        assert!(why.contains("longer than"), "{why}");
    }
}
