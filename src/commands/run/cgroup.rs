//! The exec cgroup check (docs/architecture/instances/lifecycle.md): whether
//! a process exec'd into a container lands in the container's own cgroup, as
//! `devsbd cgroup check` finds it. Run after create, on every `start` and by
//! `inspect`, never recorded: runc runs afresh for each exec, so a host
//! runtime upgrade fixes existing containers at once. Only a process outside
//! is reported, with what to do about it.

use std::process::{Command, Stdio};

use crate::runtime::backend;

/// The exec of `devsbd cgroup check` in `container`, as its own user (any
/// exec is placed the same way).
fn check_argv(container: &str) -> Vec<String> {
    ["exec", container, crate::devsbd::BIN, "cgroup", "check"].map(String::from).to_vec()
}

/// The cgroup path an exec'd process got, when it lies outside the
/// container's cgroup. `None` when inside or unknown (the exec failed, a
/// helper too old for the check).
pub(crate) fn outside(container: &str) -> Option<String> {
    let out = Command::new(backend().bin()).args(check_argv(container)).stdin(Stdio::null()).output().ok()?;
    verdict(out.status.code(), &String::from_utf8_lossy(&out.stderr))
}

/// `devsbd cgroup check`'s outcome (contract in
/// docs/architecture/helper/in-container.md): exit 1 with its
/// `devsbd: cgroup: outside: <path>` line is outside; anything else is
/// inside or unknown. The line is required, not just the code: the
/// runtime's own exec failures exit 1 too.
fn verdict(code: Option<i32>, stderr: &str) -> Option<String> {
    if code != Some(1) {
        return None;
    }
    stderr.lines().find_map(|l| l.trim().strip_prefix("devsbd: cgroup: outside: ")).map(String::from)
}

/// What `run`, `start` and `inspect` say when `container`'s execs land
/// outside its cgroup (at `path`).
pub(crate) fn warning(container: &str, path: &str) -> String {
    format!(
        "warning: processes exec'd into `{container}` run outside its cgroup ({path}), so a container \
         runtime started from a shell or hook there (docker-in-docker's dockerd) can't start containers. \
         The host's OCI runtime does this (runc before 1.4.1 on a rootless engine): upgrade it, or until \
         then restart the container so its entrypoint starts that daemon"
    )
}

/// Check `container` and print [`warning`] if its execs land outside.
pub(crate) fn warn_if_outside(container: &str) {
    if let Some(path) = outside(container) {
        eprintln!("{}", warning(container, &path));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_needs_the_outside_line() {
        let path = "/../../app.slice/docker.service";
        assert_eq!(verdict(Some(1), &format!("devsbd: cgroup: outside: {path}\n")), Some(path.into()));
        assert_eq!(verdict(Some(0), ""), None);
        assert_eq!(verdict(Some(1), "Error response from daemon: container x is not running\n"), None);
        assert_eq!(verdict(Some(1), "devsbd: cgroup: /proc/self/cgroup: denied\n"), None);
        assert_eq!(verdict(Some(2), "usage: devsbd version|…\n"), None, "a helper too old for the check");
        assert_eq!(verdict(None, ""), None, "killed by a signal");
    }

    #[test]
    fn check_argv_execs_the_helper() {
        assert_eq!(check_argv("c"), ["exec", "c", crate::devsbd::BIN, "cgroup", "check"]);
    }
}
