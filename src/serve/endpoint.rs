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
        let mut fd = libc::pollfd { fd: self.inner.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        let ms = timeout.as_millis().min(i32::MAX as u128) as libc::c_int;
        // SAFETY: one valid pollfd for the duration of the call.
        let n = unsafe { libc::poll(&mut fd, 1, ms) };
        if n < 0 {
            let e = io::Error::last_os_error();
            return if e.kind() == io::ErrorKind::Interrupted { Ok(None) } else { Err(e) };
        }
        if n == 0 {
            return Ok(None);
        }
        match self.inner.accept() {
            Ok((stream, _)) => {
                // macOS hands out accepted sockets with the listener's O_NONBLOCK.
                stream.set_nonblocking(false)?;
                Ok(Some(stream))
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Remove the socket file. Only the lock holder calls this, right before
    /// exiting, so it never removes a successor's socket.
    pub fn remove(self) {
        drop(self.inner);
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Connect to the daemon in `dir`. Fails (not found / refused) when no daemon
/// is listening; the caller decides whether to start one.
pub fn connect(dir: &Path) -> io::Result<Stream> {
    Stream::connect(socket_path(dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(s: &str) -> Option<&OsStr> {
        Some(OsStr::new(s))
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
}
