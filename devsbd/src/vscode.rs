//! `devsbd vscode-goto <ABS_PATH>[:LINE[:COL]] [--wait SECS]`: open a file at
//! a line in the VS Code window attached to this container, by running the
//! server's own remote CLI (`code -g`) against the window's IPC socket.
//!
//! Which socket (docs/inbox-threads.md, *VS Code at a line*): every
//! `/tmp/vscode-ipc-*.sock` accepts connections, but server-main owns one per
//! integrated terminal and the agent host owns one too; only the socket held
//! by a `--type=extensionHost` process is the window's. With several, the
//! newest extension host (last window opened or reloaded) wins.
//!
//! Root in a container usually lacks `CAP_SYS_PTRACE`, so it can't read
//! another user's `/proc/<pid>/fd`: the host's root exec finds extension
//! hosts by their world-readable cmdline and, when it can't see the newest
//! one's fds, re-runs this verb as that host's owner, who can.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const USAGE: &str = "usage: devsbd vscode-goto <ABS_PATH>[:LINE[:COL]] [--wait SECS]";
const MAX_WAIT: u64 = 60;
/// Exit code when no window is attached, so the host can say "dropped".
const NO_WINDOW: i32 = 3;
const PATH: &str = "/usr/local/bin:/usr/bin:/bin";

pub fn run(args: &[String]) -> i32 {
    let Some((goto, wait)) = parse_args(args) else {
        eprintln!("{USAGE}");
        return 2;
    };
    let euid = match fs::metadata("/proc/self") {
        Ok(m) => m.uid(),
        Err(e) => {
            eprintln!("devsbd: /proc/self: {e}");
            return 1;
        }
    };
    let deadline = Instant::now() + Duration::from_secs(wait);
    let found = loop {
        if let Some(f) = find(euid == 0) {
            break f;
        }
        if Instant::now() >= deadline {
            eprintln!("devsbd: no VS Code window attached");
            return NO_WINDOW;
        }
        std::thread::sleep(Duration::from_millis(500));
    };
    let result = match found {
        Found::Window(host, socket) => exec_cli(&host, &socket, &goto, euid),
        Found::Delegate(uid, gid) => {
            let left = deadline.saturating_duration_since(Instant::now()).as_secs();
            delegate(uid, gid, &goto, left)
        }
    };
    result.unwrap_or_else(|e| {
        eprintln!("devsbd: {e}");
        1
    })
}

/// `(goto argument for `code -g`, wait seconds)`.
fn parse_args(args: &[String]) -> Option<(String, u64)> {
    let (mut goto, mut wait) = (None, 0);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--wait" => wait = it.next()?.parse::<u64>().ok()?.min(MAX_WAIT),
            _ if goto.is_none() => goto = Some(parse_goto(a)?),
            _ => return None,
        }
    }
    Some((goto?, wait))
}

/// Validate `PATH[:LINE[:COL]]` and return it as `code -g` takes it. Paths may
/// hold `:`, so LINE/COL are only taken from the right when numeric.
fn parse_goto(arg: &str) -> Option<String> {
    let numeric = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let (path, nums) = match arg.rsplit_once(':') {
        Some((rest, n)) if numeric(n) => match rest.rsplit_once(':') {
            Some((p, l)) if numeric(l) => (p, vec![l, n]),
            _ => (rest, vec![n]),
        },
        _ => (arg, vec![]),
    };
    if !path.starts_with('/') || path.contains('\0') {
        return None;
    }
    let mut out = path.to_string();
    for n in nums {
        let n: u32 = n.parse().ok().filter(|&n| n > 0)?;
        out.push_str(&format!(":{n}"));
    }
    Some(out)
}

/// An extension host process, read from world-readable `/proc` files only.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Host {
    pid: u32,
    start: u64,
    uid: u32,
    gid: u32,
    cmdline: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
enum Found {
    /// The window's extension host and its IPC socket path.
    Window(Host, String),
    /// Root can't see the newest host's fds: rerun as its owner `(uid, gid)`.
    Delegate(u32, u32),
}

fn find(root: bool) -> Option<Found> {
    let sockets = listening_ipc_sockets(&fs::read_to_string("/proc/net/unix").ok()?);
    if sockets.is_empty() {
        return None;
    }
    choose(extension_hosts(), root, |pid| {
        // Root may list the fds but not readlink them: that EACCES must reach
        // `choose`, other errors (an fd closed mid-scan) are skipped.
        for fd in fs::read_dir(format!("/proc/{pid}/fd"))?.flatten() {
            let target = match fs::read_link(fd.path()) {
                Err(e) if e.kind() == io::ErrorKind::PermissionDenied => return Err(e),
                r => r.ok(),
            };
            if let Some(socket) = target.and_then(|t| sockets.get(&socket_inode(t.to_str()?)?).cloned()) {
                return Ok(Some(socket));
            }
        }
        Ok(None)
    })
}

fn extension_hosts() -> Vec<Host> {
    let Ok(dir) = fs::read_dir("/proc") else { return vec![] };
    dir.flatten()
        .filter_map(|entry| {
            let pid = entry.file_name().to_str()?.parse::<u32>().ok()?;
            let cmdline = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
            if !is_extension_host(&cmdline) {
                return None;
            }
            let start = start_time(&fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)?;
            let (uid, gid) = owner_ids(&fs::read_to_string(format!("/proc/{pid}/status")).ok()?)?;
            Some(Host { pid, start, uid, gid, cmdline })
        })
        .collect()
}

/// The newest host whose fds hold one of the IPC sockets (`socket_of`: pid →
/// that socket). As root, a host whose fds are off limits is handed to its
/// owner rather than skipped, so it still wins if it's the newest.
fn choose(mut hosts: Vec<Host>, root: bool, socket_of: impl Fn(u32) -> io::Result<Option<String>>) -> Option<Found> {
    hosts.sort_by_key(|h| std::cmp::Reverse(h.start));
    for h in hosts {
        match socket_of(h.pid) {
            Ok(Some(socket)) => return Some(Found::Window(h, socket)),
            Err(e) if root && e.kind() == io::ErrorKind::PermissionDenied => {
                return Some(Found::Delegate(h.uid, h.gid));
            }
            _ => {}
        }
    }
    None
}

/// Rerun this verb as `uid` with what's left of the wait; it prints and exits
/// as if run directly.
fn delegate(uid: u32, gid: u32, goto: &str, wait: u64) -> io::Result<i32> {
    let exe = std::env::current_exe()?;
    let status = Command::new(&exe)
        .env_clear()
        .env("PATH", PATH)
        .current_dir("/")
        .uid(uid)
        .gid(gid)
        .args(["vscode-goto", goto, "--wait", &wait.to_string()])
        .status()
        .map_err(|e| io::Error::other(format!("cannot rerun {} as uid {uid}: {e}", exe.display())))?;
    Ok(status.code().unwrap_or(1))
}

/// Run the window's server CLI as the extension host's owner: its cmdline
/// (hence the CLI path) is that user's to write, so it never runs as anyone
/// else, and the env is clean so nothing from this exec leaks in.
fn exec_cli(host: &Host, socket: &str, goto: &str, euid: u32) -> io::Result<i32> {
    let bin = server_bin_dir(&host.cmdline)
        .ok_or_else(|| io::Error::other(format!("pid {}: not a VS Code server node", host.pid)))?;
    let cli = remote_cli(&bin.join("bin/remote-cli"))?;
    let passwd = fs::read_to_string("/etc/passwd").unwrap_or_default();
    let home = crate::boot::resolve_user(&passwd, "", &host.uid.to_string()).map_or_else(|| "/".into(), |a| a.home);

    let mut cmd = Command::new(&cli);
    cmd.env_clear()
        .env("PATH", PATH)
        .env("HOME", home)
        .env("VSCODE_IPC_HOOK_CLI", socket)
        // node dies on a cwd its user can't read (e.g. root's /root).
        .current_dir("/")
        .args(["-g", goto]);
    if euid == 0 {
        cmd.uid(host.uid).gid(host.gid);
    } else if euid != host.uid {
        return Err(io::Error::other(format!(
            "the VS Code window runs as uid {}: run as root or as that user",
            host.uid
        )));
    }
    println!("socket={socket} pid={} cli={}", host.pid, cli.display());
    io::stdout().flush()?;
    let status = cmd.status().map_err(|e| io::Error::other(format!("cannot run {}: {e}", cli.display())))?;
    Ok(status.code().unwrap_or(1))
}

/// The single regular file in the server's `bin/remote-cli/` (`code`,
/// `code-insiders`, `cursor`, … depending on the editor variant).
fn remote_cli(dir: &Path) -> io::Result<PathBuf> {
    let mut files = fs::read_dir(dir)
        .map_err(|e| io::Error::other(format!("{}: {e}", dir.display())))?
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| e.path());
    match (files.next(), files.next()) {
        (Some(cli), None) => Ok(cli),
        _ => Err(io::Error::other(format!("{}: expected exactly one CLI", dir.display()))),
    }
}

/// `/proc/net/unix` → listening (`__SO_ACCEPTCON`) sockets bound to
/// `/tmp/vscode-ipc-*.sock`, by inode. Columns: `Num RefCount Protocol Flags
/// Type St Inode [Path]`; unbound and abstract sockets have no `/` path.
fn listening_ipc_sockets(text: &str) -> HashMap<u64, String> {
    const ACCEPTCON: u32 = 0x10000;
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            let path = *f.get(7)?;
            let flags = u32::from_str_radix(f[3], 16).ok()?;
            let name = path.strip_prefix("/tmp/vscode-ipc-")?.strip_suffix(".sock")?;
            (flags & ACCEPTCON != 0 && f.len() == 8 && !name.is_empty() && !name.contains('/'))
                .then(|| Some((f[6].parse().ok()?, path.to_string())))
                .flatten()
        })
        .collect()
}

/// `/proc/<pid>/fd/N`'s link target `socket:[INODE]` → INODE.
fn socket_inode(target: &str) -> Option<u64> {
    target.strip_prefix("socket:[")?.strip_suffix(']')?.parse().ok()
}

fn args(cmdline: &[u8]) -> impl Iterator<Item = &[u8]> {
    cmdline.split(|&b| b == 0)
}

fn is_extension_host(cmdline: &[u8]) -> bool {
    args(cmdline).any(|a| a == b"--type=extensionHost")
}

/// argv0 `<dir>/bin/<commit>/node` → `<dir>/bin/<commit>`, the shape of every
/// server flavor (`.vscode-server`, `.vscode-server-insiders`, `.cursor-server`).
fn server_bin_dir(cmdline: &[u8]) -> Option<PathBuf> {
    let argv0 = Path::new(std::str::from_utf8(args(cmdline).next()?).ok()?);
    let commit = argv0.parent()?;
    let ok = argv0.is_absolute()
        && argv0.file_name()? == "node"
        && commit.file_name().is_some()
        && commit.parent()?.file_name()? == "bin";
    ok.then(|| commit.to_path_buf())
}

/// Start time (field 22, clock ticks since boot) from `/proc/<pid>/stat`;
/// fields are counted from the last `)`, as comm may hold spaces or `)`.
fn start_time(stat: &str) -> Option<u64> {
    stat[stat.rfind(')')? + 1..].split_whitespace().nth(19)?.parse().ok()
}

/// Real `(uid, gid)` from `/proc/<pid>/status`.
fn owner_ids(status: &str) -> Option<(u32, u32)> {
    let id = |key: &str| {
        status.lines().find_map(|l| l.strip_prefix(key)?.split_whitespace().next()?.parse().ok())
    };
    Some((id("Uid:")?, id("Gid:")?))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NET_UNIX: &str = "\
Num       RefCount Protocol Flags    Type St Inode Path
0000000000000000: 00000003 00000000 00000000 0001 03 904025
0000000000000000: 00000002 00000000 00010000 0001 01 120532 /tmp/vscode-ipc-6b85558d-5686-4e4d-a7e4-0d0868de176c.sock
0000000000000000: 00000002 00000000 00010000 0001 01 28561 /tmp/vscode-ipc-8685dc9f-0af6-466b-b007-9e2922941344.sock
0000000000000000: 00000003 00000000 00000000 0001 03 28570 /tmp/vscode-ipc-8685dc9f-0af6-466b-b007-9e2922941344.sock
0000000000000000: 00000002 00000000 00010000 0001 01 28600 /tmp/vscode-git-1a2b3c.sock
0000000000000000: 00000002 00000000 00010000 0001 01 28601 @/tmp/vscode-ipc-abstract.sock
0000000000000000: 00000002 00000000 00010000 0001 01 28602 /run/devsandbox/api.sock
0000000000000000: 00000002 00000000 00010000 0001 01 28603 /tmp/vscode-ipc-a b.sock
";

    #[test]
    fn listening_ipc_sockets_by_inode() {
        let got = listening_ipc_sockets(NET_UNIX);
        let want = HashMap::from([
            (120532, "/tmp/vscode-ipc-6b85558d-5686-4e4d-a7e4-0d0868de176c.sock".to_string()),
            (28561, "/tmp/vscode-ipc-8685dc9f-0af6-466b-b007-9e2922941344.sock".to_string()),
        ]);
        // Not: unbound, accepted (St 03, no ACCEPTCON), other names, abstract,
        // a path with a space (column count off).
        assert_eq!(got, want);
        assert!(listening_ipc_sockets("").is_empty());
    }

    #[test]
    fn socket_inode_from_fd_link() {
        assert_eq!(socket_inode("socket:[6843712]"), Some(6843712));
        assert_eq!(socket_inode("pipe:[6843712]"), None);
        assert_eq!(socket_inode("/dev/null"), None);
        assert_eq!(socket_inode("socket:[x]"), None);
    }

    const EXT_HOST: &[u8] = b"/root/.vscode-server/bin/04c0d99f/node\0--dns-result-order=ipv4first\0/root/.vscode-server/bin/04c0d99f/out/bootstrap-fork\0--type=extensionHost\0--transformURIs\0--useHostProxy=true\0";
    const AGENT_HOST: &[u8] =
        b"/root/.vscode-server/bin/04c0d99f/node\0/root/.vscode-server/bin/04c0d99f/out/bootstrap-fork\0--type=agentHost\0";
    const SERVER_MAIN: &[u8] =
        b"/root/.vscode-server/bin/04c0d99f/node\0/root/.vscode-server/bin/04c0d99f/out/server-main.js\0--start-server\0";

    #[test]
    fn extension_host_is_an_exact_arg() {
        assert!(is_extension_host(EXT_HOST));
        assert!(!is_extension_host(AGENT_HOST));
        assert!(!is_extension_host(SERVER_MAIN));
        // A shell whose script merely mentions it.
        assert!(!is_extension_host(b"sh\0-c\0grep -- --type=extensionHost x\0"));
    }

    #[test]
    fn server_bin_dir_from_argv0() {
        assert_eq!(server_bin_dir(EXT_HOST), Some(PathBuf::from("/root/.vscode-server/bin/04c0d99f")));
        assert_eq!(
            server_bin_dir(b"/home/u/.cursor-server/bin/abc/node\0--type=extensionHost\0"),
            Some(PathBuf::from("/home/u/.cursor-server/bin/abc"))
        );
        assert_eq!(server_bin_dir(b"node\0--type=extensionHost\0"), None);
        assert_eq!(server_bin_dir(b"/usr/bin/node\0"), None);
        assert_eq!(server_bin_dir(b"/x/bin/abc/python\0"), None);
        assert_eq!(server_bin_dir(b""), None);
    }

    #[test]
    fn start_time_is_field_22() {
        let stat = "794 (MainThread) S 456 397 397 0 -1 4194304 17962286 8759205 0 330 26968 27003 6077 21881 20 0 44 0 43321 30462726144 158177 18446744073709551615";
        assert_eq!(start_time(stat), Some(43321));
        assert_eq!(start_time("7 (a) b) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 99 0"), Some(99));
        assert_eq!(start_time("7 (x) S 1 2"), None);
        assert_eq!(start_time(""), None);
    }

    #[test]
    fn owner_ids_are_the_real_ones() {
        let status = "Name:\tnode\nUmask:\t0022\nState:\tS (sleeping)\nUid:\t1000\t0\t0\t0\nGid:\t1001\t0\t0\t0\nGroups:\t\n";
        assert_eq!(owner_ids(status), Some((1000, 1001)));
        assert_eq!(owner_ids("Name:\tx\nUid:\t1\t1\t1\t1\n"), None);
    }

    #[test]
    fn choose_takes_the_newest_host_holding_a_socket() {
        let h = |pid, start, uid| Host { pid, start, uid, gid: uid + 1, cmdline: EXT_HOST.to_vec() };
        let hosts = vec![h(1, 100, 0), h(2, 300, 0), h(3, 900, 0), h(5, 200, 0)];
        // pid 3 is newest but holds no IPC socket (e.g. it just started).
        let socket_of = |pid: u32| Ok((pid != 3).then(|| format!("/tmp/vscode-ipc-{pid}.sock")));
        assert_eq!(
            choose(hosts.clone(), true, socket_of),
            Some(Found::Window(h(2, 300, 0), "/tmp/vscode-ipc-2.sock".into()))
        );
        assert_eq!(choose(vec![], true, socket_of), None);
        assert_eq!(choose(hosts, true, |_| Ok(None)), None);
    }

    #[test]
    fn choose_hands_unreadable_hosts_to_their_owner_only_as_root() {
        let h = |pid, start, uid| Host { pid, start, uid, gid: uid + 1, cmdline: EXT_HOST.to_vec() };
        let hosts = vec![h(1, 100, 0), h(2, 300, 1000)];
        let socket_of = |pid: u32| match pid {
            2 => Err(io::Error::from(io::ErrorKind::PermissionDenied)),
            _ => Ok(Some("/tmp/vscode-ipc-root.sock".to_string())),
        };
        assert_eq!(choose(hosts.clone(), true, socket_of), Some(Found::Delegate(1000, 1001)));
        // Not root: no delegating, the next host it can see wins.
        assert_eq!(
            choose(hosts.clone(), false, socket_of),
            Some(Found::Window(h(1, 100, 0), "/tmp/vscode-ipc-root.sock".into()))
        );
        // A host that vanished mid-scan is just skipped.
        let gone = |pid: u32| if pid == 2 { Err(io::Error::from(io::ErrorKind::NotFound)) } else { socket_of(pid) };
        assert!(matches!(choose(hosts, true, gone), Some(Found::Window(h, _)) if h.pid == 1));
    }

    #[test]
    fn goto_takes_numbers_from_the_right() {
        let g = |s: &str| parse_goto(s);
        assert_eq!(g("/w/a.rs").as_deref(), Some("/w/a.rs"));
        assert_eq!(g("/w/a.rs:4").as_deref(), Some("/w/a.rs:4"));
        assert_eq!(g("/w/a.rs:4:2").as_deref(), Some("/w/a.rs:4:2"));
        assert_eq!(g("/w/a:b.rs:04:2").as_deref(), Some("/w/a:b.rs:4:2"));
        assert_eq!(g("/w/a:b").as_deref(), Some("/w/a:b"));
        assert_eq!(g("/w/12:3:4:5").as_deref(), Some("/w/12:3:4:5"));
        assert_eq!(g("/w/a:").as_deref(), Some("/w/a:"));
        assert_eq!(g("/w/a:x:4").as_deref(), Some("/w/a:x:4"));
        assert_eq!(g("w/a.rs:4"), None, "relative");
        assert_eq!(g("/w/a.rs:0"), None);
        assert_eq!(g("/w/a.rs:4:0"), None);
        assert_eq!(g("/w/a.rs:99999999999"), None);
        assert_eq!(g("/w/a\0b"), None);
        assert_eq!(g(""), None);
    }

    #[test]
    fn args_take_one_goto_and_a_capped_wait() {
        let a = |v: &[&str]| parse_args(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(a(&["/w/a:3"]), Some(("/w/a:3".into(), 0)));
        assert_eq!(a(&["/w/a", "--wait", "5"]), Some(("/w/a".into(), 5)));
        assert_eq!(a(&["--wait", "600", "/w/a"]), Some(("/w/a".into(), 60)));
        assert_eq!(a(&[]), None);
        assert_eq!(a(&["/w/a", "/w/b"]), None);
        assert_eq!(a(&["/w/a", "--wait"]), None);
        assert_eq!(a(&["/w/a", "--wait", "-1"]), None);
        assert_eq!(a(&["--wait", "5"]), None);
    }
}
