//! `devsandbox machine add|ls|set|rm` (docs/architecture/daemon/machines.md,
//! *`devsandbox machine add|ls|set|rm`*): a client of the daemon's `machines.*`,
//! never in-process, since the registry and the links are the daemon's.
//!
//! `add` without `--config` probes the machine for its candidate roots and
//! lets the user pick (a TTY) or takes the only usable one (no TTY). The
//! picked or typed roots go to `machines.add`, which checks them on the
//! machine itself.
//!
//! Before probing, `add` checks the machine's devsandbox (`install`) and,
//! when it's missing or another version, copies this binary there: asked on
//! a TTY, without asking under `--auto-install`, refused otherwise.

use std::io::{IsTerminal, Write};

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::render::table;
use crate::serve::api::RootCandidate;
use crate::serve::client::{self, Conn};
use crate::serve::endpoint;
use crate::serve::machines::Entry;
use crate::serve::machines::api::{Added, MachineRow, Probed};
use crate::serve::machines::install::{self, Dest, Plan, Remote};

const HEADERS: [&str; 6] = ["NAME", "TARGET", "ROOTS", "AGENT", "STATE", "VERSION"];

pub fn add(target: &str, name: Option<&str>, configs: &[String], agent: bool, auto_install: bool) -> Result<()> {
    let command = ensure_devsandbox(target, auto_install)?;
    let mut conn = connect()?;
    let roots = if configs.is_empty() {
        let probed: Probed = call(&mut conn, "machines.probe", json!({ "target": target, "command": command }))?;
        if std::io::stdin().is_terminal() {
            pick_roots(target, &probed.roots)?
        } else {
            vec![only_root(target, &probed.roots)?]
        }
    } else {
        configs.to_vec()
    };
    let added: Added = call(&mut conn, "machines.add", json!({ "target": target, "name": name, "roots": roots, "agent": agent, "command": command }))?;
    let roots: Vec<String> = added.roots.iter().map(|r| r.display().to_string()).collect();
    println!("added {} ({target}): {}", added.name, roots.join(", "));
    if let Some(warning) = added.warning {
        eprintln!("warning: {warning}");
    }
    Ok(())
}

/// [`install::DEFAULT_DEST`] as the registry's `command`: shell text, the
/// remote login shell expands the `~`.
const LOCAL_COMMAND: &str = "~/.local/bin/devsandbox";

/// Check `target`'s devsandbox and copy this one there when it's missing or
/// another version (docs/architecture/daemon/machines.md, *Install*). The
/// machine's `command`: `None` is `devsandbox` on its login `PATH`.
fn ensure_devsandbox(target: &str, auto_install: bool) -> Result<Option<String>> {
    let ours = env!("CARGO_PKG_VERSION");
    let entry = Entry { name: target.into(), target: target.into(), roots: vec![], command: None, agent: false };
    let remote = install::check(&entry, None)?;
    let Plan::Install { dest, from } = install::decide(&remote, ours) else {
        return Ok(None);
    };
    let found = match &dest {
        Dest::Found(path) => Some(path.as_str()),
        Dest::Default => None,
    };
    if !install::platform_matches(&remote.os, &remote.arch) {
        let why = platform_mismatch(target, &remote);
        if found.is_none() {
            bail!("{why}: install devsandbox there yourself");
        }
        // Today's behaviour: the daemon warns about the other version.
        eprintln!("note: {why}: leaving its devsandbox as it is");
        return Ok(None);
    }
    match consent(std::io::stdin().is_terminal(), auto_install) {
        Consent::Install => {}
        Consent::Refuse => bail!("{}", refusal(target, found, from.as_deref(), ours)),
        Consent::Ask => {
            if !is_yes(&prompt(&question(target, found, from.as_deref(), ours))?) {
                if found.is_none() {
                    bail!("no devsandbox on {target}, so it isn't added");
                }
                return Ok(None);
            }
        }
    }
    let shown = found.unwrap_or(LOCAL_COMMAND);
    eprintln!("copying devsandbox {ours} to {target}:{shown}…");
    let installed = install::install(&entry, dest.path(), &crate::exe::devsandbox()?)?;
    let after = install::check(&entry, None)?;
    let command = match rechecked(&after, &dest, ours, target, &installed)? {
        Rechecked::OnPath => None,
        Rechecked::OffPath => {
            let direct = install::check(&entry, Some(&installed))?;
            if direct.version.as_deref() != Some(ours) {
                bail!("{installed} on {target} doesn't run as devsandbox {ours} after the copy");
            }
            Some(LOCAL_COMMAND.to_string())
        }
    };
    eprintln!("installed devsandbox {ours} at {installed}");
    if command.is_some() {
        eprintln!("note: `devsandbox` on {target}'s login PATH isn't that one, so the machine is registered with command = \"{LOCAL_COMMAND}\"");
    }
    Ok(command)
}

/// Whether to install without asking, ask, or refuse.
#[derive(Debug, PartialEq, Eq)]
enum Consent {
    Install,
    Ask,
    Refuse,
}

fn consent(tty: bool, auto_install: bool) -> Consent {
    match (auto_install, tty) {
        (true, _) => Consent::Install,
        (false, true) => Consent::Ask,
        (false, false) => Consent::Refuse,
    }
}

/// `from` as a sentence's object: `0.7.0`, or that it's unknown.
fn version_text(from: Option<&str>) -> &str {
    from.unwrap_or("of an unknown (older) version")
}

/// The error off a TTY without `--auto-install`. `found`: the machine's
/// devsandbox, `None` when missing.
fn refusal(target: &str, found: Option<&str>, from: Option<&str>, ours: &str) -> String {
    match found {
        None => format!("{target} has no devsandbox: pass --auto-install to copy this one ({ours}) there"),
        Some(path) => format!(
            "{target} runs devsandbox {} at {path}, this is {ours}: pass --auto-install to replace it",
            version_text(from)
        ),
    }
}

/// The `[y/N]` question on a TTY.
fn question(target: &str, found: Option<&str>, from: Option<&str>, ours: &str) -> String {
    match found {
        None => format!("devsandbox isn't on {target}'s PATH. Copy this one ({ours}) to {LOCAL_COMMAND} there? [y/N] "),
        Some(path) => format!(
            "{target} runs devsandbox {} at {path}. Replace it with this one ({ours})? [y/N] ",
            from.unwrap_or("(unknown version)")
        ),
    }
}

fn is_yes(answer: &str) -> bool {
    answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes")
}

fn platform_mismatch(target: &str, remote: &Remote) -> String {
    format!(
        "{target} is {} {}; this devsandbox is built for {} {}, so it can't be copied there",
        remote.os,
        remote.arch,
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

/// What the check after the copy says about the login `PATH`.
#[derive(Debug, PartialEq, Eq)]
enum Rechecked {
    /// `devsandbox` there is ours now.
    OnPath,
    /// Installed to [`LOCAL_COMMAND`], which the login `PATH` doesn't reach
    /// first: the machine's `command` is that path.
    OffPath,
}

/// `after`: the login shell's devsandbox after copying ours to `dest`,
/// `installed` there. An update in place that `PATH` still doesn't resolve
/// to is an error: no `command` can fix that.
fn rechecked(after: &Remote, dest: &Dest, ours: &str, target: &str, installed: &str) -> Result<Rechecked> {
    if after.version.as_deref() == Some(ours) {
        return Ok(Rechecked::OnPath);
    }
    if *dest == Dest::Default {
        return Ok(Rechecked::OffPath);
    }
    match &after.path {
        None => bail!("updated {installed}, but {target}'s login PATH no longer finds a devsandbox"),
        Some(path) => bail!(
            "updated {installed}, but `devsandbox` on {target}'s login PATH is {path} (devsandbox {})",
            version_text(after.version.as_deref())
        ),
    }
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
    let cells: Vec<[String; 6]> = rows.into_iter().map(cells).collect();
    print!("{}", table(&HEADERS, &cells));
    Ok(())
}

pub fn set(name: &str, agent: bool) -> Result<()> {
    let mut conn = connect()?;
    let _: Value = call(&mut conn, "machines.set", json!({ "name": name, "agent": agent }))?;
    println!("{name}: agent {}", on_off(agent));
    Ok(())
}

/// `--agent`'s value: `on` or `off`.
pub fn parse_on_off(s: &str) -> Result<bool, String> {
    match s {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => Err(format!("`{s}` isn't `on` or `off`")),
    }
}

fn on_off(b: bool) -> &'static str {
    if b { "on" } else { "off" }
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
fn cells(r: MachineRow) -> [String; 6] {
    let roots = match r.roots.split_first() {
        None => "-".to_string(),
        Some((first, [])) => first.display().to_string(),
        Some((first, rest)) => format!("{} +{}", first.display(), rest.len()),
    };
    [r.name, r.target, roots, on_off(r.agent).into(), r.state.as_str().into(), r.version.unwrap_or_else(|| "-".into())]
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
            sandboxes: sandboxes.iter().map(|s| SandboxEntry::named(s.to_string())).collect(),
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

    fn remote(path: Option<&str>, version: Option<&str>) -> Remote {
        Remote { os: "Linux".into(), arch: "x86_64".into(), path: path.map(Into::into), version: version.map(Into::into) }
    }

    #[test]
    fn install_asks_on_a_tty_and_needs_the_flag_off_one() {
        assert_eq!(consent(true, false), Consent::Ask);
        assert_eq!(consent(false, false), Consent::Refuse);
        assert_eq!(consent(true, true), Consent::Install);
        assert_eq!(consent(false, true), Consent::Install);
        for yes in ["y", "Y", "yes", "YES"] {
            assert!(is_yes(yes), "{yes}");
        }
        for no in ["", "n", "no", "yep", "y es"] {
            assert!(!is_yes(no), "{no}");
        }
    }

    #[test]
    fn install_messages_name_what_is_there() {
        assert_eq!(
            refusal("box", None, None, "1.0.0"),
            "box has no devsandbox: pass --auto-install to copy this one (1.0.0) there"
        );
        assert_eq!(
            refusal("box", Some("/b/devsandbox"), Some("0.9.0"), "1.0.0"),
            "box runs devsandbox 0.9.0 at /b/devsandbox, this is 1.0.0: pass --auto-install to replace it"
        );
        assert_eq!(
            refusal("box", Some("/b/devsandbox"), None, "1.0.0"),
            "box runs devsandbox of an unknown (older) version at /b/devsandbox, this is 1.0.0: pass --auto-install to replace it"
        );
        assert_eq!(
            question("box", None, None, "1.0.0"),
            "devsandbox isn't on box's PATH. Copy this one (1.0.0) to ~/.local/bin/devsandbox there? [y/N] "
        );
        assert_eq!(
            question("box", Some("/b/devsandbox"), Some("0.9.0"), "1.0.0"),
            "box runs devsandbox 0.9.0 at /b/devsandbox. Replace it with this one (1.0.0)? [y/N] "
        );
        assert_eq!(
            question("box", Some("/b/devsandbox"), None, "1.0.0"),
            "box runs devsandbox (unknown version) at /b/devsandbox. Replace it with this one (1.0.0)? [y/N] "
        );
        let mismatch = Remote { os: "FreeBSD".into(), arch: "riscv64".into(), path: None, version: None };
        let why = platform_mismatch("box", &mismatch);
        assert!(why.starts_with("box is FreeBSD riscv64; this devsandbox is built for "), "{why}");
        assert!(!install::platform_matches(&mismatch.os, &mismatch.arch));
    }

    #[test]
    fn local_command_is_the_default_dest() {
        assert_eq!(LOCAL_COMMAND.strip_prefix('~'), install::DEFAULT_DEST.strip_prefix("$HOME"));
    }

    #[test]
    fn the_recheck_decides_the_command() {
        let found = Dest::Found("/b/devsandbox".into());
        let local = "/home/me/.local/bin/devsandbox";
        let check = |after: &Remote, dest: &Dest, installed: &str| rechecked(after, dest, "1.0.0", "box", installed);
        // On the login PATH now, wherever it went.
        assert_eq!(check(&remote(Some(local), Some("1.0.0")), &Dest::Default, local).unwrap(), Rechecked::OnPath);
        assert_eq!(check(&remote(Some("/b/devsandbox"), Some("1.0.0")), &found, "/b/devsandbox").unwrap(), Rechecked::OnPath);
        // `~/.local/bin` off the login PATH, or behind another devsandbox.
        assert_eq!(check(&remote(None, None), &Dest::Default, local).unwrap(), Rechecked::OffPath);
        assert_eq!(check(&remote(Some("/opt/devsandbox"), Some("0.1.0")), &Dest::Default, local).unwrap(), Rechecked::OffPath);
        // Updated in place, yet PATH resolves elsewhere.
        let err = check(&remote(Some("/opt/devsandbox"), Some("0.1.0")), &found, "/b/devsandbox").unwrap_err().to_string();
        assert_eq!(err, "updated /b/devsandbox, but `devsandbox` on box's login PATH is /opt/devsandbox (devsandbox 0.1.0)");
        let err = check(&remote(Some("/opt/devsandbox"), None), &found, "/b/devsandbox").unwrap_err().to_string();
        assert!(err.ends_with("(devsandbox of an unknown (older) version)"), "{err}");
        let err = check(&remote(None, None), &found, "/b/devsandbox").unwrap_err().to_string();
        assert!(err.contains("no longer finds a devsandbox"), "{err}");
    }

    #[test]
    fn agent_takes_on_or_off() {
        assert_eq!(parse_on_off("on"), Ok(true));
        assert_eq!(parse_on_off("off"), Ok(false));
        for bad in ["", "yes", "true", "ON"] {
            assert!(parse_on_off(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn ls_cells_show_the_first_root() {
        let row = |roots: &[&str], version: Option<&str>| MachineRow {
            name: "m".into(),
            target: "me@m".into(),
            roots: roots.iter().map(Into::into).collect(),
            command: None,
            agent: false,
            state: crate::serve::snapshots::LinkState::Idle,
            error: None,
            version: version.map(String::from),
        };
        assert_eq!(cells(row(&["/a"], None)), ["m", "me@m", "/a", "off", "idle", "-"].map(String::from));
        assert_eq!(cells(row(&["/a", "/b", "/c"], Some("0.7.0")))[2..], ["/a +2", "off", "idle", "0.7.0"].map(String::from));
        assert_eq!(cells(MachineRow { agent: true, ..row(&[], None) })[3], "on");
        assert_eq!(cells(row(&[], None))[2], "-");
    }
}
