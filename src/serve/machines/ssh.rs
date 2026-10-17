//! The ssh transport to a machine (docs/architecture/daemon/machines.md,
//! *ssh*): the argv the daemon, the CLI and the TUI all build through, and
//! the dial that spawns it.
//!
//! Plain ssh: no ControlMaster of our own, the user's `~/.ssh/config`
//! decides. The remote command goes through the login shell
//! (`$SHELL -lc`), so the remote `PATH` and env are what the user
//! gets in a shell there, not sshd's bare non-interactive env.
//!
//! A [`Duplex`] is always a unix socket: the ssh child's pipes are pumped
//! into one end of a socket pair by threads. So `client::Conn` and
//! `client::hello` work on a machine connection as they do on the local
//! daemon's, read timeouts included, and tests dial a test daemon's socket
//! behind a pump that adds the relay's sentinel (`test_support::relayed`).
//!
//! [`skip_lines`] is how a reader finds our first line past what the login
//! shell printed: the link's sentinel, a remote forward's tunnel header.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{ErrorKind, Read};
use std::net::Shutdown;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use super::Entry;
use crate::serve::endpoint::Stream;

/// How much of ssh's stderr is kept: enough for its last lines
/// (`Permission denied (publickey).`, `sh: 1: devsandbox: not found`).
const STDERR_TAIL: usize = 4096;

/// A parsed `target`: what ssh takes as its destination, plus a port from
/// an `ssh://` URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// `user@host`, `host` or an ssh-config alias.
    pub host: String,
    pub port: Option<u16>,
}

/// `ssh://[user@]host[:port]` becomes `[user@]host` and `-p port`; anything
/// else (`me@host`, an alias) passes through for ssh to interpret. Refused:
/// empty, whitespace or control characters, and a leading `-`, which ssh
/// would take as an option (`-oProxyCommand=…`).
pub fn parse_target(target: &str) -> Result<Target> {
    if target.is_empty() {
        bail!("empty ssh target");
    }
    if target.chars().any(|c| c.is_whitespace() || c.is_control()) {
        bail!("ssh target `{}` has whitespace or control characters", target.escape_debug());
    }
    let Some(rest) = target.strip_prefix("ssh://") else {
        if target.starts_with('-') {
            bail!("ssh target `{target}` starts with `-`");
        }
        return Ok(Target { host: target.into(), port: None });
    };
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    if rest.contains('/') {
        bail!("ssh target `{target}`: a URL takes no path");
    }
    let (user, hostport) = match rest.rsplit_once('@') {
        Some((user, hostport)) if !user.is_empty() => (Some(user), hostport),
        Some(_) => bail!("ssh target `{target}`: empty user"),
        None => (None, rest),
    };
    // `[::1]:22`: brackets only to set a v6 address apart from its port.
    let (host, port) = if let Some(v6) = hostport.strip_prefix('[') {
        let (host, after) = v6.split_once(']').with_context(|| format!("ssh target `{target}`: unclosed `[`"))?;
        match after {
            "" => (host, None),
            _ => (host, Some(after.strip_prefix(':').with_context(|| format!("ssh target `{target}`: junk after `]`"))?)),
        }
    } else {
        match hostport.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (hostport, None),
        }
    };
    if host.is_empty() {
        bail!("ssh target `{target}`: empty host");
    }
    let port = match port {
        None => None,
        Some(p) => match p.parse::<u16>() {
            Ok(n) if n > 0 => Some(n),
            _ => bail!("ssh target `{target}`: bad port `{p}`"),
        },
    };
    let host = match user {
        Some(user) => format!("{user}@{host}"),
        None => host.to_string(),
    };
    if host.starts_with('-') {
        bail!("ssh target `{target}` starts with `-`");
    }
    Ok(Target { host, port })
}

/// `word` as one POSIX shell word: single-quoted, each `'` as `'\''`.
pub fn quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', r"'\''"))
}

/// The string ssh hands the remote user's shell: `exec "$SHELL" -lc
/// '<cmd>'`, `<cmd>` being `exec <command> <args…>`. Plain `$SHELL`, which
/// sshd always sets, not `${SHELL:-…}`: fish and csh don't parse that, and
/// the outer string is parsed by the user's shell, whatever it is. Two shells parse
/// it: the remote's (sshd's `$shell -c`) the outer string, then the login
/// shell `<cmd>`, so each arg is quoted for the inner one and `<cmd>` as a
/// whole for the outer one. `command` is shell text as the user wrote it
/// (`~/bin/devsandbox` must still expand), never quoted.
pub fn remote_command(command: &str, args: &[impl AsRef<str>]) -> String {
    let mut cmd = format!("exec {command}");
    for arg in args {
        cmd.push(' ');
        cmd.push_str(&quote(arg.as_ref()));
    }
    format!("exec \"$SHELL\" -lc {}", quote(&cmd))
}

/// The argv of `entry`'s API connection: `ssh -T -A -o BatchMode=yes [-p N]
/// -- <host> <devsandbox api --stdio --client <client> --session-env>`.
/// `BatchMode`: whoever runs it (the daemon) has no TTY, so ssh fails rather
/// than prompting. `--session-env`: the remote relay swaps each verb's
/// `env` for its own login env.
pub fn api_argv(entry: &Entry, client: &str) -> Result<Vec<OsString>> {
    let args = ["api", "--stdio", "--client", client, "--session-env"];
    argv(entry, &["-T", "-A", "-o", "BatchMode=yes"], &args)
}

/// The argv of an interactive command on `entry` (a terminal tab, a CLI
/// re-exec): `ssh -t -A [-p N] -- <host> <devsandbox remote_args…>`. No
/// `BatchMode`: a person is there to answer ssh's prompts.
pub fn tty_argv(entry: &Entry, remote_args: &[impl AsRef<str>]) -> Result<Vec<OsString>> {
    cli_argv(entry, true, remote_args)
}

/// The argv of a CLI command re-run on `entry`
/// (docs/architecture/daemon/machines.md, *The CLI: `cli.rs`*):
/// [`tty_argv`]'s, but `-T` when this process isn't on a terminal
/// (`devsandbox exec m/x -- cat f > out` must get the bytes, not a pty's
/// CRLFs). No `BatchMode`: a person is there.
pub fn cli_argv(entry: &Entry, tty: bool, remote_args: &[impl AsRef<str>]) -> Result<Vec<OsString>> {
    argv(entry, &[if tty { "-t" } else { "-T" }, "-A"], remote_args)
}

/// [`tty_argv`] running `program` on `entry` instead of its devsandbox: a
/// dashboard service tab's `<runtime> exec -it …`
/// (docs/architecture/daemon/machines.md, *ssh*). `program` is shell text,
/// like `entry`'s `command`.
pub fn tty_argv_with(entry: &Entry, program: &str, remote_args: &[impl AsRef<str>]) -> Result<Vec<OsString>> {
    argv_with(entry, &["-t", "-A"], program, remote_args)
}

/// The argv of a remote forward's bridge (docs/architecture/daemon/forwards.md,
/// *Remote bridges*): `ssh -T [-o BatchMode=yes] [-p N] -- <host> <devsandbox
/// -C <root> tunnel [--instance=<key>] [--service=<svc>] --port <n>>`. No
/// `-A`: a forward's bridge serves no agent. `batch` when whoever runs it has
/// no TTY (the daemon). The `=` forms keep a name starting with `-` a value.
pub fn tunnel_argv(
    entry: &Entry,
    batch: bool,
    root: &Path,
    instance: Option<&str>,
    service: Option<&str>,
    port: u16,
) -> Result<Vec<OsString>> {
    let mut args = vec!["-C".to_string(), root.to_string_lossy().into_owned(), "tunnel".into()];
    args.extend(instance.map(|i| format!("--instance={i}")));
    args.extend(service.map(|s| format!("--service={s}")));
    args.extend(["--port".into(), port.to_string()]);
    let opts: &[&str] = if batch { &["-T", "-o", "BatchMode=yes"] } else { &["-T"] };
    argv(entry, opts, &args)
}

/// The argv of `entry`'s agent relay (docs/architecture/daemon/machines.md,
/// *Agent*): `ssh -T -o BatchMode=yes [-p N] -- <host> <devsandbox
/// agent-relay --client <client>>`. No `-A`: the agent travels inside the
/// relay's own streams.
#[allow(dead_code)] // `service` doesn't run the relay yet
pub fn agent_argv(entry: &Entry, client: &str) -> Result<Vec<OsString>> {
    argv(entry, &["-T", "-o", "BatchMode=yes"], &["agent-relay", "--client", client])
}

fn argv(entry: &Entry, opts: &[&str], remote_args: &[impl AsRef<str>]) -> Result<Vec<OsString>> {
    argv_with(entry, opts, entry.command(), remote_args)
}

fn argv_with(entry: &Entry, opts: &[&str], program: &str, remote_args: &[impl AsRef<str>]) -> Result<Vec<OsString>> {
    let target = parse_target(&entry.target)?;
    let mut argv: Vec<OsString> = ["ssh"].iter().chain(opts).map(OsString::from).collect();
    if let Some(port) = target.port {
        argv.extend(["-p".into(), port.to_string().into()]);
    }
    // `--`: the host can't be read as an option (parse_target refuses a
    // leading `-` too).
    argv.extend(["--".into(), target.host.into(), remote_command(program, remote_args).into()]);
    Ok(argv)
}

/// Opens a connection to a machine's daemon: production spawns ssh
/// ([`ssh_dial`]), tests connect to a test daemon's socket. `agent` is the
/// `SSH_AUTH_SOCK` for `-A`: a verb's requester's, or the daemon's
/// preferred one for its held link; `None` leaves ssh this process's.
pub type Dial = Arc<dyn Fn(&Entry, Option<&Path>) -> Result<Duplex> + Send + Sync>;

/// The production [`Dial`]: [`spawn`] of [`api_argv`], saying `client` to
/// the remote relay (its `serve.log`).
pub fn ssh_dial(client: String) -> Dial {
    Arc::new(move |entry: &Entry, agent: Option<&Path>| spawn(&api_argv(entry, &client)?, agent))
}

/// A connection to a peer daemon: a socket to read and write lines on, and
/// the [`Peer`] behind it (an ssh child, or nothing).
pub struct Duplex {
    pub stream: Stream,
    pub peer: Peer,
}

impl Duplex {
    /// A plain socket to a daemon (tests).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn socket(stream: Stream) -> Self {
        Self { stream, peer: Peer::default() }
    }
}

/// What's behind a [`Duplex`]: the ssh child and its stderr tail. Dropping
/// it kills and reaps the child.
#[derive(Default)]
pub struct Peer {
    child: Option<Child>,
    stderr: Option<Arc<Tail>>,
}

impl Peer {
    /// Kill and reap the ssh child, if any. Idempotent.
    pub fn close(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// The last of ssh's stderr, trimmed; `""` without a child. Waits up to
    /// `wait` for stderr's EOF, so after a connection lost to ssh exiting
    /// the reason (written just before) is in.
    pub fn stderr_tail(&self, wait: Duration) -> String {
        self.stderr.as_ref().map(|t| t.read(wait)).unwrap_or_default()
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.close();
    }
}

/// A bounded ring of a stream's last bytes, filled by a thread.
#[derive(Default)]
pub struct Tail {
    state: Mutex<(VecDeque<u8>, bool)>,
    eof: Condvar,
}

impl Tail {
    /// Keep the tail of `stream` (a child's stderr), read to its end on a
    /// thread named `name`. Fails only when the thread can't start.
    pub fn collect(mut stream: impl Read + Send + 'static, name: &str) -> std::io::Result<Arc<Tail>> {
        let tail = Arc::new(Tail::default());
        let filled = Arc::clone(&tail);
        std::thread::Builder::new().name(name.into()).spawn(move || {
            let mut buf = [0u8; 1024];
            loop {
                match stream.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => filled.push(&buf[..n]),
                    Err(e) if e.kind() == ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
            filled.finish();
        })?;
        Ok(tail)
    }

    fn push(&self, bytes: &[u8]) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.0.extend(bytes);
        let over = state.0.len().saturating_sub(STDERR_TAIL);
        state.0.drain(..over);
    }

    fn finish(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).1 = true;
        self.eof.notify_all();
    }

    /// The tail so far, trimmed, after waiting up to `wait` for the stream's
    /// EOF.
    pub fn read(&self, wait: Duration) -> String {
        let deadline = Instant::now() + wait;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        while !state.1 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            state = self.eof.wait_timeout(state, left).unwrap_or_else(|e| e.into_inner()).0;
        }
        String::from_utf8_lossy(state.0.make_contiguous()).trim().to_string()
    }
}

/// Spawn `argv` (an ssh) as a [`Duplex`]: stdin and stdout pumped to a
/// socket pair, stderr kept in a [`Tail`]. `SSH_AUTH_SOCK` is `agent` when
/// given. The child's stdout EOF reaches the socket as EOF; the socket's
/// EOF (the [`Duplex`] dropped or shut) closes the child's stdin.
pub fn spawn(argv: &[OsString], agent: Option<&Path>) -> Result<Duplex> {
    let (program, args) = argv.split_first().context("empty ssh argv")?;
    let mut cmd = Command::new(program);
    cmd.args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(agent) = agent {
        cmd.env("SSH_AUTH_SOCK", agent);
    }
    let mut child = cmd.spawn().with_context(|| format!("cannot run {}", program.to_string_lossy()))?;
    let peer_of = |child| Peer { child: Some(child), stderr: None };
    let (Some(mut stdin), Some(mut stdout), Some(stderr)) = (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        drop(peer_of(child));
        bail!("ssh's pipes are missing");
    };
    let mut peer = peer_of(child);
    let (ours, theirs) = Stream::pair().context("cannot make a socket pair")?;
    let up = theirs.try_clone().context("cannot clone the socket pair")?;
    // A failed spawn drops `peer`: the child is killed.
    peer.stderr = Some(Tail::collect(stderr, "machine-ssh-stderr").context("cannot start the ssh pumps")?);
    let threads = [
        std::thread::Builder::new().name("machine-ssh-down".into()).spawn(move || {
            pipe(&mut stdout, &mut &theirs);
            let _ = theirs.shutdown(Shutdown::Write);
        }),
        std::thread::Builder::new().name("machine-ssh-up".into()).spawn(move || {
            pipe(&mut &up, &mut stdin);
            // `stdin` drops here: ssh sees EOF, the remote relay a hang-up.
        }),
    ];
    for t in threads {
        // A failed spawn drops `peer`: the child is killed.
        t.context("cannot start the ssh pumps")?;
    }
    Ok(Duplex { stream: ours, peer })
}

/// `from` → `to` until either ends, chunk by chunk as they come (a plain
/// loop: `io::copy`'s Linux fast path (splice) stalled here, the hello
/// never reaching the child).
fn pipe(from: &mut impl Read, to: &mut impl std::io::Write) {
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return,
        };
        if to.write_all(&buf[..n]).and_then(|()| to.flush()).is_err() {
            return;
        }
    }
}

/// How much of the first skipped line [`skip_lines`] keeps, in chars.
const NOISE_CHARS: usize = 200;

/// Why [`skip_lines`] found no line.
#[derive(Debug)]
pub enum SkipError<E> {
    /// The stream ended first.
    Eof,
    /// The bound was reached first.
    TooLong,
    Io(std::io::Error),
    /// The matcher refused a line.
    Refused(E),
}

/// Read `r` up to the first line `matcher` takes (`Ok(Some)`), skipping the
/// ones it doesn't (`Ok(None)`): what a login shell or a motd printed before
/// our own first line. One byte at a time, so nothing past that line's
/// newline is consumed; at most `limit` bytes, skipped lines included. The
/// matcher gets a line without its `\n`. `noise` gets the first non-blank
/// skipped line (or, when nothing was skipped, the partial line the stream
/// or the bound cut off), lossy and cut at [`NOISE_CHARS`], for diagnostics.
pub fn skip_lines<T, E>(
    r: &mut impl Read,
    limit: usize,
    noise: &mut Option<String>,
    mut matcher: impl FnMut(&[u8]) -> Result<Option<T>, E>,
) -> Result<T, SkipError<E>> {
    let mut line = Vec::new();
    let mut total = 0;
    let mut byte = [0u8; 1];
    let keep = |noise: &mut Option<String>, line: &[u8]| {
        let text = String::from_utf8_lossy(line);
        let text = text.trim();
        if noise.is_none() && !text.is_empty() {
            *noise = Some(text.chars().take(NOISE_CHARS).collect());
        }
    };
    loop {
        match r.read(&mut byte) {
            Ok(0) => {
                keep(noise, &line);
                return Err(SkipError::Eof);
            }
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => {
                keep(noise, &line);
                return Err(SkipError::Io(e));
            }
        }
        total += 1;
        if total > limit {
            keep(noise, &line);
            return Err(SkipError::TooLong);
        }
        if byte[0] != b'\n' {
            line.push(byte[0]);
            continue;
        }
        if let Some(found) = matcher(&line).map_err(SkipError::Refused)? {
            return Ok(found);
        }
        keep(noise, &line);
        line.clear();
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// A test daemon's socket as a machine link sees it through the relay:
    /// the relay's first line (`relay::write_sentinel`), then the daemon's
    /// bytes, both ways pumped over a socket pair. Either end closing
    /// reaches the other as EOF.
    pub fn relayed(daemon: Stream) -> Result<Duplex> {
        let (ours, theirs) = Stream::pair()?;
        crate::serve::relay::write_sentinel(&mut &theirs, true)?;
        let (theirs_up, daemon_down) = (theirs.try_clone()?, daemon.try_clone()?);
        std::thread::spawn(move || {
            pipe(&mut &theirs_up, &mut &daemon);
            let _ = daemon.shutdown(Shutdown::Write);
        });
        std::thread::spawn(move || {
            pipe(&mut &daemon_down, &mut &theirs);
            let _ = theirs.shutdown(Shutdown::Write);
        });
        Ok(Duplex::socket(ours))
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::path::PathBuf;

    use super::*;

    fn entry(target: &str, command: Option<&str>) -> Entry {
        Entry { name: "m".into(), target: target.into(), roots: vec![], command: command.map(Into::into), agent: false }
    }

    fn strs(argv: &[OsString]) -> Vec<&str> {
        argv.iter().map(|a| a.to_str().unwrap()).collect()
    }

    #[test]
    fn targets_parse_urls_and_pass_the_rest_through() {
        let t = |host: &str, port| Target { host: host.into(), port };
        assert_eq!(parse_target("me@host").unwrap(), t("me@host", None));
        assert_eq!(parse_target("my-alias").unwrap(), t("my-alias", None));
        assert_eq!(parse_target("ssh://me@host:2222").unwrap(), t("me@host", Some(2222)));
        assert_eq!(parse_target("ssh://host").unwrap(), t("host", None));
        assert_eq!(parse_target("ssh://host/").unwrap(), t("host", None));
        assert_eq!(parse_target("ssh://me@[::1]:22").unwrap(), t("me@::1", Some(22)));
        assert_eq!(parse_target("ssh://[fe80::1]").unwrap(), t("fe80::1", None));
        // ssh's own `user@host:port` isn't a port there; left for ssh as is.
        assert_eq!(parse_target("me@host:22").unwrap(), t("me@host:22", None));
        for bad in ["", " ", "me@ho st", "host\n", "-oProxyCommand=x", "ssh://", "ssh://-o@x", "ssh://host:0", "ssh://host:x",
            "ssh://host:99999", "ssh://@host", "ssh://host/path", "ssh://[::1", "ssh://[::1]x", "ssh://me@:22"]
        {
            assert!(parse_target(bad).is_err(), "{bad:?} parsed");
        }
        let err = parse_target("ssh://-oProxyCommand=x").unwrap_err();
        assert!(format!("{err:#}").contains("starts with `-`"), "{err:#}");
    }

    #[test]
    fn quoting_is_posix_single_quotes() {
        assert_eq!(quote("plain"), "'plain'");
        assert_eq!(quote(""), "''");
        assert_eq!(quote("it's"), r"'it'\''s'");
        assert_eq!(remote_command("devsandbox", &["ps"]), r#"exec "$SHELL" -lc 'exec devsandbox '\''ps'\'''"#);
    }

    #[test]
    fn api_and_tty_argvs() {
        let api = api_argv(&entry("ssh://me@box:2200", None), "machine:laptop").unwrap();
        let remote = remote_command("devsandbox", &["api", "--stdio", "--client", "machine:laptop", "--session-env"]);
        assert_eq!(strs(&api), ["ssh", "-T", "-A", "-o", "BatchMode=yes", "-p", "2200", "--", "me@box", remote.as_str()]);

        let e = entry("box", Some("~/bin/devsandbox"));
        let tty = tty_argv(&e, &["exec", "web", "-C", "/srv/it's here"]).unwrap();
        let remote = remote_command("~/bin/devsandbox", &["exec", "web", "-C", "/srv/it's here"]);
        assert_eq!(strs(&tty), ["ssh", "-t", "-A", "--", "box", remote.as_str()]);
        assert!(remote.contains("~/bin/devsandbox"), "the command is shell text: {remote}");
        let piped = cli_argv(&entry("ssh://box:22", None), false, &["ps"]).unwrap();
        let remote = remote_command("devsandbox", &["ps"]);
        assert_eq!(strs(&piped), ["ssh", "-T", "-A", "-p", "22", "--", "box", remote.as_str()]);
        assert!(api_argv(&entry("-oProxyCommand=x", None), "c").is_err());
    }

    #[test]
    fn tunnel_argvs() {
        let root = Path::new("/srv/it's");
        let daemon = tunnel_argv(&entry("ssh://me@box:2200", None), true, root, Some("web"), Some("-db"), 5432).unwrap();
        let remote =
            remote_command("devsandbox", &["-C", "/srv/it's", "tunnel", "--instance=web", "--service=-db", "--port", "5432"]);
        assert_eq!(strs(&daemon), ["ssh", "-T", "-o", "BatchMode=yes", "-p", "2200", "--", "me@box", remote.as_str()]);
        let cli = tunnel_argv(&entry("box", Some("~/bin/devsandbox")), false, root, None, Some("redis"), 6379).unwrap();
        let remote = remote_command("~/bin/devsandbox", &["-C", "/srv/it's", "tunnel", "--service=redis", "--port", "6379"]);
        assert_eq!(strs(&cli), ["ssh", "-T", "--", "box", remote.as_str()]);
        assert!(!strs(&cli).contains(&"-A"), "a forward serves no agent");
        assert!(tunnel_argv(&entry("-oProxyCommand=x", None), true, root, Some("web"), None, 1).is_err());
    }

    #[test]
    fn agent_argvs() {
        let argv = agent_argv(&entry("ssh://me@box:2200", Some("~/bin/devsandbox")), "machine:lap").unwrap();
        let remote = remote_command("~/bin/devsandbox", &["agent-relay", "--client", "machine:lap"]);
        assert_eq!(strs(&argv), ["ssh", "-T", "-o", "BatchMode=yes", "-p", "2200", "--", "me@box", remote.as_str()]);
        assert!(!strs(&argv).contains(&"-A"), "the relay carries the agent itself");
        assert!(agent_argv(&entry("-oProxyCommand=x", None), "machine:lap").is_err());
    }

    /// A script that prints each arg as `[arg]`, at a path with a space and
    /// a `'` in it.
    fn echo_args(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dsv-ssh-{tag}-{}", std::process::id())).join("it's here");
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("echo-args");
        std::fs::write(&script, "#!/bin/sh\nfor a in \"$@\"; do printf '[%s]\\n' \"$a\"; done\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    /// The remote string through two real shells, as sshd would run it: the
    /// outer `sh -c`, then the login shell's `-lc`.
    #[test]
    fn remote_commands_survive_both_shells() {
        let script = echo_args("quote");
        let home = script.parent().unwrap();
        let args = ["plain", "two words", "it's", r#"say "hi""#, "$HOME", "`id`", "a\\b", "", "-l", "*"];
        // The command is shell text: quote a path that needs it.
        let remote = remote_command(&quote(script.to_str().unwrap()), &args);
        let out = Command::new("/bin/sh")
            .arg("-c")
            .arg(&remote)
            .env_clear()
            .env("SHELL", "/bin/sh")
            .env("HOME", home)
            .env("PATH", "/usr/bin:/bin")
            .current_dir(home)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let want: String = args.iter().map(|a| format!("[{a}]\n")).collect();
        assert_eq!(String::from_utf8_lossy(&out.stdout), want);
        let _ = std::fs::remove_dir_all(home.parent().unwrap());
    }

    #[test]
    fn spawned_pipes_reach_the_socket_and_stderr_is_kept() {
        let argv: Vec<OsString> = ["sh", "-c", "echo \"agent=$SSH_AUTH_SOCK\" >&2; read l; echo \"got $l\"; exec cat"]
            .into_iter()
            .map(Into::into)
            .collect();
        let mut duplex = spawn(&argv, Some(Path::new("/tmp/agent.sock"))).unwrap();
        duplex.stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut reader = BufReader::new(duplex.stream.try_clone().unwrap());
        duplex.stream.write_all(b"one\ntwo\n").unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert_eq!(line, "got one\n");
        line.clear();
        reader.read_line(&mut line).unwrap();
        assert_eq!(line, "two\n");
        // Our EOF is the child's: `cat` exits, stdout's EOF comes back.
        duplex.stream.shutdown(Shutdown::Write).unwrap();
        line.clear();
        assert_eq!(reader.read_line(&mut line).unwrap(), 0);
        assert_eq!(duplex.peer.stderr_tail(Duration::from_secs(5)), "agent=/tmp/agent.sock");
        duplex.peer.close();
        duplex.peer.close();
    }

    #[test]
    fn the_stderr_tail_is_bounded_and_close_kills() {
        let argv: Vec<OsString> =
            ["sh", "-c", "i=0; while [ $i -lt 600 ]; do echo line-$i >&2; i=$((i+1)); done; exec sleep 60"]
                .into_iter()
                .map(Into::into)
                .collect();
        let mut duplex = spawn(&argv, None).unwrap();
        let started = Instant::now();
        // Still running: the wait runs out; what's there by then is kept.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !duplex.peer.stderr_tail(Duration::ZERO).ends_with("line-599") && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let tail = duplex.peer.stderr_tail(Duration::from_millis(50));
        assert!(tail.len() <= STDERR_TAIL && tail.ends_with("line-599") && !tail.contains("line-0\n"), "{tail}");
        duplex.peer.close();
        assert!(started.elapsed() < Duration::from_secs(30), "close didn't kill");
        let mut rest = Vec::new();
        duplex.stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        assert_eq!(duplex.stream.read_to_end(&mut rest).unwrap(), 0, "the socket sees the child's end");
    }

    #[test]
    fn a_missing_program_is_an_error() {
        let err = spawn(&["/nonexistent/ssh".into()], None).err().unwrap();
        assert!(format!("{err:#}").contains("cannot run /nonexistent/ssh"), "{err:#}");
        assert!(spawn(&[], None).is_err());
    }
}
