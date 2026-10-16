//! `devsandbox machine add|ls|rm` (docs/machines.md, step 2): a client of
//! the daemon's `machines.*`, never in-process, since the registry and (from
//! step 3) the links are the daemon's.
//!
//! `add` without `--config` probes the machine for its candidate roots and
//! lets the user pick (a TTY) or takes the only usable one (no TTY). The
//! picked or typed roots go to `machines.add`, which checks them on the
//! machine itself.

use std::io::{IsTerminal, Write};

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::render::table;
use crate::serve::api::RootCandidate;
use crate::serve::client::{self, Conn};
use crate::serve::endpoint;
use crate::serve::machines::api::{Added, MachineRow, Probed};

const HEADERS: [&str; 5] = ["NAME", "TARGET", "ROOTS", "STATE", "VERSION"];

pub fn add(target: &str, name: Option<&str>, configs: &[String]) -> Result<()> {
    let mut conn = connect()?;
    let roots = if configs.is_empty() {
        let probed: Probed = call(&mut conn, "machines.probe", json!({ "target": target }))?;
        if std::io::stdin().is_terminal() {
            pick_roots(target, &probed.roots)?
        } else {
            vec![only_root(target, &probed.roots)?]
        }
    } else {
        configs.to_vec()
    };
    let added: Added = call(&mut conn, "machines.add", json!({ "target": target, "name": name, "roots": roots }))?;
    let roots: Vec<String> = added.roots.iter().map(|r| r.display().to_string()).collect();
    println!("added {} ({target}): {}", added.name, roots.join(", "));
    if let Some(warning) = added.warning {
        eprintln!("warning: {warning}");
    }
    Ok(())
}

pub fn ls(json: bool) -> Result<()> {
    let mut conn = connect()?;
    if json {
        let rows: Value = call(&mut conn, "machines.list", Value::Null)?;
        println!("{}", serde_json::to_string_pretty(&rows).context("serialize machines")?);
        return Ok(());
    }
    let rows: Vec<MachineRow> = call(&mut conn, "machines.list", Value::Null)?;
    if rows.is_empty() {
        println!("no machines; `devsandbox machine add <target>` adds one");
        return Ok(());
    }
    let cells: Vec<[String; 5]> = rows.into_iter().map(cells).collect();
    print!("{}", table(&HEADERS, &cells));
    Ok(())
}

pub fn rm(name: &str) -> Result<()> {
    let mut conn = connect()?;
    let _: Value = call(&mut conn, "machines.rm", json!({ "name": name }))?;
    println!("removed {name}");
    Ok(())
}

fn connect() -> Result<Conn> {
    client::connect(&endpoint::socket_dir()?, "cli").context("`devsandbox machine` needs devsandbox serve")
}

/// One call; an error answer is the command's error, with the daemon's
/// message.
fn call<T: DeserializeOwned>(conn: &mut Conn, method: &str, params: Value) -> Result<T> {
    let r = conn.call(method, params)?;
    if let Some(e) = r.error {
        bail!("{}", e.message);
    }
    serde_json::from_value(r.result.unwrap_or(Value::Null)).with_context(|| format!("unexpected {method} answer"))
}

/// `ls`'s cells: the first root and how many more.
fn cells(r: MachineRow) -> [String; 5] {
    let roots = match r.roots.split_first() {
        None => "-".to_string(),
        Some((first, [])) => first.display().to_string(),
        Some((first, rest)) => format!("{} +{}", first.display(), rest.len()),
    };
    [r.name, r.target, roots, r.state.as_str().into(), r.version.unwrap_or_else(|| "-".into())]
}

/// A candidate as the picker shows it: `/home/me/.devsandboxes  (api, web)`.
fn label(c: &RootCandidate) -> String {
    let what = match &c.error {
        Some(e) => format!("unusable: {e}"),
        None if c.sandboxes.is_empty() => "no sandboxes".into(),
        None => c.sandboxes.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", "),
    };
    format!("{}  ({what})", c.root.display())
}

/// The numbered multi-select on stderr: every candidate, then "other
/// path…", which asks for a path typed as on the machine.
fn pick_roots(target: &str, candidates: &[RootCandidate]) -> Result<Vec<String>> {
    eprintln!("config roots on {target} (one or more, e.g. `1,3`):");
    for (i, c) in candidates.iter().enumerate() {
        eprintln!("  {}) {}", i + 1, label(c));
    }
    eprintln!("  {}) other path…", candidates.len() + 1);
    let selection = parse_selection(&prompt("> ")?, candidates.len())?;
    let mut roots = picked(candidates, &selection)?;
    if selection.other {
        let path = prompt(&format!("path on {target} (`~` is its home): "))?;
        if path.is_empty() {
            bail!("no path given");
        }
        roots.push(path);
    }
    Ok(roots)
}

fn prompt(text: &str) -> Result<String> {
    eprint!("{text}");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(answer.trim().to_string())
}

/// What a typed selection picked: candidate indexes (0-based, in the order
/// typed) and whether "other path…" was among them.
#[derive(Debug, PartialEq, Eq)]
struct Selection {
    roots: Vec<usize>,
    other: bool,
}

/// `1,3`, `1 3` or `1-3` over `candidates` numbered from 1, plus
/// `candidates + 1` for "other path…". Repeats count once.
fn parse_selection(input: &str, candidates: usize) -> Result<Selection> {
    let mut sel = Selection { roots: Vec::new(), other: false };
    let number = |s: &str| -> Result<usize> {
        let n: usize = s.trim().parse().with_context(|| format!("invalid selection `{s}`"))?;
        if n == 0 || n > candidates + 1 {
            bail!("selection {n} out of range (1-{})", candidates + 1);
        }
        Ok(n)
    };
    for token in input.split(|c: char| c == ',' || c.is_whitespace()).filter(|t| !t.is_empty()) {
        let (from, to) = match token.split_once('-') {
            Some((a, b)) => (number(a)?, number(b)?),
            None => (number(token)?, number(token)?),
        };
        if from > to {
            bail!("invalid range `{token}`");
        }
        for n in from..=to {
            if n == candidates + 1 {
                sel.other = true;
            } else if !sel.roots.contains(&(n - 1)) {
                sel.roots.push(n - 1);
            }
        }
    }
    if sel.roots.is_empty() && !sel.other {
        bail!("nothing selected");
    }
    Ok(sel)
}

/// The selected candidates' roots; one with an `error` is refused, saying
/// why (it's listed so the user sees it's there and why it can't be used).
fn picked(candidates: &[RootCandidate], sel: &Selection) -> Result<Vec<String>> {
    sel.roots
        .iter()
        .map(|&i| {
            let c = &candidates[i];
            match &c.error {
                Some(e) => bail!("{} can't be used: {e}", c.root.display()),
                None => Ok(c.root.display().to_string()),
            }
        })
        .collect()
}

/// Off a TTY: the one usable candidate. None or several is an error that
/// lists them and points at `--config`.
fn only_root(target: &str, candidates: &[RootCandidate]) -> Result<String> {
    let usable: Vec<&RootCandidate> = candidates.iter().filter(|c| c.error.is_none()).collect();
    if let [one] = usable[..] {
        return Ok(one.root.display().to_string());
    }
    let listed: String = candidates.iter().map(|c| format!("\n  {}", label(c))).collect();
    let how = "pass --config <path on the machine> (repeatable)";
    if usable.is_empty() {
        bail!("found no usable config root on {target}{listed}\n{how}");
    }
    bail!("{target} has several config roots, so none is taken without a TTY:{listed}\n{how}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve::api::SandboxEntry;

    fn cand(root: &str, sandboxes: &[&str], error: Option<&str>) -> RootCandidate {
        RootCandidate {
            root: root.into(),
            sandboxes: sandboxes.iter().map(|s| SandboxEntry { name: s.to_string() }).collect(),
            error: error.map(String::from),
        }
    }

    #[test]
    fn selections_take_lists_ranges_and_other() {
        let sel = |roots: &[usize], other| Selection { roots: roots.to_vec(), other };
        assert_eq!(parse_selection("1", 3).unwrap(), sel(&[0], false));
        assert_eq!(parse_selection("3,1", 3).unwrap(), sel(&[2, 0], false));
        assert_eq!(parse_selection(" 1 3 ", 3).unwrap(), sel(&[0, 2], false));
        assert_eq!(parse_selection("1, 3,1", 3).unwrap(), sel(&[0, 2], false));
        assert_eq!(parse_selection("2-4", 3).unwrap(), sel(&[1, 2], true));
        assert_eq!(parse_selection("1", 0).unwrap(), sel(&[], true), "no candidates: only other");
        for bad in ["", " , ", "0", "5", "x", "3-1", "1-", "-2"] {
            assert!(parse_selection(bad, 3).is_err(), "{bad:?} parsed");
        }
    }

    #[test]
    fn an_unusable_pick_is_refused() {
        let cands = [cand("/a", &["web"], None), cand("/b", &[], Some("no directory /b"))];
        let sel = Selection { roots: vec![0], other: true };
        assert_eq!(picked(&cands, &sel).unwrap(), ["/a"]);
        let err = picked(&cands, &Selection { roots: vec![0, 1], other: false }).unwrap_err();
        assert_eq!(err.to_string(), "/b can't be used: no directory /b");
    }

    #[test]
    fn off_a_tty_only_one_usable_root_is_taken() {
        let one = [cand("/a", &["web", "api"], None), cand("/b", &[], Some("bad config"))];
        assert_eq!(only_root("box", &one).unwrap(), "/a");
        let err = only_root("box", &[]).unwrap_err().to_string();
        assert!(err.starts_with("found no usable config root on box") && err.contains("--config"), "{err}");
        let err = only_root("box", &one[1..]).unwrap_err().to_string();
        assert!(err.contains("/b  (unusable: bad config)"), "{err}");
        let two = [cand("/a", &["web"], None), cand("/c", &[], None)];
        let err = only_root("box", &two).unwrap_err().to_string();
        assert!(err.contains("several") && err.contains("\n  /a  (web)\n  /c  (no sandboxes)\n") && err.contains("--config"), "{err}");
    }

    #[test]
    fn ls_cells_show_the_first_root() {
        let row = |roots: &[&str], version: Option<&str>| MachineRow {
            name: "m".into(),
            target: "me@m".into(),
            roots: roots.iter().map(Into::into).collect(),
            command: None,
            state: crate::serve::snapshots::LinkState::Idle,
            error: None,
            version: version.map(String::from),
        };
        assert_eq!(cells(row(&["/a"], None)), ["m", "me@m", "/a", "idle", "-"].map(String::from));
        assert_eq!(cells(row(&["/a", "/b", "/c"], Some("0.7.0")))[2..], ["/a +2", "idle", "0.7.0"].map(String::from));
        assert_eq!(cells(row(&[], None))[2], "-");
    }
}
