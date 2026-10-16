//! An instance's processes and runs as the daemon's `instances.procs`
//! answers them (docs/reference/api.md): the parse of a container's `ps -eo
//! pid,ppid,args` listing and of its helper's `devsbd run ls`, pure so they
//! unit-test without docker, plus the bounded `devsbd run ls` probe itself.
//! The dashboard's forest and row formatting stay in `tui::procs`.

use std::time::Duration;

use serde::{Deserialize, Serialize};

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

/// One tracked run of a container's helper (docs/architecture/helper/in-container.md),
/// as a `devsbd run ls` line `<id> <state> <started> <argv…>` says it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    /// `running`, `lost`, or with its code: `exited 4`, `killed 9`.
    pub state: String,
    /// RFC 3339, as the helper wrote it.
    pub started: String,
    /// The command, space-joined as the helper printed it.
    pub argv: String,
}

/// The runs in `devsbd run ls` output, oldest first; blank lines skipped.
/// Lenient: a line short of fields leaves them empty rather than dropping
/// the run (the helper's output is container-controlled, not trusted to be
/// well-formed, but a run it names is still shown).
pub fn parse_runs(ls: &str) -> Vec<Run> {
    ls.lines().filter(|l| !l.trim().is_empty()).map(parse_run).collect()
}

/// One `devsbd run ls` line. Split on single spaces, as the helper joins
/// its fields, so `argv` keeps its own spacing.
fn parse_run(line: &str) -> Run {
    let words: Vec<&str> = line.split(' ').collect();
    // `exited N` / `killed N` are two words, `running` / `lost` one.
    let state_len = match words.get(1) {
        Some(&"exited" | &"killed") => 2,
        _ => 1,
    };
    let join = |from: usize, to: usize| words.get(from..to.min(words.len())).unwrap_or(&[]).join(" ");
    Run {
        id: words.first().copied().unwrap_or("").to_string(),
        state: join(1, 1 + state_len),
        started: words.get(1 + state_len).copied().unwrap_or("").to_string(),
        argv: join(2 + state_len, words.len()),
    }
}

/// Stdout kept from a child's `devsbd run ls` before it's given up on.
const RUN_LS_CAP: usize = 64 * 1024;

/// How long a child's `devsbd run ls` may take before it's killed.
const RUN_LS_TIMEOUT: Duration = Duration::from_secs(5);

/// `devsbd run ls` in `container`, bounded in size and time (its output is
/// container-controlled). `None` on any failure — no helper, one predating
/// runs, a timeout, or output past [`RUN_LS_CAP`] (dropped whole rather than
/// parsed cut off): the instance then just shows no runs.
pub fn run_ls(container: &str) -> Option<String> {
    let backend = super::backend();
    let mut cmd = std::process::Command::new(backend.bin());
    cmd.args(["exec", container, crate::devsbd::BIN, "run", "ls"]).stdin(std::process::Stdio::null());
    let out = super::bounded::run(&mut cmd, RUN_LS_CAP, RUN_LS_TIMEOUT).ok()?;
    (out.status.success() && !out.truncated).then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
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

    fn run(id: &str, state: &str, started: &str, argv: &str) -> Run {
        Run { id: id.into(), state: state.into(), started: started.into(), argv: argv.into() }
    }

    #[test]
    fn parse_runs_splits_state_with_its_code() {
        let ls = "1790000000-a1b2 exited 4 2026-09-21T14:13:20Z sh -c echo  hi\n\
\n\
1790000001-0000 running 2026-09-21T14:13:21Z zidane -p fix it\n\
1790000002-ffff killed 9 2026-09-21T14:13:22Z sleep 9\n";
        assert_eq!(
            parse_runs(ls),
            [
                run("1790000000-a1b2", "exited 4", "2026-09-21T14:13:20Z", "sh -c echo  hi"),
                run("1790000001-0000", "running", "2026-09-21T14:13:21Z", "zidane -p fix it"),
                run("1790000002-ffff", "killed 9", "2026-09-21T14:13:22Z", "sleep 9"),
            ]
        );
        assert!(parse_runs("").is_empty());
    }

    #[test]
    fn parse_runs_keeps_short_lines_with_empty_fields() {
        assert_eq!(parse_runs("x exited"), [run("x", "exited", "", "")]);
        assert_eq!(parse_runs("x lost 2026"), [run("x", "lost", "2026", "")]);
        assert_eq!(parse_runs("x"), [run("x", "", "", "")]);
    }
}
