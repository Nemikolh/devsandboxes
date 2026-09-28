//! The boot file: what `devsbd boot` runs when the container (re)starts
//! (docs/automations.md, "`\"runtime\"` — the runtime restarts it"). Written by
//! the host, read by the helper; one file shared by both crates (devsbd
//! includes it via `#[path]`) so the two sides can't drift. std-only and
//! hand-parsed: the helper has no dependencies.
//!
//! Format, line-based UTF-8, one directive per line, `<key> <value>`:
//!
//! ```text
//! user vscode              optional, at most once: remoteUser (`name|uid[:group|gid]`)
//! cwd /workspaces/repo     optional, at most once: working directory
//! env KEY=value            any number, applied in order (split at the first `=`)
//! cmd sh\0-c\0echo hi      one per command, run sequentially; argv split on NUL
//! ```
//!
//! Values are escaped (`escape.rs`) so they can hold anything: `\\` → `\`,
//! `\n` → newline, `\0` → NUL (so a raw NUL is only ever `cmd`'s argv
//! separator). Any other escape, an unknown key, or a repeated `user`/`cwd` is
//! a parse error:
//! host and helper ship together, so a mismatch is a bug, not a version skew
//! to tolerate. Empty lines are ignored; a `cmd` with no value has no argv
//! (and is skipped by the runner, like an empty lifecycle argv).

use super::escape::{escape, unescape};

/// Where the host writes the boot file.
pub const PATH: &str = "/run/devsandbox/boot";
/// Where `devsbd boot` appends the commands' output.
pub const LOG: &str = "/run/devsandbox/boot.log";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BootSpec {
    pub user: Option<String>,
    pub cwd: Option<String>,
    pub env: Vec<(String, String)>,
    pub cmds: Vec<Vec<String>>,
}

pub fn serialize(spec: &BootSpec) -> String {
    let mut out = String::new();
    let mut line = |key: &str, parts: &[&str], sep: char| {
        out.push_str(key);
        out.push(' ');
        for (i, part) in parts.iter().enumerate() {
            if i > 0 {
                out.push(sep);
            }
            escape(part, &mut out);
        }
        out.push('\n');
    };
    if let Some(user) = &spec.user {
        line("user", &[user], ' ');
    }
    if let Some(cwd) = &spec.cwd {
        line("cwd", &[cwd], ' ');
    }
    for (key, value) in &spec.env {
        line("env", &[key, value], '=');
    }
    for argv in &spec.cmds {
        let parts: Vec<&str> = argv.iter().map(String::as_str).collect();
        line("cmd", &parts, '\0');
    }
    out
}

pub fn parse(text: &str) -> Result<BootSpec, String> {
    let mut spec = BootSpec::default();
    for (n, line) in text.split('\n').enumerate() {
        if line.is_empty() {
            continue;
        }
        let at = |e: String| format!("line {}: {e}", n + 1);
        let (key, rest) = line.split_once(' ').unwrap_or((line, ""));
        match key {
            "user" | "cwd" => {
                let slot = if key == "user" { &mut spec.user } else { &mut spec.cwd };
                if slot.is_some() {
                    return Err(at(format!("repeated `{key}`")));
                }
                *slot = Some(unescape(rest).map_err(at)?);
            }
            "env" => {
                // Split before unescaping: an escaped `=` can't occur (only
                // `\\`, `\n`, `\0` are escapes), so the first raw `=` ends the key.
                let (k, v) = rest.split_once('=').ok_or_else(|| at("env without `=`".into()))?;
                spec.env.push((unescape(k).map_err(at)?, unescape(v).map_err(at)?));
            }
            "cmd" => {
                let argv = if rest.is_empty() {
                    Vec::new()
                } else {
                    rest.split('\0').map(unescape).collect::<Result<_, _>>().map_err(at)?
                };
                spec.cmds.push(argv);
            }
            other => return Err(at(format!("unknown key `{other}`"))),
        }
    }
    Ok(spec)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Checked from both crates (this module is compiled into each), so the
    /// host's serializer and the helper's parser agree on the exact bytes.
    const FIXTURE: &str = "user vscode\n\
cwd /workspaces/repo\n\
env FOO=bar\n\
env MULTI=a\\nb=c\\\\d\n\
cmd sh\0-c\0echo hi && touch /tmp/x\n\
cmd ./loop.sh\0\0--flag\n";

    fn fixture_spec() -> BootSpec {
        BootSpec {
            user: Some("vscode".into()),
            cwd: Some("/workspaces/repo".into()),
            env: vec![("FOO".into(), "bar".into()), ("MULTI".into(), "a\nb=c\\d".into())],
            cmds: vec![
                vec!["sh".into(), "-c".into(), "echo hi && touch /tmp/x".into()],
                vec!["./loop.sh".into(), "".into(), "--flag".into()],
            ],
        }
    }

    #[test]
    fn fixture_round_trips() {
        assert_eq!(serialize(&fixture_spec()), FIXTURE);
        assert_eq!(parse(FIXTURE), Ok(fixture_spec()));
    }

    #[test]
    fn newlines_and_nuls_in_values_are_escaped() {
        let spec = BootSpec {
            user: None,
            cwd: Some("/a\nb".into()),
            env: vec![("K".into(), "x\ny\0z\\".into())],
            cmds: vec![vec!["printf".into(), "1\n2".into(), "nul\0inside".into()]],
        };
        let text = serialize(&spec);
        // Exactly one line per directive: no raw newline leaked into a value.
        assert_eq!(text.lines().count(), 3);
        assert_eq!(parse(&text), Ok(spec));
    }

    #[test]
    fn empty_spec_and_empty_cmd() {
        assert_eq!(serialize(&BootSpec::default()), "");
        assert_eq!(parse(""), Ok(BootSpec::default()));
        let spec = parse("cmd\n\ncmd \n").unwrap();
        assert_eq!(spec.cmds, vec![Vec::<String>::new(), Vec::new()]);
    }

    #[test]
    fn rejects_malformed() {
        assert!(parse("bogus x\n").unwrap_err().contains("unknown key `bogus`"));
        assert!(parse("user a\nuser b\n").unwrap_err().starts_with("line 2: repeated"));
        assert!(parse("env NOEQUALS\n").is_err());
        assert!(parse("cwd a\\tb\n").unwrap_err().contains("bad escape"));
        assert!(parse("cwd trailing\\\n").is_err());
    }
}
