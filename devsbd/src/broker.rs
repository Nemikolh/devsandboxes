//! `devsbd daemon`'s credential-broker side (docs/architecture/helper/in-container.md,
//! *Broker listeners*): at start, the broker file's phantom files and its
//! loopback listeners. Each accepted connection becomes an `HTTP_PROXY`
//! stream to the newest bridge whose host serves one, its `ProxyHeader`
//! first; the broker itself runs in the host's `serve`. The proxy ports
//! are `proxy.rs`.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicU32;
use std::sync::Arc;
use std::time::Duration;

use crate::brokerfile::{self, BrokerSpec, Mode, PhantomFile};
use crate::mux::Mux;
use crate::proto::{channel, ProxyHeader};
use crate::proxy::{self, ProxyPort, Upstreams};

/// The hidden verb [`write_phantom`] runs as a phantom file's owner.
pub const WRITE_VERB: &str = "phantom-write";

/// [`write_main`]'s exit code for a file it left alone.
const SKIPPED_EXIT: i32 = 3;

/// Bound on a phantom file's content (a tool's config file).
const MAX_CONTENT: u64 = 1 << 20;

/// How long a refused client gets to read the 503 before the close.
const REFUSE_LINGER: Duration = Duration::from_millis(500);

/// The bridge for one new connection, held as the agent's are; `None`:
/// no host serves `HTTP_PROXY`.
pub type Route = Arc<dyn Fn() -> Option<Arc<Mux>> + Send + Sync>;

const NO_HOST_BODY: &str = "no devsandbox host connected\n";

/// Read the broker file at `path` (absent: nothing to do), write its phantom
/// files, then bind its listeners and proxy ports and serve each on its own thread. Every
/// failure is logged and skips only its own entry: the daemon's other
/// sockets matter more.
pub fn start(path: &Path, route: Route, ids: Arc<AtomicU32>) {
    let spec = match fs::read_to_string(path).map(|t| brokerfile::parse(&t)) {
        Ok(Ok(spec)) => spec,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return,
        Ok(Err(e)) => return eprintln!("devsbd: broker file {}: {e}", path.display()),
        Err(e) => return eprintln!("devsbd: broker file {}: {e}", path.display()),
    };
    let BrokerSpec { listeners, proxy_listeners, files, proxy_env } = spec;
    for file in &files {
        if let Err(e) = write_phantom(file) {
            eprintln!("devsbd: phantom file {}: {e}", file.path);
        }
    }
    for l in listeners {
        let listener = match TcpListener::bind((Ipv4Addr::LOCALHOST, l.port)) {
            Ok(listener) => listener,
            Err(e) => {
                eprintln!("devsbd: broker listener 127.0.0.1:{}: {e}", l.port);
                continue;
            }
        };
        let header = match l.mode {
            Mode::BaseUrl { upstream } => ProxyHeader::BaseUrl { upstream, listener: l.port },
        };
        let (route, ids) = (Arc::clone(&route), Arc::clone(&ids));
        std::thread::spawn(move || serve(listener, &header, &route, &ids));
    }
    let upstreams = Arc::new(Upstreams::from_env(&proxy_env));
    if !proxy_listeners.is_empty() {
        for why in upstreams.problems() {
            eprintln!("devsbd: proxy ports: {why}");
        }
    }
    for l in proxy_listeners {
        let listener = match TcpListener::bind((Ipv4Addr::LOCALHOST, l.port)) {
            Ok(listener) => listener,
            Err(e) => {
                eprintln!("devsbd: proxy port 127.0.0.1:{}: {e}", l.port);
                continue;
            }
        };
        let port = Arc::new(ProxyPort { hosts: l.hosts, upstreams: Arc::clone(&upstreams) });
        let (route, ids) = (Arc::clone(&route), Arc::clone(&ids));
        std::thread::spawn(move || proxy::serve(listener, &port, &route, &ids));
    }
}

/// Accept on `listener` forever, a thread per client: routing may hold it.
pub(crate) fn serve(listener: TcpListener, header: &ProxyHeader, route: &Route, ids: &Arc<AtomicU32>) {
    let header = header.encode();
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let (header, route, ids) = (header.clone(), Arc::clone(route), Arc::clone(ids));
        std::thread::spawn(move || {
            let _ = conn.set_nodelay(true);
            let Some(mux) = route() else { return refuse(conn) };
            let stream = crate::daemon::next_stream_id(&ids);
            let _ = mux.attach_with_header(stream, conn, channel::HTTP_PROXY, &header);
        });
    }
}

/// Answer a client no host can take with a 503 (a valid answer to a request
/// and to a `CONNECT` alike), so the tool reports why instead of a reset.
pub(crate) fn refuse(conn: TcpStream) {
    respond(conn, "503 Service Unavailable", NO_HOST_BODY);
}

/// Answer `status` with a plain-text `body`, then close.
pub(crate) fn respond(mut conn: TcpStream, status: &str, body: &str) {
    let answer = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    if conn.write_all(answer.as_bytes()).is_err() {
        return;
    }
    let _ = conn.shutdown(Shutdown::Write);
    // Unread request bytes at close would make the kernel reset the
    // connection, which can discard the answer before the client reads it.
    let _ = conn.set_read_timeout(Some(REFUSE_LINGER));
    let _ = io::copy(&mut (&conn).take(64 * 1024), &mut io::sink());
}

/// Write `file` by running [`WRITE_VERB`] as its owner, so a path the
/// container's user controls (a link in their home) can't steer a root
/// write: the write can do only what the owner could. A daemon that isn't
/// root (`devsbd boot` as the container command's user) writes only its own
/// user's files.
fn write_phantom(file: &PhantomFile) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let Some((account, groups)) = crate::rpc_runs::lookup(&file.owner) else {
        return Err(io::Error::other(format!("no user `{}` in this container", file.owner)));
    };
    let euid = fs::metadata("/proc/self")?.uid();
    // The running binary itself, even if an install has replaced the file.
    let mut cmd = Command::new("/proc/self/exe");
    cmd.args([WRITE_VERB, &file.path, &file.phantom])
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    if euid == 0 {
        crate::rpc_runs::switch_user(&mut cmd, account.uid, account.gid, groups);
    } else if euid != account.uid {
        return Err(io::Error::other(format!("the daemon runs as uid {euid}, not as `{}`", file.owner)));
    }
    let mut child = cmd.spawn()?;
    let fed = child.stdin.take().map(|mut stdin| stdin.write_all(file.content.as_bytes()));
    let status = child.wait()?;
    fed.transpose()?;
    match status.code() {
        Some(0) => Ok(()),
        Some(SKIPPED_EXIT) => Err(io::Error::other("left alone: something other than its phantom file is there")),
        _ => Err(io::Error::other(format!("writer {status}"))),
    }
}

/// `devsbd phantom-write <path> <phantom>`, content on stdin, run as the
/// file's owner by [`write_phantom`].
pub fn write_main(args: &[String]) -> i32 {
    let [path, phantom] = args else {
        eprintln!("usage: devsbd {WRITE_VERB} <path> <phantom> < content");
        return 2;
    };
    let mut content = Vec::new();
    if let Err(e) = io::stdin().take(MAX_CONTENT + 1).read_to_end(&mut content) {
        eprintln!("devsbd: {path}: reading the content: {e}");
        return 1;
    }
    if content.len() as u64 > MAX_CONTENT {
        eprintln!("devsbd: {path}: content longer than {MAX_CONTENT} bytes");
        return 1;
    }
    match write_file(Path::new(path), phantom.as_bytes(), &content) {
        Ok(true) => 0,
        Ok(false) => SKIPPED_EXIT,
        Err(e) => {
            eprintln!("devsbd: {path}: {e}");
            1
        }
    }
}

/// Whether a file holding `existing` (`None`: no file) may be replaced by
/// a phantom file for `phantom`: only one already holding that phantom, so
/// a file the user or the image put there is never overwritten.
fn replaceable(existing: Option<&[u8]>, phantom: &[u8]) -> bool {
    match existing {
        None => true,
        Some(bytes) => !phantom.is_empty() && bytes.windows(phantom.len()).any(|w| w == phantom),
    }
}

/// Write `content` at `path` atomically (temp file in the same dir, 0600,
/// renamed over), creating missing parent dirs, when [`replaceable`]. A
/// link, dir or other non-file at `path` is left alone. `Ok(false)`: left
/// alone.
fn write_file(path: &Path, phantom: &[u8], content: &[u8]) -> io::Result<bool> {
    let existing = match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
        Ok(meta) if meta.is_file() => Some(fs::read(path)?),
        Ok(_) => return Ok(false),
    };
    if !replaceable(existing.as_deref(), phantom) {
        return Ok(false);
    }
    if existing.as_deref() == Some(content) {
        return Ok(true);
    }
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a file path"));
    };
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{}.devsbd-{}", name.to_string_lossy(), std::process::id()));
    let written = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| f.write_all(content).and_then(|()| f.sync_all()))
        .and_then(|()| fs::rename(&tmp, path));
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("devsbd-broker-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn replaceable_only_when_absent_or_holding_the_phantom() {
        assert!(replaceable(None, b"sntrys_x"));
        assert!(replaceable(Some(b"[auth]\ntoken=sntrys_x\n"), b"sntrys_x"));
        assert!(!replaceable(Some(b"[auth]\ntoken=real\n"), b"sntrys_x"));
        assert!(!replaceable(Some(b""), b"sntrys_x"));
        assert!(!replaceable(Some(b"x"), b""));
    }

    #[test]
    fn write_file_creates_replaces_its_own_and_skips_others() {
        let dir = test_dir("write");
        let path = dir.join("a/b/.sentryclirc");
        assert!(write_file(&path, b"p1", b"token=p1\n").unwrap());
        assert_eq!(fs::read(&path).unwrap(), b"token=p1\n");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        // Its own earlier content: replaced.
        assert!(write_file(&path, b"p1", b"[auth]\ntoken=p1\n").unwrap());
        assert_eq!(fs::read(&path).unwrap(), b"[auth]\ntoken=p1\n");
        // Someone else's file (another phantom, a real token): kept.
        assert!(!write_file(&path, b"p2", b"token=p2\n").unwrap());
        assert_eq!(fs::read(&path).unwrap(), b"[auth]\ntoken=p1\n");
        // A link is never written through nor replaced, even to a phantom file.
        let link = dir.join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(!write_file(&link, b"p1", b"token=p1 via link\n").unwrap());
        assert_eq!(fs::read(&path).unwrap(), b"[auth]\ntoken=p1\n");
        assert!(!write_file(&dir.join("a"), b"p1", b"p1").unwrap(), "a dir is left alone");
        let left: Vec<_> = fs::read_dir(dir.join("a/b")).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(left, [std::ffi::OsString::from(".sentryclirc")]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_client_with_no_route_gets_a_503() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let route: Route = Arc::new(|| None);
        let header = ProxyHeader::BaseUrl { upstream: "sentry.io".into(), listener: port };
        std::thread::spawn(move || serve(listener, &header, &route, &Arc::new(AtomicU32::new(1))));
        let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        client.write_all(b"GET /api/0/ HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").unwrap();
        let mut answer = String::new();
        client.read_to_string(&mut answer).unwrap();
        assert!(answer.starts_with("HTTP/1.1 503 Service Unavailable\r\n"), "{answer}");
        assert!(answer.ends_with(&format!("Content-Length: {}\r\nConnection: close\r\n\r\n{NO_HOST_BODY}", NO_HOST_BODY.len())));
    }
}
