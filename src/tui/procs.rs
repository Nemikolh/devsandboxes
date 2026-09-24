//! Process layer for the Instances tree: the third level, showing an instance's
//! running processes as a `ps -ef --forest`-style tree. Parsing and forest
//! building live here as pure functions so they unit-test without docker; the
//! fetch itself (the runtime's `proc_list`) is driven by the event loop.

/// One rendered process row, forest-ordered. `depth` drives the `\_ ` indent at
/// render time; `pid` sits in the TREE gutter and `args` spans the rest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcRow {
    pub pid: String,
    pub depth: usize,
    pub args: String,
}

/// Per-instance process state stored on [`App`](super::app::App). `Rows` is the
/// forest for a running container; `Message` is a single dim placeholder row
/// (not fetched yet, container not running, or a fetch error). `signalable` is
/// true when the rows' pids are container-namespace (from `exec ps`), so the
/// SIGTERM/SIGKILL shortcuts can target them; false for a host-side `top`
/// fallback (see [`ProcList`](super::super::runtime::ProcList)).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcState {
    Rows { rows: Vec<ProcRow>, signalable: bool },
    Message(String),
}

/// The `row` payload of a [`Node::Proc`](super::data::Node::Proc) that stands for
/// a [`ProcState::Message`] placeholder rather than an index into a `Rows` vec.
pub const MESSAGE_ROW: usize = usize::MAX;

/// Known coding-agent command names. Matched against the basename of each
/// whitespace-separated token in a process's args, so a bare `codex`, a
/// `/usr/local/bin/codex`, and a `node …/claude` wrapper all count. Entries are
/// basenames only (no paths, no extensions); keep them distinctive to avoid
/// matching ordinary argument values.
pub const AGENT_NAMES: &[&str] = &[
    "zidane",
    "claude",
    "claude-code",
    "codex",
    "crush",
    "aider",
    "goose",
    "gemini",
    "opencode",
    "cline",
    "qwen",
    "cursor-agent",
    "amp",
    "plandex",
    "copilot",
];

/// True when a process's args name a known coding agent. Each whitespace token is
/// reduced to its path basename (`/usr/local/bin/codex` → `codex`) and compared
/// case-insensitively against [`AGENT_NAMES`]. Flag tokens (`-…`) are skipped so a
/// value that happens to match can't false-positive off a flag.
pub fn is_agent(args: &str) -> bool {
    args.split_whitespace()
        .filter(|tok| !tok.starts_with('-'))
        .any(|tok| {
            let base = tok.rsplit(['/', '\\']).next().unwrap_or(tok);
            AGENT_NAMES.iter().any(|name| base.eq_ignore_ascii_case(name))
        })
}

/// Parse `ps -eo pid,ppid,args` output into
/// `(pid, ppid, args)` triples. The first line is a header and is skipped; each
/// remaining line splits into pid, ppid, and the remainder as args. Lines with
/// fewer than three whitespace-separated fields are skipped.
pub fn parse_top(out: &str) -> Vec<(String, String, String)> {
    out.lines()
        .skip(1)
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let pid = it.next()?;
            let ppid = it.next()?;
            // args = the remainder of the line after pid and ppid. Recover it
            // from the original line so internal spacing in the command is kept.
            let args = remainder_after(line, pid, ppid)?;
            Some((pid.to_string(), ppid.to_string(), args.to_string()))
        })
        .collect()
}

/// The slice of `line` following the `pid` and `ppid` tokens, trimmed of the
/// leading whitespace. `None` when the line doesn't actually contain both
/// tokens in order (defensive; the caller already split them off).
fn remainder_after<'a>(line: &'a str, pid: &str, ppid: &str) -> Option<&'a str> {
    let after_pid = line.trim_start().strip_prefix(pid)?;
    let after_ppid = after_pid.trim_start().strip_prefix(ppid)?;
    let args = after_ppid.trim_start();
    // A line with pid+ppid but no args (fewer than three fields) is skipped.
    if args.is_empty() {
        None
    } else {
        Some(args)
    }
}

/// Build the process forest from parsed `(pid, ppid, args)` rows.
///
/// Roots are procs whose ppid is not itself a pid in the set (orphan ppids —
/// e.g. the host init — become roots). Traversal is depth-first; a node's
/// children are ordered by numeric pid when both parse, falling back to string
/// order. `depth` is the tree depth (roots at 0). A visited set guards against
/// cycles so a malformed ppid loop can't recurse forever.
pub fn build_forest(rows: Vec<(String, String, String)>) -> Vec<ProcRow> {
    use std::collections::{BTreeSet, HashMap};

    let pids: BTreeSet<&str> = rows.iter().map(|(pid, _, _)| pid.as_str()).collect();

    // Children indexed by ppid, in input order; args indexed by pid.
    let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut args: HashMap<&str, &str> = HashMap::new();
    for (pid, ppid, arg) in &rows {
        args.insert(pid.as_str(), arg.as_str());
        children.entry(ppid.as_str()).or_default().push(pid.as_str());
    }

    // Sort each child list by numeric pid where possible, else lexically.
    for list in children.values_mut() {
        list.sort_by(|a, b| match (a.parse::<u64>(), b.parse::<u64>()) {
            (Ok(x), Ok(y)) => x.cmp(&y),
            _ => a.cmp(b),
        });
    }

    // Roots: procs whose ppid isn't a known pid, in numeric-then-string order.
    let mut roots: Vec<&str> = rows
        .iter()
        .filter(|(_, ppid, _)| !pids.contains(ppid.as_str()))
        .map(|(pid, _, _)| pid.as_str())
        .collect();
    roots.sort_by(|a, b| match (a.parse::<u64>(), b.parse::<u64>()) {
        (Ok(x), Ok(y)) => x.cmp(&y),
        _ => a.cmp(b),
    });

    let mut out = Vec::with_capacity(rows.len());
    let mut visited: BTreeSet<&str> = BTreeSet::new();
    let mut stack: Vec<(&str, usize)> = roots.into_iter().rev().map(|pid| (pid, 0)).collect();
    while let Some((pid, depth)) = stack.pop() {
        if !visited.insert(pid) {
            continue; // cycle guard
        }
        out.push(ProcRow {
            pid: pid.to_string(),
            depth,
            args: args.get(pid).copied().unwrap_or("").to_string(),
        });
        if let Some(kids) = children.get(pid) {
            for kid in kids.iter().rev() {
                stack.push((kid, depth + 1));
            }
        }
    }

    // Any pid never reached (a pure ppid cycle with no external root) is emitted
    // as a depth-0 node so no process silently vanishes.
    for (pid, _, arg) in &rows {
        if visited.insert(pid.as_str()) {
            out.push(ProcRow { pid: pid.clone(), depth: 0, args: arg.clone() });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_top_skips_header_and_keeps_arg_spacing() {
        let out = concat!(
            "PID   PPID  COMMAND\n",
            "1     0     /sbin/init\n",
            "42    1     node   server.js  --port 3000\n",
        );
        let rows = parse_top(out);
        assert_eq!(
            rows,
            vec![
                ("1".into(), "0".into(), "/sbin/init".into()),
                ("42".into(), "1".into(), "node   server.js  --port 3000".into()),
            ],
        );
    }

    #[test]
    fn parse_top_skips_short_lines() {
        let out = "PID PPID ARGS\n1 0\n2 0 sh\n";
        let rows = parse_top(out);
        assert_eq!(rows, vec![("2".into(), "0".into(), "sh".into())]);
    }

    fn row(pid: &str, ppid: &str, args: &str) -> (String, String, String) {
        (pid.into(), ppid.into(), args.into())
    }

    #[test]
    fn build_forest_nests_children_under_parents() {
        // ppid 0 is not in the set, so pid 1 is the only root.
        let rows = vec![
            row("1", "0", "init"),
            row("10", "1", "b"),
            row("5", "1", "a"),
            row("11", "5", "a-child"),
        ];
        let forest = build_forest(rows);
        assert_eq!(
            forest,
            vec![
                ProcRow { pid: "1".into(), depth: 0, args: "init".into() },
                // children of 1 ordered by numeric pid: 5 before 10.
                ProcRow { pid: "5".into(), depth: 1, args: "a".into() },
                ProcRow { pid: "11".into(), depth: 2, args: "a-child".into() },
                ProcRow { pid: "10".into(), depth: 1, args: "b".into() },
            ],
        );
    }

    #[test]
    fn build_forest_multiple_orphan_ppids_are_roots() {
        // Neither ppid (0, 99) is a known pid, so both procs are roots.
        let rows = vec![row("7", "99", "g"), row("3", "0", "a")];
        let forest = build_forest(rows);
        let pids: Vec<&str> = forest.iter().map(|r| r.pid.as_str()).collect();
        assert_eq!(pids, vec!["3", "7"]); // numeric root order
        assert!(forest.iter().all(|r| r.depth == 0));
    }

    #[test]
    fn build_forest_survives_a_cycle() {
        // 1→2→1 is a cycle with no external root; the visited set stops it and
        // both nodes still appear exactly once.
        let rows = vec![row("1", "2", "a"), row("2", "1", "b")];
        let forest = build_forest(rows);
        assert_eq!(forest.len(), 2);
        let pids: std::collections::BTreeSet<&str> =
            forest.iter().map(|r| r.pid.as_str()).collect();
        assert_eq!(pids, ["1", "2"].into_iter().collect());
    }

    #[test]
    fn build_forest_empty_is_empty() {
        assert!(build_forest(Vec::new()).is_empty());
    }

    #[test]
    fn is_agent_matches_bare_name_and_path_and_wrapper() {
        assert!(is_agent("codex"));
        assert!(is_agent("/usr/local/bin/codex --resume"));
        // node/python wrappers: the script token is what matches.
        assert!(is_agent("node /opt/claude-code/dist/claude"));
        assert!(is_agent("CLAUDE")); // case-insensitive
        assert!(is_agent("codex exec"));
    }

    #[test]
    fn is_agent_rejects_non_agents_and_partial_matches() {
        assert!(!is_agent(""));
        assert!(!is_agent("/sbin/init"));
        assert!(!is_agent("node server.js"));
        // A basename must equal an agent name, not merely contain it.
        assert!(!is_agent("claudette"));
        assert!(!is_agent("/opt/gemini-tools/run"));
        // A flag token that spells an agent name is not the process.
        assert!(!is_agent("mytool --amp"));
    }
}
