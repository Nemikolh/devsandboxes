//! `devsbd credential <get|store|erase>`: git's credential helper for the
//! git hosts of the instance's claims (docs/architecture/credentials/git.md).
//! `get` answers `x-access-token` and the claim's phantom for `https` and
//! a host the broker file lists under `credential`; the broker swaps the
//! phantom for the credential. Anything else gets no answer, so git falls
//! through to its next helper. `store` and `erase` are ignored: the phantom
//! never changes on a success or a failure. The phantom comes from the
//! broker file, never the env: git's callers may run under `env -i`.

use std::io::{Read, Write};

use crate::brokerfile;

const USAGE: &str = "usage: devsbd credential get|store|erase";

/// Bound on what git writes: a handful of `key=value` lines.
const MAX_INPUT: u64 = 64 << 10;

pub fn run(args: &[String]) -> i32 {
    let get = match args {
        [op] if op == "get" => true,
        [op] if op == "store" || op == "erase" => false,
        _ => {
            eprintln!("{USAGE}");
            return 2;
        }
    };
    let mut input = String::new();
    // Read even for store/erase: git writes the description to every helper.
    let _ = std::io::stdin().take(MAX_INPUT).read_to_string(&mut input);
    if !get {
        return 0;
    }
    let broker = std::fs::read_to_string(brokerfile::PATH).ok();
    let out = answer(broker.as_deref(), &input);
    let _ = std::io::stdout().write_all(out.as_bytes());
    0
}

/// `get`'s output for git's `input` given the broker file's text (`None`:
/// none): empty unless `input` asks for `https` on a listed host.
pub fn answer(broker: Option<&str>, input: &str) -> String {
    let Some(spec) = broker.and_then(|t| brokerfile::parse(t).ok()) else { return String::new() };
    let mut protocol = None;
    let mut host = None;
    for line in input.lines() {
        if line.is_empty() {
            break;
        }
        match line.split_once('=') {
            Some(("protocol", v)) => protocol = Some(v),
            Some(("host", v)) => host = Some(v),
            _ => {}
        }
    }
    if protocol != Some("https") {
        return String::new();
    }
    let Some(host) = host.map(|h| h.strip_suffix(":443").unwrap_or(h).to_ascii_lowercase()) else {
        return String::new();
    };
    match spec.credentials.iter().find(|c| c.host == host) {
        Some(c) => format!("username=x-access-token\npassword={}\n", c.phantom),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BROKER: &str = "proxy-listen 29001 github.com\ncredential github.com dsbx_github_abc\n";

    #[test]
    fn answers_https_on_a_listed_host_only() {
        let want = "username=x-access-token\npassword=dsbx_github_abc\n";
        assert_eq!(answer(Some(BROKER), "protocol=https\nhost=github.com\n\n"), want);
        assert_eq!(answer(Some(BROKER), "host=GitHub.com:443\nprotocol=https\npath=o/r.git\n"), want);
        assert_eq!(answer(Some(BROKER), "protocol=https\nhost=github.com"), want);
        for input in [
            "protocol=http\nhost=github.com\n",
            "protocol=https\nhost=gitlab.com\n",
            "protocol=https\nhost=github.com:8443\n",
            "protocol=https\n",
            "host=github.com\n",
            "protocol=https\n\nhost=github.com\n",
            "",
        ] {
            assert_eq!(answer(Some(BROKER), input), "", "{input:?}");
        }
    }

    #[test]
    fn no_or_bad_broker_file_answers_nothing() {
        let input = "protocol=https\nhost=github.com\n";
        assert_eq!(answer(None, input), "");
        assert_eq!(answer(Some("bogus\n"), input), "");
        assert_eq!(answer(Some("proxy-listen 29001 github.com\n"), input), "");
    }
}
