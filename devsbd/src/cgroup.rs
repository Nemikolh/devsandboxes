//! `devsbd cgroup check`: does a process exec'd into this container land in
//! the container's own cgroup? Run by the host as an exec, so it measures
//! exactly what it asks about: a rootless runc before 1.4.1 skips its
//! fallback to init's cgroup when the join fails (a nested cgroup v2 with
//! controllers enabled at the root, as docker-in-docker sets up), leaving
//! the exec'd process in the engine's cgroup, outside the container's
//! namespace. A container runtime started from such a process can't place
//! its own containers (docs/architecture/helper/in-container.md).

const USAGE: &str = "usage: devsbd cgroup check";

pub fn run(args: &[String]) -> i32 {
    if !matches!(args, [verb] if verb == "check") {
        eprintln!("{USAGE}");
        return 2;
    }
    match std::fs::read_to_string("/proc/self/cgroup") {
        Ok(text) => match outside(&text) {
            Some(path) => {
                eprintln!("devsbd: cgroup: outside: {path}");
                1
            }
            None => 0,
        },
        Err(e) => {
            eprintln!("devsbd: cgroup: /proc/self/cgroup: {e}");
            1
        }
    }
}

/// The cgroup v2 path in `/proc/self/cgroup` when it lies outside this
/// cgroup namespace's root (`/..` up front: the kernel's way of naming an
/// ancestor). `None` when inside, or with no v2 entry (cgroup v1 only).
fn outside(text: &str) -> Option<&str> {
    let path = text.lines().find_map(|l| l.strip_prefix("0::"))?;
    (path == "/.." || path.starts_with("/../")).then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outside_reads_the_v2_entry() {
        assert_eq!(outside("0::/../../app.slice/docker.service\n"), Some("/../../app.slice/docker.service"));
        assert_eq!(outside("0::/..\n"), Some("/.."));
        assert_eq!(outside("0::/init\n"), None);
        assert_eq!(outside("0::/\n"), None);
        assert_eq!(outside("0::/..x\n"), None, "a name, not the parent");
        // Hybrid: the v1 lines don't count, the v2 one does.
        assert_eq!(outside("12:memory:/../x\n0::/\n"), None);
        assert_eq!(outside("12:memory:/\n0::/../y\n"), Some("/../y"));
        assert_eq!(outside("12:memory:/../x\n"), None, "v1 only");
    }

    #[test]
    fn usage_takes_check_only() {
        let r = |v: &[&str]| run(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(r(&[]), 2);
        assert_eq!(r(&["check", "--x"]), 2);
        assert_eq!(r(&["probe"]), 2);
    }
}
