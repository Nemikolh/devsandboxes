//! Process layer for the Instances tree: the third level, showing an instance's
//! running processes as a `ps -ef --forest`-style tree. Forest building and
//! row formatting live here as pure functions so they unit-test without
//! docker; the listing and its parse are the daemon's (`instances.procs`,
//! `runtime::procs`), asked for by `tui::poll` on the event loop's schedule.

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
/// fallback (see [`ProcList`](super::super::runtime::ProcList)). `sessions`:
/// the helper's session count, `None` when it didn't answer a list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcState {
    Rows { rows: Vec<ProcRow>, signalable: bool, sessions: Option<SessionCount> },
    Message(String),
}

/// How many of an instance's helper sessions are running, and how many of
/// those have no client attached: the Detail line and the tree badge.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionCount {
    pub running: usize,
    pub detached: usize,
}

impl SessionCount {
    pub fn of(sessions: &[crate::runtime::procs::Session]) -> SessionCount {
        let running = sessions.iter().filter(|s| s.running());
        SessionCount { running: running.clone().count(), detached: running.filter(|s| !s.attached).count() }
    }
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

    let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut args: HashMap<&str, &str> = HashMap::new();
    for (pid, ppid, arg) in &rows {
        args.insert(pid.as_str(), arg.as_str());
        children.entry(ppid.as_str()).or_default().push(pid.as_str());
    }

    for list in children.values_mut() {
        list.sort_by(|a, b| match (a.parse::<u64>(), b.parse::<u64>()) {
            (Ok(x), Ok(y)) => x.cmp(&y),
            _ => a.cmp(b),
        });
    }

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

/// The gutter label of a run row (docs/architecture/dashboard/tabs-and-actions.md, "Process layer"): not a pid,
/// so the signal shortcuts and the agent count skip it.
pub const RUN_PID: &str = "run";

/// The gutter label of a session row: not a pid either; `a`/`enter`
/// attach it, `K` ends it ([`session_id`]).
pub const SESSION_PID: &str = "session";

/// Runs shown under an instance's processes, newest kept.
pub const MAX_RUN_ROWS: usize = 5;

/// Rows for the last [`MAX_RUN_ROWS`] of `runs` (oldest first, as
/// `instances.procs` answers them), after the process forest: `run` in the
/// gutter, the run minus its start time as args.
pub fn run_rows(runs: &[crate::runtime::procs::Run]) -> Vec<ProcRow> {
    runs[runs.len().saturating_sub(MAX_RUN_ROWS)..].iter().map(run_row).collect()
}

/// One instance's `instances.procs` entry as the tree shows it: the process
/// forest then its runs (docs/architecture/dashboard/tabs-and-actions.md, "Process layer"), or why there's none
/// as a placeholder row.
pub fn proc_state(entry: crate::serve::api::InstanceProcs) -> ProcState {
    use crate::serve::api::InstanceProcs;
    match entry {
        InstanceProcs::Listed(listing) => {
            let triples = listing.processes.into_iter().map(|p| (p.pid, p.ppid, p.args)).collect();
            let mut rows = build_forest(triples);
            rows.extend(run_rows(&listing.runs));
            let sessions = listing.sessions.map(|list| {
                rows.extend(session_rows(&list));
                SessionCount::of(&list)
            });
            ProcState::Rows { rows, signalable: listing.signalable, sessions }
        }
        InstanceProcs::Failed { error } => ProcState::Message(format!("(processes unavailable: {error})")),
    }
}

/// Rows for the running `sessions` (attached or not), after the runs:
/// `session` in the gutter, `<id> attached|detached <argv>` as args. Ended
/// and `lost` ones can't be attached, so aren't rows.
pub fn session_rows(sessions: &[crate::runtime::procs::Session]) -> Vec<ProcRow> {
    sessions
        .iter()
        .filter(|s| s.running() && !s.id.is_empty())
        .map(|s| {
            let mut args = format!("{} {}", s.id, if s.attached { "attached" } else { "detached" });
            if !s.argv.is_empty() {
                args.push(' ');
                args.push_str(&s.argv);
            }
            ProcRow { pid: SESSION_PID.to_string(), depth: 0, args }
        })
        .collect()
}

/// The session id of a [`session_rows`] row (its args' first word), `None`
/// for any other row.
pub fn session_id(row: &ProcRow) -> Option<&str> {
    (row.pid == SESSION_PID).then(|| row.args.split(' ').next()).flatten().filter(|id| !id.is_empty())
}

/// `<id> <state> <argv…>`: the line as the helper printed it, start time left out.
fn run_row(run: &crate::runtime::procs::Run) -> ProcRow {
    let mut args = [run.id.as_str(), run.state.as_str()].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join(" ");
    if !run.argv.is_empty() {
        args.push(' ');
        args.push_str(&run.argv);
    }
    ProcRow { pid: RUN_PID.to_string(), depth: 0, args }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runs(ls: &str) -> Vec<crate::runtime::procs::Run> {
        crate::runtime::procs::parse_runs(ls)
    }

    #[test]
    fn run_rows_keep_the_newest_runs_without_start_times() {
        let ls = "1790000000-a1b2 exited 4 2026-09-21T14:13:20Z sh -c echo hi\n\
1790000001-0000 running 2026-09-21T14:13:21Z zidane -p fix it\n\
1790000002-ffff lost 2026-09-21T14:13:22Z sleep 9\n";
        let rows = run_rows(&runs(ls));
        let args: Vec<&str> = rows.iter().map(|r| r.args.as_str()).collect();
        assert_eq!(
            args,
            [
                "1790000000-a1b2 exited 4 sh -c echo hi",
                "1790000001-0000 running zidane -p fix it",
                "1790000002-ffff lost sleep 9",
            ]
        );
        assert!(rows.iter().all(|r| r.pid == RUN_PID && r.depth == 0));
        let many: String = (0..8).map(|i| format!("17900000{i:02}-0000 lost x y\n")).collect();
        let rows = run_rows(&runs(&many));
        assert_eq!(rows.len(), MAX_RUN_ROWS);
        assert!(rows[0].args.starts_with("1790000003-0000"), "{:?}", rows[0]);
        assert!(run_rows(&[]).is_empty());
    }

    /// A wire entry becomes the forest plus its run rows, or the error as the
    /// placeholder the tree showed before the daemon fetched them.
    #[test]
    fn wire_entries_become_proc_states() {
        use crate::serve::api::{InstanceProcs, Process, ProcsListing};
        let p = |pid: &str, ppid: &str, args: &str| Process { pid: pid.into(), ppid: ppid.into(), args: args.into() };
        let listing = ProcsListing {
            processes: vec![p("1", "0", "init"), p("7", "1", "claude")],
            signalable: true,
            runs: runs("1790000000-a1b2 running 2026-09-21T14:13:20Z sleep 9"),
            sessions: Some(crate::runtime::procs::parse_sessions(
                "1790000001-0000 running detached 2026-09-21T14:13:21Z bash -l\n\
1790000002-0000 exited 0 - 2026-09-21T14:13:22Z vim\n\
1790000003-0000 running attached 2026-09-21T14:13:23Z claude\n\
1790000004-0000 lost - 2026-09-21T14:13:24Z sh",
            )),
        };
        assert_eq!(
            proc_state(InstanceProcs::Listed(listing)),
            ProcState::Rows {
                rows: vec![
                    ProcRow { pid: "1".into(), depth: 0, args: "init".into() },
                    ProcRow { pid: "7".into(), depth: 1, args: "claude".into() },
                    ProcRow { pid: RUN_PID.into(), depth: 0, args: "1790000000-a1b2 running sleep 9".into() },
                    ProcRow { pid: SESSION_PID.into(), depth: 0, args: "1790000001-0000 detached bash -l".into() },
                    ProcRow { pid: SESSION_PID.into(), depth: 0, args: "1790000003-0000 attached claude".into() },
                ],
                signalable: true,
                sessions: Some(SessionCount { running: 2, detached: 1 }),
            }
        );
        let none = ProcsListing { processes: Vec::new(), signalable: true, runs: runs("x running y z"), sessions: Some(Vec::new()) };
        let ProcState::Rows { rows, sessions, .. } = proc_state(InstanceProcs::Listed(none)) else { panic!() };
        assert_eq!((rows.len(), sessions), (1, Some(SessionCount::default())), "no sessions, no session rows");
        let top = ProcsListing { processes: vec![p("660011", "1", "x")], signalable: false, runs: Vec::new(), sessions: None };
        assert!(matches!(proc_state(InstanceProcs::Listed(top)), ProcState::Rows { signalable: false, sessions: None, .. }));
        assert_eq!(
            proc_state(InstanceProcs::Failed { error: "not running".into() }),
            ProcState::Message("(processes unavailable: not running)".into())
        );
    }

    #[test]
    fn session_ids_come_from_session_rows_only() {
        let row = |pid: &str, args: &str| ProcRow { pid: pid.into(), depth: 0, args: args.into() };
        assert_eq!(session_id(&row(SESSION_PID, "1790000001-0000 detached bash")), Some("1790000001-0000"));
        assert_eq!(session_id(&row(RUN_PID, "1790000001-0000 running bash")), None);
        assert_eq!(session_id(&row("42", "bash")), None);
        assert_eq!(session_id(&row(SESSION_PID, "")), None);
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
