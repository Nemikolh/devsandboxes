//! The `run.*` RPC methods (docs/architecture/helper/in-container.md, *Runs
//! over RPC*): `runs.rs`' operations for the host, which calls them instead
//! of exec'ing `devsbd run …` when the daemon lists them (`rpc_server`'s
//! `METHODS`). Each answers what the CLI form prints, as JSON:
//!
//! | Method | Params | Records |
//! |---|---|---|
//! | `run.start` | `argv`, `cwd?`, `env?`, `user?` | `result {id}` |
//! | `run.ls` | — | `result {runs: [line…]}` (`run ls`' lines) |
//! | `run.logs` | `id`, `offset?`, `follow?` | `stdout`…, `result {offset[, state]}` |
//! | `run.wait` | `id`, `timeout?` (s) | `event {state}`…, `result {state}` |
//! | `run.rm` | `id`, `force?` | `result {}` |
//! | `run.prune` | `keep?` | `result {removed}` |
//!
//! A failure is `error failed` with the CLI's message (`no run `x``), bad
//! params `error bad-request`. `run.logs` without `follow` is one chunk (at
//! most `runs::MAX_CHUNK`) from `offset`; with it, every chunk until the run
//! ended and its log is drained, then the end state. `run.wait` holds the
//! stream until the run isn't running or `timeout` passes (`state` then
//! `running`). Both poll the runs dir in here (`runs::POLL`) and stop early
//! when the caller hangs up (`Out::gone`).

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Child;
use std::time::{Duration, Instant};

use crate::boot::{resolve_user, supplementary_groups, Account};
use crate::rpc::{code, Call, Json, Obj, Record};
use crate::rpc_server::{Ctx, Out};
use crate::runs::{self, State, MAX_CHUNK, POLL};

fn bad(msg: impl Into<String>) -> Record {
    Record::error(code::BAD_REQUEST, msg)
}

fn failed(e: impl ToString) -> Record {
    Record::error(code::FAILED, e.to_string())
}

/// The `id` param.
fn id(call: &Call) -> Result<&str, Record> {
    call.params.str("id").ok_or_else(|| bad(format!("{} needs `id`", call.method)))
}

/// An optional non-negative integer param.
fn uint(call: &Call, key: &str) -> Result<Option<u64>, Record> {
    match call.params.get(key) {
        None | Some(Json::Null) => Ok(None),
        Some(Json::Int(n)) if *n >= 0 => Ok(Some(*n as u64)),
        Some(_) => Err(bad(format!("{}: `{key}` must be a non-negative integer", call.method))),
    }
}

/// An optional bool param (absent = false).
fn flag(call: &Call, key: &str) -> Result<bool, Record> {
    match call.params.get(key) {
        None | Some(Json::Null) => Ok(false),
        Some(Json::Bool(b)) => Ok(*b),
        Some(_) => Err(bad(format!("{}: `{key}` must be a bool", call.method))),
    }
}

fn state_obj(state: State) -> Obj {
    Obj::new().with("state", state.label())
}

/// `run.start`: as `devsbd run start --cwd <cwd> -- <argv>` run as `user`
/// (what `<runtime> exec -u <user> -w <cwd> -e …` gives the exec path): the
/// run's dir handed to that user, then its supervisor started as them (their
/// supplementary groups included, [`switch_user`]) with `HOME` (and `USER`)
/// from passwd, like `devsbd boot`, over this daemon's env plus `env`.
/// Without `user`, as the daemon itself.
pub fn start(ctx: &Ctx, call: &Call, _: &mut Out) -> Record {
    let p = &call.params;
    let Some(argv) = p.strs("argv").filter(|a| !a.is_empty()) else {
        return bad("run.start needs a non-empty `argv`");
    };
    let env = match p.get("env") {
        None | Some(Json::Null) => Vec::new(),
        Some(_) => match p.map("env") {
            Some(env) => env,
            None => return bad("run.start: `env` must map names to strings"),
        },
    };
    let account = match p.str("user") {
        None => None,
        Some(user) => match lookup(user) {
            Some(a) => Some(a),
            None => return failed(format!("unknown user `{user}`")),
        },
    };
    let started = runs::start_with(&ctx.runs, argv, p.str("cwd"), |id| {
        if let Some((a, _)) = &account {
            hand_over(&ctx.runs.join(id), a)?;
        }
        let mut cmd = (ctx.supervisor)(&ctx.runs, id);
        if let Some((a, groups)) = &account {
            switch_user(&mut cmd, a.uid, a.gid, groups.clone());
            cmd.env("HOME", &a.home);
            if let Some(name) = &a.name {
                cmd.env("USER", name);
            }
        }
        cmd.envs(env.iter().map(|(k, v)| (k, v)));
        Ok(cmd)
    });
    match started {
        Ok((id, child)) => {
            reap(child);
            Record::Result(Obj::new().with("id", id))
        }
        Err(e) => failed(e),
    }
}

/// `user` (`name|uid[:group|gid]`) against this container's passwd/group,
/// with the groups `docker exec -u` would give it ([`supplementary_groups`]).
pub(crate) fn lookup(user: &str) -> Option<(Account, Vec<u32>)> {
    let passwd = std::fs::read_to_string("/etc/passwd").unwrap_or_default();
    let group = std::fs::read_to_string("/etc/group").unwrap_or_default();
    let a = resolve_user(&passwd, &group, user)?;
    let groups = supplementary_groups(&group, a.name.as_deref(), a.gid);
    Some((a, groups))
}

/// Make `cmd`'s child `uid`/`gid` with supplementary `groups`, as `docker
/// exec -u` does (a run needing e.g. the `docker` group's socket works on
/// both paths). Not `CommandExt::uid`/`gid`: std then calls `setgroups(0)`,
/// clearing them, and `CommandExt::groups` is unstable; a `pre_exec` hook
/// runs after std's own id switch, so it can't add them back after one
/// either. So the whole switch is the hook: `setgroups`, `setgid`, `setuid`,
/// in that order (each needs the privileges the next one drops), each
/// checked. Between fork and exec only async-signal-safe calls: `groups` is
/// allocated here, before the fork, and the hook only reads it.
pub(crate) fn switch_user(cmd: &mut std::process::Command, uid: u32, gid: u32, groups: Vec<u32>) {
    #[cfg(target_os = "linux")]
    type Count = usize;
    #[cfg(not(target_os = "linux"))]
    type Count = std::ffi::c_int;
    unsafe extern "C" {
        fn setgroups(size: Count, list: *const u32) -> i32;
        fn setgid(gid: u32) -> i32;
        fn setuid(uid: u32) -> i32;
    }
    let hook = move || {
        let check = |rc: i32| if rc == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) };
        // SAFETY: plain syscall wrappers; `groups` outlives the calls and
        // isn't resized, so its pointer and length stay valid.
        unsafe {
            check(setgroups(groups.len() as Count, groups.as_ptr()))?;
            check(setgid(gid))?;
            check(setuid(uid))
        }
    };
    // SAFETY: the hook only makes the syscalls above and builds an
    // `io::Error` from errno, which doesn't allocate.
    unsafe {
        cmd.pre_exec(hook);
    }
}

/// Chown a fresh run dir and the files `create` wrote to `a`, so its
/// supervisor (running as `a`) can append to it and `a` can `rm` it, as if
/// `a` had run `devsbd run start` (a session's dir holds the same files).
/// `lchown`: never through a link.
pub(crate) fn hand_over(dir: &Path, a: &Account) -> std::io::Result<()> {
    use std::os::unix::fs::lchown;
    for path in [dir.to_path_buf(), dir.join("argv"), dir.join("meta")] {
        lchown(&path, Some(a.uid), Some(a.gid))?;
    }
    Ok(())
}

/// Wait for a supervisor this daemon spawned, so it doesn't stay a zombie
/// (its cmdline empty: `runs::is_supervisor` already reads that as gone).
pub(crate) fn reap(mut child: Child) {
    let _ = std::thread::Builder::new().name("run-reap".into()).stack_size(32 * 1024).spawn(move || {
        let _ = child.wait();
    });
}

/// `run.ls`: `run ls`' lines.
pub fn ls(ctx: &Ctx, _: &Call, _: &mut Out) -> Record {
    match runs::ls(&ctx.runs) {
        Ok(text) => Record::Result(Obj::new().with("runs", text.lines().map(str::to_string).collect::<Vec<_>>())),
        Err(e) => failed(e),
    }
}

/// `run.logs`: see the module doc.
pub fn logs(ctx: &Ctx, call: &Call, out: &mut Out) -> Record {
    let (id, mut offset, follow) = match (id(call), uint(call, "offset"), flag(call, "follow")) {
        (Ok(id), Ok(offset), Ok(follow)) => (id, offset.unwrap_or(0), follow),
        (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => return e,
    };
    // Set once the run is seen ended: the next empty read is the end.
    let mut ended = None;
    loop {
        let chunk = match runs::read_chunk(&ctx.runs, id, offset, MAX_CHUNK) {
            Ok(chunk) => chunk,
            Err(e) => return failed(e),
        };
        if !chunk.is_empty() {
            offset += chunk.len() as u64;
            if out.send(&Record::Stdout(chunk)).is_err() {
                return failed("the caller is gone");
            }
            if follow {
                continue;
            }
        }
        if !follow {
            return Record::Result(Obj::new().with("offset", offset as i64));
        }
        if let Some(state) = ended {
            return Record::Result(state_obj(state).with("offset", offset as i64));
        }
        let state = match runs::run_dir(&ctx.runs, id).and_then(|dir| runs::load_state(&dir, id)) {
            Ok((_, state)) => state,
            Err(e) => return failed(e),
        };
        if state != State::Running {
            // Read once more: it may have written more before it ended.
            ended = Some(state);
            continue;
        }
        if out.gone() {
            return failed("the caller is gone");
        }
        std::thread::sleep(POLL);
    }
}

/// `run.wait`: see the module doc. `timeout` defaults as `run wait`'s.
pub fn wait(ctx: &Ctx, call: &Call, out: &mut Out) -> Record {
    let (id, timeout) = match (id(call), uint(call, "timeout")) {
        (Ok(id), Ok(t)) => (id, Duration::from_secs(t.unwrap_or(runs::DEFAULT_WAIT))),
        (Err(e), _) | (_, Err(e)) => return e,
    };
    let dir = match runs::run_dir(&ctx.runs, id) {
        Ok(dir) => dir,
        Err(e) => return failed(e),
    };
    let deadline = Instant::now() + timeout;
    let mut told = None;
    loop {
        let state = match runs::load_state(&dir, id) {
            Ok((_, state)) => state,
            Err(e) => return failed(e),
        };
        if state != State::Running || Instant::now() >= deadline || out.gone() {
            return Record::Result(state_obj(state));
        }
        if told != Some(state) {
            told = Some(state);
            if out.send(&Record::Event(state_obj(state))).is_err() {
                return failed("the caller is gone");
            }
        }
        std::thread::sleep(POLL.min(deadline.saturating_duration_since(Instant::now())));
    }
}

/// `run.rm`: as `run rm [--force]`.
pub fn rm(ctx: &Ctx, call: &Call, _: &mut Out) -> Record {
    let (id, force) = match (id(call), flag(call, "force")) {
        (Ok(id), Ok(force)) => (id, force),
        (Err(e), _) | (_, Err(e)) => return e,
    };
    match runs::rm(&ctx.runs, id, force) {
        Ok(()) => Record::Result(Obj::new()),
        Err(e) => failed(e),
    }
}

/// `run.prune`: as `run prune [--keep N]`.
pub fn prune(ctx: &Ctx, call: &Call, _: &mut Out) -> Record {
    let keep = match uint(call, "keep") {
        Ok(keep) => usize::try_from(keep.unwrap_or(0)).unwrap_or(usize::MAX),
        Err(e) => return e,
    };
    match runs::prune(&ctx.runs, keep) {
        Ok(n) => Record::Result(Obj::new().with("removed", n as i64)),
        Err(e) => failed(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc_server::tests::{pair_with, round_trip};
    use crate::rpc_server::METHODS;
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("devsbd-rpc-runs-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        root
    }

    /// The test binary as the supervisor (`runs::tests::supervisor_process`).
    fn supervisor(root: &Path, id: &str) -> Command {
        crate::runs::tests::test_supervisor(&std::env::current_exe().unwrap(), root, id)
    }

    fn ctx(root: &Path) -> Ctx {
        Ctx { root: true, table: METHODS, runs: root.to_path_buf(), supervisor, ..no_sessions() }
    }

    fn no_sessions() -> Ctx {
        crate::rpc_server::tests::ctx(true, METHODS)
    }

    fn call(method: &str, params: Obj) -> Call {
        Call { method: method.into(), params, ..Call::default() }
    }

    /// Every record a call answers, in-process.
    fn records(ctx: &Ctx, method: &str, params: Obj) -> Vec<Record> {
        let mut wire = Vec::new();
        let last = {
            let mut out = Out::new(&mut wire, &|| false);
            let m = METHODS.iter().find(|m| m.name == method).unwrap();
            (m.handler)(ctx, &call(method, params), &mut out)
        };
        let mut r = std::io::Cursor::new(wire);
        let mut got = Vec::new();
        while let Some(rec) = crate::rpc::read_record(&mut r).unwrap() {
            got.push(rec);
        }
        got.push(last);
        got
    }

    fn result(recs: &[Record]) -> &Obj {
        match recs.last() {
            Some(Record::Result(o)) => o,
            other => panic!("not a result: {other:?}"),
        }
    }

    fn stdout(recs: &[Record]) -> Vec<u8> {
        recs.iter()
            .flat_map(|r| match r {
                Record::Stdout(b) => b.clone(),
                _ => Vec::new(),
            })
            .collect()
    }

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn start(ctx: &Ctx, argv: &[&str], env: Vec<(String, String)>) -> String {
        let params = Obj::new().with("argv", strings(argv)).with("env", env);
        let recs = records(ctx, "run.start", params);
        result(&recs).str("id").expect("an id").to_string()
    }

    /// start → wait (events, then the end) → logs (one chunk, offsets, and a
    /// follow that ends with the run) → ls → rm / prune, over a real
    /// supervisor process.
    #[test]
    fn runs_over_rpc_end_to_end() {
        let root = temp_root("e2e");
        let ctx = ctx(&root);
        let env = vec![("RUN_GREETING".to_string(), "hello".to_string())];
        let id = start(&ctx, &["sh", "-c", "sleep 0.5; echo \"$RUN_GREETING\"; exit 3"], env);

        let recs = records(&ctx, "run.wait", Obj::new().with("id", id.as_str()).with("timeout", 10i64));
        assert_eq!(recs.first(), Some(&Record::Event(Obj::new().with("state", "running"))), "{recs:?}");
        assert_eq!(result(&recs).str("state"), Some("exited 3"));
        assert_eq!(recs.len(), 2, "one event while running: {recs:?}");

        let recs = records(&ctx, "run.logs", Obj::new().with("id", id.as_str()));
        assert_eq!(stdout(&recs), b"hello\n");
        assert_eq!(result(&recs), &Obj::new().with("offset", 6i64));
        let recs = records(&ctx, "run.logs", Obj::new().with("id", id.as_str()).with("offset", 2i64));
        assert_eq!(stdout(&recs), b"llo\n");
        let recs = records(&ctx, "run.logs", Obj::new().with("id", id.as_str()).with("offset", 99i64));
        assert_eq!(recs, [Record::Result(Obj::new().with("offset", 99i64))], "nothing past the end");

        // `follow` on a live run: every chunk, then its end.
        let id2 = start(&ctx, &["sh", "-c", "echo a; sleep 0.5; echo b"], Vec::new());
        let recs = records(&ctx, "run.logs", Obj::new().with("id", id2.as_str()).with("follow", true));
        assert_eq!(stdout(&recs), b"a\nb\n");
        assert_eq!(result(&recs), &Obj::new().with("state", "exited 0").with("offset", 4i64));

        let recs = records(&ctx, "run.ls", Obj::new());
        let lines = result(&recs).strs("runs").unwrap().to_vec();
        assert_eq!(lines.join("\n") + "\n", crate::runs::ls(&root).unwrap());
        assert_eq!(lines.len(), 2);

        assert_eq!(records(&ctx, "run.rm", Obj::new().with("id", id.as_str())), [Record::Result(Obj::new())]);
        let recs = records(&ctx, "run.prune", Obj::new().with("keep", 0i64));
        assert_eq!(result(&recs), &Obj::new().with("removed", 1i64));
        assert!(crate::runs::run_ids(&root).unwrap().is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    /// `run.wait` on a live run ends at its timeout, `running`; a run that
    /// already ended answers at once, no event.
    #[test]
    fn wait_times_out_and_answers_ended_runs_at_once() {
        let root = temp_root("wait");
        let ctx = ctx(&root);
        let id = start(&ctx, &["sleep", "5"], Vec::new());
        let t = Instant::now();
        let recs = records(&ctx, "run.wait", Obj::new().with("id", id.as_str()).with("timeout", 1i64));
        assert!(t.elapsed() >= Duration::from_secs(1));
        assert_eq!(recs.last(), Some(&Record::Result(Obj::new().with("state", "running"))));
        let recs = records(&ctx, "run.rm", Obj::new().with("id", id.as_str()).with("force", true));
        assert_eq!(recs, [Record::Result(Obj::new())]);

        let id = start(&ctx, &["true"], Vec::new());
        records(&ctx, "run.wait", Obj::new().with("id", id.as_str()));
        let recs = records(&ctx, "run.wait", Obj::new().with("id", id.as_str()));
        assert_eq!(recs, [Record::Result(Obj::new().with("state", "exited 0"))]);
        let _ = fs::remove_dir_all(&root);
    }

    /// The child gets exactly the uid, gid and supplementary groups asked
    /// for. Needs root to switch; skipped otherwise.
    #[test]
    fn switch_user_sets_ids_and_supplementary_groups() {
        if !std::fs::metadata("/proc/self").map(|m| std::os::unix::fs::MetadataExt::uid(&m) == 0).unwrap_or(false) {
            return;
        }
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "id -u; id -g; id -G"]);
        switch_user(&mut cmd, 65534, 65533, vec![65533, 10, 999]);
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "{out:?}");
        let text = String::from_utf8(out.stdout).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[..2], ["65534", "65533"]);
        let mut groups: Vec<&str> = lines[2].split(' ').collect();
        groups.sort();
        assert_eq!(groups, ["10", "65533", "999"]);
    }

    #[test]
    fn failures_and_bad_params() {
        let root = temp_root("bad");
        let ctx = ctx(&root);
        let missing = "1790000000-ffff";
        let failed_with = |recs: Vec<Record>, needle: &str| match recs.as_slice() {
            [Record::Error { code: c, message }] => {
                assert!(message.contains(needle), "{message}");
                c.clone()
            }
            other => panic!("not one error: {other:?}"),
        };
        for method in ["run.logs", "run.wait", "run.rm"] {
            let c = failed_with(records(&ctx, method, Obj::new().with("id", missing)), "no run `1790000000-ffff`");
            assert_eq!(c, code::FAILED, "{method}");
            let c = failed_with(records(&ctx, method, Obj::new()), "needs `id`");
            assert_eq!(c, code::BAD_REQUEST, "{method}");
        }
        assert_eq!(failed_with(records(&ctx, "run.logs", Obj::new().with("id", "../x")), "no run"), code::FAILED);
        let neg = Obj::new().with("id", missing).with("offset", -1i64);
        assert_eq!(failed_with(records(&ctx, "run.logs", neg), "non-negative"), code::BAD_REQUEST);
        let follow = Obj::new().with("id", missing).with("follow", "yes");
        assert_eq!(failed_with(records(&ctx, "run.logs", follow), "must be a bool"), code::BAD_REQUEST);
        assert_eq!(failed_with(records(&ctx, "run.start", Obj::new()), "non-empty `argv`"), code::BAD_REQUEST);
        let no_user = Obj::new().with("argv", strings(&["true"])).with("user", "no-such-user-here");
        assert_eq!(failed_with(records(&ctx, "run.start", no_user), "unknown user"), code::FAILED);
        let bad_env = Obj::new().with("argv", strings(&["true"])).with("env", Obj::new().with("A", 1i64));
        assert_eq!(failed_with(records(&ctx, "run.start", bad_env), "`env`"), code::BAD_REQUEST);
        assert!(crate::runs::run_ids(&root).unwrap().is_empty(), "nothing started");
        let _ = fs::remove_dir_all(&root);
    }

    /// Over a mux pair: `follow` is one held stream whose records arrive
    /// while the run is still going, ending with the run.
    #[test]
    fn follow_streams_over_a_mux_pair() {
        let root = temp_root("mux");
        let ctx = ctx(&root);
        let id = start(&ctx, &["sh", "-c", "echo one; sleep 0.3; echo two"], Vec::new());
        let r = root.clone();
        let host = pair_with(move || Ctx { root: false, table: METHODS, runs: r.clone(), supervisor, ..no_sessions() });
        let follow = Record::Call(call("run.logs", Obj::new().with("id", id.as_str()).with("follow", true)));
        let recs = round_trip(&host, 1, &[follow]);
        assert_eq!(stdout(&recs), b"one\ntwo\n");
        assert_eq!(result(&recs).str("state"), Some("exited 0"));
        // A non-root daemon serves the reads but not `run.start`.
        let start = Record::Call(call("run.start", Obj::new().with("argv", strings(&["true"]))));
        assert!(matches!(&round_trip(&host, 2, &[start])[..], [Record::Error { code: c, .. }] if c == code::UNKNOWN_METHOD));
        let _ = fs::remove_dir_all(&root);
    }
}
