//! `devsbd userns check [--as-root]`: can a process here create the user and
//! pid namespaces Chromium's sandbox needs? Run by the host after create, in
//! the real container (its seccomp profile, user and AppArmor), as the exec
//! user (docs/architecture/instances/identity.md, *Tool environment*). The
//! sequence is the zygote's: a new user namespace, then a raw
//! `clone(CLONE_NEWPID)` from inside it (docker's profile filters `clone`,
//! which is what Chrome calls, not `unshare`). `--as-root` first steps into
//! a single-uid namespace at a non-root euid, as the browser wrapper does,
//! and nests the same sequence there.
//!
//! The work happens in a forked child, so the caller's namespaces are
//! untouched. [`enter_userns`] is the reusable core.

const USAGE: &str = "usage: devsbd userns check [--as-root]";

/// The inner id `--as-root` maps the container's root to: non-root, so
/// Chrome keeps its sandbox; any fixed id works, nothing is looked up.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const WRAPPED_ID: u32 = 1001;

/// The step that failed, named as in the outcome line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    UnshareUser,
    Setgroups,
    UidMap,
    GidMap,
    ClonePid,
    Fork,
    Wait,
}

impl Step {
    fn name(self) -> &'static str {
        match self {
            Step::UnshareUser => "unshare(CLONE_NEWUSER)",
            Step::Setgroups => "setgroups",
            Step::UidMap => "uid_map",
            Step::GidMap => "gid_map",
            Step::ClonePid => "clone(CLONE_NEWPID)",
            Step::Fork => "fork",
            Step::Wait => "waitpid",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fail {
    pub step: Step,
    pub errno: i32,
}

pub fn run(args: &[String]) -> i32 {
    let as_root = match args {
        [verb] if verb == "check" => false,
        [verb, flag] if verb == "check" && flag == "--as-root" => true,
        _ => {
            eprintln!("{USAGE}");
            return 2;
        }
    };
    imp::check(as_root)
}

/// The one stderr line of a failed check.
pub fn outcome_line(step: Step, errno: i32) -> String {
    format!("devsbd: userns: {}: {}", step.name(), errno_name(errno))
}

/// The errnos namespace creation is known to fail with, by name; others as
/// the number (Linux values, the same on x86_64 and aarch64).
fn errno_name(errno: i32) -> String {
    match errno {
        1 => "EPERM".into(),
        13 => "EACCES".into(),
        22 => "EINVAL".into(),
        28 => "ENOSPC".into(),
        38 => "ENOSYS".into(),
        n => n.to_string(),
    }
}

/// A `waitpid` status as the exit code (`Ok`) or the killing signal (`Err`).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn exit_status(status: i32) -> Result<i32, i32> {
    if status & 0x7f == 0 { Ok((status >> 8) & 0xff) } else { Err(status & 0x7f) }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::{Fail, Step, WRAPPED_ID, exit_status, outcome_line};
    use std::ffi::{c_int, c_long};

    mod sys {
        use std::ffi::{c_int, c_long};
        pub const CLONE_NEWUSER: c_int = 0x1000_0000;
        pub const CLONE_NEWPID: c_long = 0x2000_0000;
        pub const SIGCHLD: c_long = 17;
        #[cfg(target_arch = "x86_64")]
        pub const SYS_CLONE: c_long = 56;
        #[cfg(target_arch = "aarch64")]
        pub const SYS_CLONE: c_long = 220;

        unsafe extern "C" {
            pub fn unshare(flags: c_int) -> c_int;
            pub fn fork() -> c_int;
            pub fn waitpid(pid: c_int, status: *mut c_int, options: c_int) -> c_int;
            pub fn _exit(code: c_int) -> !;
            pub fn geteuid() -> u32;
            pub fn getegid() -> u32;
            pub fn syscall(n: c_long, ...) -> c_long;
        }
    }

    fn errno() -> i32 {
        std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
    }

    /// Move this process into a new user namespace where `inner_uid` /
    /// `inner_gid` are its current euid/egid: `unshare(CLONE_NEWUSER)`,
    /// `setgroups` denied (required before an unprivileged `gid_map`), then
    /// one-line maps. The process must be single-threaded.
    pub fn enter_userns(inner_uid: u32, inner_gid: u32) -> Result<(), Fail> {
        // SAFETY: plain syscall wrappers; no memory is shared.
        let (uid, gid) = unsafe { (sys::geteuid(), sys::getegid()) };
        if unsafe { sys::unshare(sys::CLONE_NEWUSER) } != 0 {
            return Err(Fail { step: Step::UnshareUser, errno: errno() });
        }
        let write = |path: &str, text: String, step: Step| {
            std::fs::write(path, text).map_err(|e| Fail { step, errno: e.raw_os_error().unwrap_or(0) })
        };
        write("/proc/self/setgroups", "deny".into(), Step::Setgroups)?;
        write("/proc/self/uid_map", format!("{inner_uid} {uid} 1\n"), Step::UidMap)?;
        write("/proc/self/gid_map", format!("{inner_gid} {gid} 1\n"), Step::GidMap)
    }

    /// Fork, run the sequence in the child, exit with its outcome.
    pub fn check(as_root: bool) -> i32 {
        // SAFETY: the helper is single-threaded here; the child only makes
        // syscalls, writes stderr and exits.
        let pid = unsafe { sys::fork() };
        if pid < 0 {
            eprintln!("{}", outcome_line(Step::Fork, errno()));
            return 1;
        }
        if pid == 0 {
            let code = match zygote(as_root) {
                Ok(()) => 0,
                Err(f) => {
                    eprintln!("{}", outcome_line(f.step, f.errno));
                    1
                }
            };
            unsafe { sys::_exit(code) }
        }
        match wait(pid) {
            Ok(Ok(code)) => code,
            Ok(Err(sig)) => {
                eprintln!("devsbd: userns: check killed by signal {sig}");
                1
            }
            Err(f) => {
                eprintln!("{}", outcome_line(f.step, f.errno));
                1
            }
        }
    }

    fn wait(pid: c_int) -> Result<Result<i32, i32>, Fail> {
        let mut status: c_int = 0;
        loop {
            // SAFETY: `status` outlives the call.
            if unsafe { sys::waitpid(pid, &mut status, 0) } == pid {
                return Ok(exit_status(status));
            }
            let e = errno();
            if e != 4 {
                // not EINTR
                return Err(Fail { step: Step::Wait, errno: e });
            }
        }
    }

    /// Chrome's zygote sequence; with `as_root`, nested in a single-uid
    /// namespace at [`WRAPPED_ID`] first.
    fn zygote(as_root: bool) -> Result<(), Fail> {
        if as_root {
            enter_userns(WRAPPED_ID, WRAPPED_ID)?;
        }
        let (uid, gid) = unsafe { (sys::geteuid(), sys::getegid()) };
        enter_userns(uid, gid)?;
        // A null stack makes the raw clone fork-like (the child returns on a
        // copy of this stack), and the remaining args are all 0, so their
        // per-arch order doesn't matter.
        // SAFETY: the child only calls `_exit`.
        let pid = unsafe { sys::syscall(sys::SYS_CLONE, sys::CLONE_NEWPID | sys::SIGCHLD, 0 as c_long, 0 as c_long, 0 as c_long, 0 as c_long) };
        if pid < 0 {
            return Err(Fail { step: Step::ClonePid, errno: errno() });
        }
        if pid == 0 {
            unsafe { sys::_exit(0) }
        }
        match wait(pid as c_int)? {
            Ok(0) => Ok(()),
            // The grandchild only `_exit(0)`s; anything else is the clone's.
            _ => Err(Fail { step: Step::ClonePid, errno: 0 }),
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    pub fn check(_as_root: bool) -> i32 {
        eprintln!("devsbd: userns: Linux only");
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_line_names_step_and_errno() {
        assert_eq!(outcome_line(Step::UnshareUser, 1), "devsbd: userns: unshare(CLONE_NEWUSER): EPERM");
        assert_eq!(outcome_line(Step::UidMap, 1), "devsbd: userns: uid_map: EPERM");
        assert_eq!(outcome_line(Step::GidMap, 22), "devsbd: userns: gid_map: EINVAL");
        assert_eq!(outcome_line(Step::Setgroups, 13), "devsbd: userns: setgroups: EACCES");
        assert_eq!(outcome_line(Step::ClonePid, 28), "devsbd: userns: clone(CLONE_NEWPID): ENOSPC");
        assert_eq!(outcome_line(Step::UnshareUser, 38), "devsbd: userns: unshare(CLONE_NEWUSER): ENOSYS");
        assert_eq!(outcome_line(Step::Fork, 11), "devsbd: userns: fork: 11");
    }

    #[test]
    fn usage_takes_check_and_one_flag() {
        let r = |v: &[&str]| run(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(r(&[]), 2);
        assert_eq!(r(&["probe"]), 2);
        assert_eq!(r(&["check", "--bogus"]), 2);
        assert_eq!(r(&["check", "--as-root", "--as-root"]), 2);
    }

    #[test]
    fn wait_status_decodes() {
        assert_eq!(exit_status(0), Ok(0));
        assert_eq!(exit_status(1 << 8), Ok(1));
        assert_eq!(exit_status(9), Err(9));
    }
}
