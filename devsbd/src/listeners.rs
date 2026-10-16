//! Who listens on a TCP port in this container: the daemon's answer to the
//! host's `PortQuery` (docs/architecture/helper/protocol.md), shown as a
//! forward's listening process. Read straight from `/proc`, so images without
//! `lsof` work too: LISTEN sockets on the port in `/proc/net/tcp{,6}` give
//! socket inodes, the `/proc/<pid>/fd` links naming one of them give the
//! pids, `/proc/<pid>/comm` the names.
//!
//! Processes come and go during the scan: anything that fails to read is
//! skipped, never an error. Bounded by `MAX_PIDS` and `proto::MAX_PORT_PROCS`.

use std::fs;
use std::path::Path;

use crate::proto::MAX_PORT_PROCS;

/// Processes scanned at most per lookup, so a container with a huge process
/// table can't turn one query into a long crawl.
const MAX_PIDS: usize = 4096;

/// `st` of a listening socket in `/proc/net/tcp{,6}` (`TCP_LISTEN`).
const TCP_LISTEN: &str = "0A";

/// The `(pid, comm)` listening on TCP `port`, in pid order, one per pid.
pub fn lookup(port: u16) -> Vec<(u32, String)> {
    lookup_in(Path::new("/proc"), port)
}

fn lookup_in(proc: &Path, port: u16) -> Vec<(u32, String)> {
    let mut inodes = Vec::new();
    for table in ["net/tcp", "net/tcp6"] {
        if let Ok(text) = fs::read_to_string(proc.join(table)) {
            inodes.extend(listen_inodes(&text, port));
        }
    }
    if inodes.is_empty() {
        return Vec::new();
    }
    let Ok(entries) = fs::read_dir(proc) else { return Vec::new() };
    let mut pids: Vec<u32> = entries
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
        .take(MAX_PIDS)
        .collect();
    pids.sort_unstable();
    let mut procs = Vec::new();
    for pid in pids {
        if procs.len() == MAX_PORT_PROCS {
            break;
        }
        let dir = proc.join(pid.to_string());
        if !holds_any(&dir, &inodes) {
            continue;
        }
        // The process may have exited since: no comm, nothing to show.
        if let Ok(comm) = fs::read_to_string(dir.join("comm")) {
            procs.push((pid, comm.trim_end_matches('\n').to_string()));
        }
    }
    procs
}

/// Whether one of `/proc/<pid>/fd`'s links is a socket in `inodes`.
fn holds_any(pid_dir: &Path, inodes: &[u64]) -> bool {
    let Ok(fds) = fs::read_dir(pid_dir.join("fd")) else { return false };
    fds.filter_map(|fd| fs::read_link(fd.ok()?.path()).ok())
        .any(|target| target.to_str().and_then(socket_inode).is_some_and(|i| inodes.contains(&i)))
}

/// The socket inodes of LISTEN entries on local `port` in a `/proc/net/tcp`
/// or `tcp6` table. Rows are `sl local rem st tx:rx tr:when retr uid timeout
/// inode …`, `local` being `<hex addr>:<hex port>`; the header and anything
/// malformed are skipped. Any local address counts (like `lsof -iTCP:<port>`).
fn listen_inodes(table: &str, port: u16) -> Vec<u64> {
    table
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let (local, st, inode) = (fields.get(1)?, fields.get(3)?, fields.get(9)?);
            let (_, hex_port) = local.rsplit_once(':')?;
            let listening = *st == TCP_LISTEN && u16::from_str_radix(hex_port, 16).ok()? == port;
            // Inode 0: a socket being torn down, owned by no fd.
            listening.then(|| inode.parse().ok()).flatten().filter(|&i| i != 0)
        })
        .collect()
}

/// The inode in an fd link target `socket:[<inode>]`; `None` for anything
/// else (files, pipes, `anon_inode:…`).
fn socket_inode(target: &str) -> Option<u64> {
    target.strip_prefix("socket:[")?.strip_suffix(']')?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TCP: &str = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 4242 1 0000000000000000 100 0 0 10 0
   1: 00000000:0BB8 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 77 1 0000000000000000 100 0 0 10 0
   2: 0100007F:1F90 0100007F:D2E4 01 00000000:00000000 00:00000000 00000000     0        0 4300 1 0000000000000000 20 4 30 10 -1
   3: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 0 1 0000000000000000 100 0 0 10 0
";

    const TCP6: &str = "\
  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000000000000000000001000000:1F90 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 9001 1 0000000000000000 100 0 0 10 0
";

    #[test]
    fn listen_inodes_table() {
        // 8080 = 0x1F90: the LISTEN row only; the established one (st 01) and
        // the inode-0 row are not listeners we can attribute.
        assert_eq!(listen_inodes(TCP, 8080), [4242]);
        assert_eq!(listen_inodes(TCP, 3000), [77]);
        assert_eq!(listen_inodes(TCP, 9), Vec::<u64>::new());
        assert_eq!(listen_inodes(TCP6, 8080), [9001]);
        // Header only, empty, garbage: nothing, no panic.
        for text in ["", "  sl  local_address rem_address st\n", "0: zz:zz 0 0A\n", "0: 1:FFFFF 0 0A a b c d e 1\n"] {
            assert_eq!(listen_inodes(text, 8080), Vec::<u64>::new(), "{text:?}");
        }
    }

    #[test]
    fn socket_inode_table() {
        assert_eq!(socket_inode("socket:[4242]"), Some(4242));
        for target in ["pipe:[4242]", "anon_inode:[eventfd]", "/dev/null", "socket:[]", "socket:[12", "socket:[x]"] {
            assert_eq!(socket_inode(target), None, "{target}");
        }
    }

    /// Against this machine's real `/proc`: a listener this test process holds
    /// is found under its own pid; a port nobody listens on finds nothing.
    #[cfg(target_os = "linux")]
    #[test]
    fn lookup_finds_this_process() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let found = lookup(port);
        let me = std::process::id();
        assert!(found.iter().any(|(pid, comm)| *pid == me && !comm.is_empty()), "{found:?}");
        drop(listener);
        assert!(lookup(port).iter().all(|(pid, _)| *pid != me));
    }
}
