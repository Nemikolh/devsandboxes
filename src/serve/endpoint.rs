//! `local_endpoint`: where the daemon listens and how clients reach it.
//!
//! Unix socket only for now. Everything above this module goes through
//! [`Listener`], [`connect`] and [`Stream`] (Read + Write + `try_clone` +
//! `shutdown` + read timeouts), so a Windows named pipe
//! (`\\.\pipe\devsandbox-<user>`) can slot in when bridging is ported.
//!
//! Access control is the file permissions: the dir is 0700 and the socket
//! 0600. Whoever can connect can exec into every container, like
//! `docker.sock`.

use std::ffi::OsStr;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

/// A connection in either direction. A named-pipe build would provide a type
/// with the same surface.
pub type Stream = std::os::unix::net::UnixStream;

pub const SOCKET_FILE: &str = "serve.sock";
pub const LOCK_FILE: &str = "serve.lock";
pub const LOG_FILE: &str = "serve.log";

/// The socket dir from the environment's values: `$XDG_RUNTIME_DIR/devsandbox`,
/// else the state dir ([`state_dir_from`]). The path stays short on purpose:
/// macOS caps socket paths at 104 bytes, so no config-root hashes in it (one
/// daemon per user; config roots are a field in requests).
pub fn socket_dir_from(runtime: Option<&OsStr>, state: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    xdg_base(runtime).map(|r| r.join("devsandbox")).or_else(|| state_dir_from(state, home))
}

/// `$XDG_STATE_HOME/devsandbox`, else `~/.local/state/devsandbox`; holds
/// `serve.log`. Empty and relative XDG values are ignored, as the XDG spec says.
pub fn state_dir_from(state: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    let base = xdg_base(state).or_else(|| xdg_base(home).map(|h| h.join(".local/state")))?;
    Some(base.join("devsandbox"))
}

fn xdg_base(value: Option<&OsStr>) -> Option<PathBuf> {
    let path = PathBuf::from(value?);
    path.is_absolute().then_some(path)
}

/// [`socket_dir_from`] over the real environment.
pub fn socket_dir() -> Result<PathBuf> {
    let var = std::env::var_os;
    socket_dir_from(var("XDG_RUNTIME_DIR").as_deref(), var("XDG_STATE_HOME").as_deref(), var("HOME").as_deref())
        .context("cannot determine the devsandbox serve socket dir ($XDG_RUNTIME_DIR, $XDG_STATE_HOME or $HOME)")
}

/// Where the detached daemon's stdout/stderr go: `<state dir>/serve.log`.
pub fn log_path() -> Result<PathBuf> {
    let var = std::env::var_os;
    let dir = state_dir_from(var("XDG_STATE_HOME").as_deref(), var("HOME").as_deref())
        .context("cannot determine the devsandbox state dir ($XDG_STATE_HOME or $HOME)")?;
    Ok(dir.join(LOG_FILE))
}

pub fn socket_path(dir: &Path) -> PathBuf {
    dir.join(SOCKET_FILE)
}

pub fn lock_path(dir: &Path) -> PathBuf {
    dir.join(LOCK_FILE)
}

/// How many bytes of a client name a link socket's name keeps before
/// cutting it and appending a hash of the whole name.
const AGENT_CLIENT_MAX: usize = 40;

/// A relayed agent's socket in `dir`: `agent-<client>.sock`, a lending
/// worker's for its hub (`fleet::agent::socket`, client `fleet-<hub>`).
/// The client is cut down to `[A-Za-z0-9._-]` (the rest become `_`) and to
/// [`AGENT_CLIENT_MAX`] bytes, a long one ending in a hash of the full name
/// so two long names don't collide: macOS caps socket paths at 104 bytes.
pub fn agent_socket(dir: &Path, client: &str) -> PathBuf {
    dir.join(format!("agent-{}.sock", agent_socket_name(client)))
}

/// An offered fleet worker's socket in `dir` (docs/architecture/daemon/fleet.md,
/// *Hub side*): `fleet-<worker>.sock`, bound by its `fleet-link` and dialed
/// by this host's link to the worker. The name is cut like
/// [`agent_socket`]'s client (a valid machine name needs only the length
/// cut).
pub fn fleet_socket(dir: &Path, worker: &str) -> PathBuf {
    dir.join(format!("fleet-{}.sock", agent_socket_name(worker)))
}

/// An offered fleet worker's transfer socket in `dir` (docs/architecture/daemon/fleet.md,
/// *Transfer*): `fleet-<worker>.transfer.sock`, bound by its `fleet-link`
/// next to [`fleet_socket`]; each connection there is one `TRANSFER`
/// stream to the worker.
pub fn fleet_transfer_socket(dir: &Path, worker: &str) -> PathBuf {
    fleet_channel_socket(dir, worker, "transfer")
}

/// An offered fleet worker's terminal socket in `dir`
/// (docs/architecture/daemon/fleet.md, *PTY*): `fleet-<worker>.pty.sock`,
/// bound by its `fleet-link` next to [`fleet_socket`]; each connection there
/// is one `PTY` stream to the worker (`fleet::pty_client` dials it).
pub fn fleet_pty_socket(dir: &Path, worker: &str) -> PathBuf {
    fleet_channel_socket(dir, worker, "pty")
}

/// An offered fleet worker's port forward socket in `dir`
/// (docs/architecture/daemon/fleet.md, *TUNNEL*): `fleet-<worker>.tunnel.sock`,
/// bound by its `fleet-link` next to [`fleet_socket`]; each connection there
/// is one `TUNNEL` stream to the worker, one forward's bridge
/// (`commands::tunnel::remote_opener` dials it).
pub fn fleet_tunnel_socket(dir: &Path, worker: &str) -> PathBuf {
    fleet_channel_socket(dir, worker, "tunnel")
}

/// The agent socket of a `fleet-link --peer <dialer>` in `dir`
/// (docs/architecture/daemon/fleet.md, *Peer links*):
/// `agent-link-<dialer>.sock`, bound while the dialer serves `AGENT`
/// (`fleet::agent::serve`). Named `agent-*.sock`, it is one of
/// [`agent_sockets`], so this host's bridges pick it up as a relayed agent.
pub fn link_agent_socket(dir: &Path, dialer: &str) -> PathBuf {
    dir.join(format!("agent-link-{}.sock", agent_socket_name(dialer)))
}

/// An offered fleet worker's socket in `dir` for one more channel its
/// `fleet-link` opens toward it: `fleet-<worker>.<suffix>.sock`, next to
/// [`fleet_socket`]; each connection there is one stream on that channel
/// (`fleet::link::opener_with`).
pub fn fleet_channel_socket(dir: &Path, worker: &str, suffix: &str) -> PathBuf {
    dir.join(format!("fleet-{}.{suffix}.sock", agent_socket_name(worker)))
}

fn agent_socket_name(client: &str) -> String {
    let safe: String = client
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' })
        .collect();
    if safe.len() <= AGENT_CLIENT_MAX {
        return safe;
    }
    let hash = crate::config::short_hash(client);
    // ASCII only by now, so any byte index is a char boundary.
    format!("{}-{}", &safe[..AGENT_CLIENT_MAX - 9], &hash[..8])
}

/// The relayed agents' sockets (`agent-*.sock`: [`link_agent_socket`],
/// [`agent_socket`]) in `dir`, newest mtime first,
/// unchecked: whoever uses one tries it (`bridge::host_agent`). Nothing
/// when `dir` is missing or unreadable.
pub fn agent_sockets(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.starts_with("agent-") && name.ends_with(".sock")
        })
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    found.into_iter().map(|(_, p)| p).collect()
}

/// Create `dir` if needed and make it 0700 (also when it already existed:
/// it's ours, and a looser mode would open the socket to other users).
pub fn ensure_private_dir(dir: &Path) -> Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("cannot create {}", dir.display()))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("cannot chmod {}", dir.display()))
}

pub struct Listener {
    inner: UnixListener,
    path: PathBuf,
}

impl Listener {
    /// Bind `dir/serve.sock` (0600). The caller holds the start lock, so any
    /// socket file already there is a dead daemon's and is replaced.
    pub fn bind(dir: &Path) -> Result<Self> {
        let path = socket_path(dir);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("cannot remove stale {}", path.display())),
        }
        let inner = UnixListener::bind(&path).with_context(|| format!("cannot bind {}", path.display()))?;
        // The 0700 dir already keeps others out between bind and chmod.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("cannot chmod {}", path.display()))?;
        // Non-blocking so a connection that vanishes between poll and accept
        // can't park the accept loop.
        inner.set_nonblocking(true).context("cannot make the listener non-blocking")?;
        Ok(Self { inner, path })
    }

    /// Wait up to `timeout` for a connection: `Ok(None)` when none came, so
    /// the accept loop can check its idle/drain state between connections
    /// without a busy loop or a listener thread.
    pub fn accept_timeout(&self, timeout: Duration) -> io::Result<Option<Stream>> {
        accept_timeout(&self.inner, timeout)
    }

    /// Remove the socket file. Only the lock holder calls this, right before
    /// exiting, so it never removes a successor's socket.
    pub fn remove(self) {
        drop(self.inner);
        let _ = std::fs::remove_file(&self.path);
    }
}

/// [`Listener::accept_timeout`] on any non-blocking `listener` (an agent
/// relay's too): wait up to `timeout`, `Ok(None)` when no connection came.
pub fn accept_timeout(listener: &UnixListener, timeout: Duration) -> io::Result<Option<Stream>> {
    Ok(accept_any(&[listener], timeout)?.map(|(_, stream)| stream))
}

/// [`accept_timeout`] over several non-blocking listeners at once: a
/// connection on the first ready one, with its index in `listeners`.
pub fn accept_any(listeners: &[&UnixListener], timeout: Duration) -> io::Result<Option<(usize, Stream)>> {
    let mut fds: Vec<libc::pollfd> =
        listeners.iter().map(|l| libc::pollfd { fd: l.as_raw_fd(), events: libc::POLLIN, revents: 0 }).collect();
    let ms = timeout.as_millis().min(i32::MAX as u128) as libc::c_int;
    // SAFETY: `fds` is valid for its length for the duration of the call.
    let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, ms) };
    if n < 0 {
        let e = io::Error::last_os_error();
        return if e.kind() == io::ErrorKind::Interrupted { Ok(None) } else { Err(e) };
    }
    let Some(i) = fds.iter().position(|fd| fd.revents != 0) else { return Ok(None) };
    match listeners[i].accept().map(|(stream, _)| (i, stream)) {
        Ok((i, stream)) => {
            // macOS hands out accepted sockets with the listener's O_NONBLOCK.
            stream.set_nonblocking(false)?;
            Ok(Some((i, stream)))
        }
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(e),
    }
}

/// Connect to the daemon in `dir`. Fails (not found / refused) when no daemon
/// is listening; the caller decides whether to start one.
pub fn connect(dir: &Path) -> io::Result<Stream> {
    Stream::connect(socket_path(dir))
}

/// Whether the process at the other end of `stream` runs as this process's
/// uid (`SO_PEERCRED` on Linux, `getpeereid` elsewhere); `false` when it
/// can't be told.
pub fn peer_is_own_user(stream: &Stream) -> bool {
    // SAFETY: getuid has no preconditions and cannot fail.
    let ours = unsafe { libc::getuid() };
    peer_uid(stream) == Some(ours)
}

#[cfg(target_os = "linux")]
fn peer_uid(stream: &Stream) -> Option<u32> {
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` and `len` are valid for writes of the sizes given; the
    // fd is open for `stream`'s life.
    let r = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    (r == 0).then_some(cred.uid)
}

#[cfg(not(target_os = "linux"))]
fn peer_uid(stream: &Stream) -> Option<u32> {
    let (mut uid, mut gid) = (0, 0);
    // SAFETY: both out-params are valid; the fd is open for `stream`'s life.
    let r = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    (r == 0).then_some(uid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(s: &str) -> Option<&OsStr> {
        Some(OsStr::new(s))
    }

    #[test]
    fn a_pair_in_this_process_is_its_own_user() {
        let (a, b) = Stream::pair().unwrap();
        assert!(peer_is_own_user(&a) && peer_is_own_user(&b));
    }

    #[test]
    fn socket_dir_prefers_runtime_then_state_then_home() {
        let p = |r, s, h| socket_dir_from(r, s, h).map(|p| p.display().to_string());
        assert_eq!(p(os("/run/user/1000"), os("/st"), os("/home/u")).as_deref(), Some("/run/user/1000/devsandbox"));
        assert_eq!(p(None, os("/st"), os("/home/u")).as_deref(), Some("/st/devsandbox"));
        assert_eq!(p(None, None, os("/home/u")).as_deref(), Some("/home/u/.local/state/devsandbox"));
        assert_eq!(p(None, None, None), None);
    }

    #[test]
    fn empty_or_relative_xdg_values_are_ignored() {
        let p = |r, s, h| socket_dir_from(r, s, h).map(|p| p.display().to_string());
        assert_eq!(p(os(""), os("rel"), os("/home/u")).as_deref(), Some("/home/u/.local/state/devsandbox"));
        assert_eq!(p(os("rel"), None, os("")), None);
    }

    #[test]
    fn state_dir_falls_back_to_home() {
        let p = |s, h| state_dir_from(s, h).map(|p| p.display().to_string());
        assert_eq!(p(os("/st"), os("/home/u")).as_deref(), Some("/st/devsandbox"));
        assert_eq!(p(None, os("/home/u")).as_deref(), Some("/home/u/.local/state/devsandbox"));
        assert_eq!(p(None, None), None);
    }

    #[test]
    fn bind_makes_dir_0700_and_socket_0600() {
        let root = std::env::temp_dir().join(format!("devsandbox-serve-perm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("devsandbox");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        // A stale socket file from a dead daemon is replaced.
        std::fs::write(socket_path(&dir), "").unwrap();

        ensure_private_dir(&dir).unwrap();
        let listener = Listener::bind(&dir).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&socket_path(&dir)), 0o600);

        assert!(listener.accept_timeout(Duration::from_millis(10)).unwrap().is_none());
        let _client = connect(&dir).unwrap();
        assert!(listener.accept_timeout(Duration::from_secs(5)).unwrap().is_some());

        listener.remove();
        assert!(!socket_path(&dir).exists());
        assert!(connect(&dir).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn agent_socket_names_are_sanitized_and_bounded() {
        let dir = Path::new("/run/user/1000/devsandbox");
        assert_eq!(agent_socket(dir, "machine:lap-top.local"), dir.join("agent-machine_lap-top.local.sock"));
        assert_eq!(agent_socket(dir, "machine:a b/c\u{e9}"), dir.join("agent-machine_a_b_c_.sock"));
        let long = |host: &str| agent_socket(dir, &format!("machine:{}{host}", "h".repeat(60)));
        let (a, b) = (long("a"), long("b"));
        assert_ne!(a, b, "two long names that share a prefix don't collide");
        assert_eq!(a, long("a"), "stable");
        let name = a.file_name().unwrap().to_str().unwrap();
        assert_eq!(name.len(), "agent-".len() + AGENT_CLIENT_MAX + ".sock".len());
        assert!(name.starts_with("agent-machine_hhh"), "{name}");
    }

    #[test]
    fn fleet_sockets_are_named_after_the_worker() {
        let dir = Path::new("/run/user/1000/devsandbox");
        assert_eq!(fleet_socket(dir, "lap.top"), dir.join("fleet-lap.top.sock"));
        assert_eq!(fleet_transfer_socket(dir, "lap.top"), dir.join("fleet-lap.top.transfer.sock"));
        assert_eq!(fleet_pty_socket(dir, "lap.top"), dir.join("fleet-lap.top.pty.sock"));
        assert_eq!(fleet_tunnel_socket(dir, "lap.top"), dir.join("fleet-lap.top.tunnel.sock"));
        let long = fleet_socket(dir, &"w".repeat(80));
        assert_eq!(long.file_name().unwrap().len(), "fleet-".len() + AGENT_CLIENT_MAX + ".sock".len());
    }

    #[test]
    fn agent_sockets_are_newest_first() {
        let dir = std::env::temp_dir().join(format!("dsv-as-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let now = std::time::SystemTime::now();
        for (name, age) in [("agent-old.sock", 20), ("agent-new.sock", 0), ("agent-mid.sock", 10), ("serve.sock", 0), ("agent-x.tmp", 0)] {
            let f = std::fs::File::create(dir.join(name)).unwrap();
            f.set_modified(now - Duration::from_secs(age)).unwrap();
        }
        let names: Vec<_> = agent_sockets(&dir).iter().map(|p| p.file_name().unwrap().to_owned()).collect();
        assert_eq!(names, ["agent-new.sock", "agent-mid.sock", "agent-old.sock"]);
        assert!(agent_sockets(&dir.join("missing")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
