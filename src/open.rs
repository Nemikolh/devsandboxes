//! Opening a container's URL on this host (docs/architecture/helper/bridges.md,
//! "Opening URLs (`kind open`)"): the policy an `open` record passes before
//! the bridge's sink hands it to the opener, and the system opener itself.
//! The policy is pure (clocks and the limiter passed in), and so are the
//! two halves' choices: on the instance's host, the profile name an open
//! carries ([`profile_to_send`], [`local_job`]); on the opening host, which
//! may be another fleet member (docs/architecture/daemon/fleet.md, *Opening
//! URLs*), the command that name maps to in its own `profiles.toml`
//! ([`open_argv`]). Only [`prepare`] (reads `state.toml`) and [`open_here`]
//! (the display, `profiles.toml`, the spawn) touch the environment.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::state::{Instance, State};

/// An `open` record queued longer ago than this is dropped at delivery: the
/// outbox keeps records while no host is attached, and an OAuth URL opening
/// an hour later is worse than none. Measured from the record's `at`.
pub const OPEN_TTL: Duration = Duration::from_secs(30);

/// URLs one instance may open back to back before [`OPEN_RATE_REFILL`]
/// applies, so a looping script can't flood the desktop with tabs.
pub const OPEN_RATE_BURST: u32 = 5;

/// One more open per instance is allowed every this long.
pub const OPEN_RATE_REFILL: Duration = Duration::from_secs(10);

/// How long after its attach to an instance on another host ended a member
/// still takes that instance's opens (`serve::attaches`): a user who just
/// closed the terminal may still be finishing the login it started.
pub const OPEN_ATTACH_WINDOW: Duration = Duration::from_secs(10 * 60);

/// What happens to one `open` record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Open,
    /// Not `http`/`https`; the scheme as given (empty when there is none).
    BadScheme(String),
    /// Queued `age` seconds ago, past [`OPEN_TTL`].
    Expired { age: u64 },
    /// The instance's rate limit is spent.
    Limited,
}

impl Verdict {
    /// Why a record isn't opened (the [`Outcome::Refused`] text); `None`
    /// for [`Verdict::Open`].
    pub fn refusal(&self) -> Option<String> {
        Some(match self {
            Verdict::Open => return None,
            Verdict::BadScheme(s) if s.is_empty() => "not a URL with a scheme".to_string(),
            Verdict::BadScheme(s) => format!("scheme `{s}` (http and https only)"),
            Verdict::Expired { age } => format!("queued {age}s ago (over {}s)", OPEN_TTL.as_secs()),
            Verdict::Limited => format!("rate limit ({OPEN_RATE_BURST} per {}s)", OPEN_RATE_REFILL.as_secs()),
        })
    }

    /// The `serve.log` line for a refusal or drop, naming `instance` and
    /// `url`; `None` for [`Verdict::Open`].
    pub fn log_line(&self, instance: &str, url: &str) -> Option<String> {
        let why = self.refusal()?;
        let verb = if matches!(self, Verdict::Expired { .. }) { "dropped" } else { "refused" };
        Some(format!("{instance}: open {verb}, {why}: {url}"))
    }
}

/// What became of one open, for the user (`opens.done`,
/// docs/reference/api.md): each open ends in exactly one, reported by the
/// host that knows: the instance's host for a refusal at intake or a failed
/// delivery, the opening host for the rest ([`open_here`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The opener was started. `unforwarded`: loopback ports the URL needed
    /// that couldn't be forwarded (an OAuth callback port taken, say); the
    /// URL opened anyway, so this is still one `opened` line, naming them.
    Opened { unforwarded: Vec<Loopback> },
    /// The policy said no ([`Verdict::refusal`]).
    Refused(String),
    /// Nothing could open it: no display, no opener, a failed spawn, a
    /// delivery the member didn't take.
    Failed(String),
}

/// One open's [`Outcome`], with what it was about: `instance` as the line
/// names it (`machine/name` for an instance on another member), `url` the one
/// opened (rewritten to a forward's port) or, when nothing opened, the one
/// given, `profile` the name it carried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenDone {
    pub instance: String,
    pub url: String,
    pub profile: Option<String>,
    pub outcome: Outcome,
}

/// The longest URL a [`OpenDone::line`] shows whole: a status line is one
/// row, and an OAuth URL easily runs to a kilobyte.
const LINE_URL: usize = 120;

/// The longest reason a [`OpenDone::line`] shows whole (a member's refusal
/// may quote the URL).
const LINE_WHY: usize = 160;

/// `s` cut to `max` chars, ending in `…` when cut.
fn shorten(s: &str, max: usize) -> String {
    match s.char_indices().nth(max.saturating_sub(1)) {
        Some((at, _)) if s.chars().count() > max => format!("{}…", &s[..at]),
        _ => s.to_string(),
    }
}

impl OpenDone {
    /// The wire's `outcome`: `opened`, `refused` or `failed`.
    pub fn kind(&self) -> &'static str {
        match self.outcome {
            Outcome::Opened { .. } => "opened",
            Outcome::Refused(_) => "refused",
            Outcome::Failed(_) => "failed",
        }
    }

    /// The wire's `why`: the refusal or failure, or for an open the ports
    /// it couldn't forward (`None` when there are none).
    pub fn why(&self) -> Option<String> {
        match &self.outcome {
            Outcome::Opened { unforwarded } if unforwarded.is_empty() => None,
            Outcome::Opened { unforwarded } => {
                let ports: Vec<String> = unforwarded
                    .iter()
                    .map(|l| format!("{}port {}", if l.callback { "callback " } else { "" }, l.port))
                    .collect();
                Some(format!("cannot forward {}", ports.join(", ")))
            }
            Outcome::Refused(why) | Outcome::Failed(why) => Some(why.clone()),
        }
    }

    /// The status line: `web: opened https://… (work)`, with `; cannot
    /// forward callback port 8085` when a port didn't forward; `web: open
    /// refused: <why>`; `web: open failed: <why>`. The URL and the reason are
    /// [`shorten`]ed.
    pub fn line(&self) -> String {
        let why = self.why().map(|w| shorten(&w, LINE_WHY));
        match (&self.outcome, why) {
            (Outcome::Opened { .. }, why) => {
                let profile = self.profile.as_ref().map_or(String::new(), |p| format!(" ({p})"));
                let note = why.map_or(String::new(), |w| format!("; {w}"));
                format!("{}: opened {}{profile}{note}", self.instance, shorten(&self.url, LINE_URL))
            }
            (Outcome::Refused(_), why) => format!("{}: open refused: {}", self.instance, why.unwrap_or_default()),
            (Outcome::Failed(_), why) => format!("{}: open failed: {}", self.instance, why.unwrap_or_default()),
        }
    }
}

/// Gets every [`OpenDone`] this host reports. The daemon's pushes
/// `opens.done` (`serve::opens`); a seam, so this module stays free of
/// `serve` (unix-only) and tests record them.
pub type OnDone = Arc<dyn Fn(OpenDone) + Send + Sync>;

/// Decide whether `url`, queued at `at` (unix seconds), opens at `now` (unix
/// seconds). Checked in order: the scheme (`http`/`https`, any case,
/// followed by `://`), then [`OPEN_TTL`], then `limit`, which is only asked
/// (and only spends a token) once the others pass. A record from the future
/// (clock skew between container and host) counts as fresh.
pub fn decide(url: &str, at: u64, now: u64, limit: impl FnOnce() -> bool) -> Verdict {
    let scheme = match url.split_once(':') {
        Some((s, _)) if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c)) => s,
        _ => "",
    };
    let web = scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https");
    if !web || !url[scheme.len()..].starts_with("://") {
        return Verdict::BadScheme(scheme.to_string());
    }
    let age = now.saturating_sub(at);
    if age > OPEN_TTL.as_secs() {
        return Verdict::Expired { age };
    }
    if !limit() {
        return Verdict::Limited;
    }
    Verdict::Open
}

/// The system opener's argv for `url` on `os` (`std::env::consts::OS`
/// values), `None` where there is no known one. `url` is the last entry;
/// [`decide`] already held it to `http(s)://`, so it can't read as an option.
pub fn opener_argv(os: &str, url: &str) -> Option<Vec<String>> {
    match os {
        "linux" => Some(vec!["xdg-open".into(), url.into()]),
        "macos" => Some(vec!["open".into(), url.into()]),
        _ => None,
    }
}

/// What one profile does on this host: a `[<profile>]` table of
/// `profiles.toml` (docs/architecture/instances/profiles.md, *On the opening
/// host*).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostProfile {
    /// The command that opens a URL in this profile (a browser and its
    /// profile flag); the URL is appended as the last argument.
    pub open: Vec<String>,
}

/// `profiles.toml`, by profile name. Commands only ever come from this
/// host's own file: only a profile's *name* comes from `state.toml` (and,
/// later, over a fleet link), so no config or machine can make this host run
/// a command.
pub type HostProfiles = BTreeMap<String, HostProfile>;

/// `profiles.toml`, next to `state.toml`.
pub fn profiles_path() -> Result<PathBuf> {
    Ok(State::path()?.with_file_name("profiles.toml"))
}

/// Parse `profiles.toml`'s contents: `[<profile>]` tables with a non-empty
/// `open` array of strings, nothing else. An empty file is no profiles.
pub fn parse_profiles(contents: &str) -> Result<HostProfiles> {
    let profiles: HostProfiles = toml::from_str(contents)?;
    if let Some(name) = profiles.iter().find(|(_, p)| p.open.is_empty()).map(|(n, _)| n) {
        bail!("[{name}]: `open` is empty (a command and its arguments)");
    }
    Ok(profiles)
}

/// The profiles in `path`; a missing file is none.
pub fn load_profiles(path: &Path) -> Result<HostProfiles> {
    match std::fs::read_to_string(path) {
        Ok(contents) => parse_profiles(&contents).with_context(|| format!("invalid {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HostProfiles::new()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// The argv that opens `url` in `profile` on `os`: the profile's `open`
/// command from `profiles` with `url` appended when it maps one, else the
/// system opener ([`opener_argv`]); also for no profile.
pub fn open_argv(os: &str, profile: Option<&str>, profiles: &HostProfiles, url: &str) -> Option<Vec<String>> {
    match profile.and_then(|p| profiles.get(p)) {
        Some(p) => Some(p.open.iter().cloned().chain([url.to_string()]).collect()),
        None => opener_argv(os, url),
    }
}

/// The profile an open of `url` for `instance` (its `state.toml` entry
/// `inst`, found by id; `None` when it's gone) carries to the opening host,
/// decided on the instance's host: the instance's profile, unless
/// `snapshot::profile_disallowed` says its sandbox no longer allows that one
/// (`allowed` is asked only then, it loads the config), in which case none
/// and a `serve.log` warning (config drift is flagged, never followed). No
/// instance or no profile: none. Only this name ever leaves the host
/// (`open.deliver`); the opening host maps it ([`open_argv`]).
pub fn profile_to_send(
    instance: &str,
    url: &str,
    inst: Option<&Instance>,
    allowed: impl FnOnce(&Instance) -> Option<Vec<String>>,
) -> (Option<String>, Option<String>) {
    let Some((inst, profile)) = inst.and_then(|i| Some((i, i.profile.as_deref()?))) else {
        return (None, None);
    };
    if crate::snapshot::profile_disallowed(Some(profile), allowed(inst).as_deref()) {
        let warning =
            format!("{instance}: profile `{profile}` no longer allowed by the config; opening {url} with the default opener");
        return (None, Some(warning));
    }
    (Some(profile.to_string()), None)
}

/// Where an open's loopback forwards go: an instance on this host
/// (`machine` `None`: `dir` its `config_dir`, `instance` its state key) or
/// on fleet member `machine` (an open delivered from there, `open.deliver`:
/// `dir` and `instance` as it named them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardTarget {
    pub machine: Option<String>,
    pub dir: PathBuf,
    pub instance: String,
}

impl ForwardTarget {
    /// The target of a local instance (its state key and entry), or why it
    /// has none.
    pub fn local(inst: Option<(&str, &Instance)>) -> Result<ForwardTarget, String> {
        let (key, i) = inst.ok_or_else(|| "not in state.toml".to_string())?;
        let dir = i.config_dir.clone().ok_or_else(|| "no config root in state.toml".to_string())?;
        Ok(ForwardTarget { machine: None, dir, instance: key.to_string() })
    }
}

/// One URL for the opening host to open ([`open_here`]): `instance` its
/// display name (for log lines), `profile` the name [`profile_to_send`]
/// chose, `target` where its loopback forwards go (or why there is none).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenJob {
    pub instance: String,
    pub url: String,
    pub profile: Option<String>,
    pub target: Result<ForwardTarget, String>,
}

/// The job for an open of `url` from local `instance`, whose `state.toml`
/// entry (key and record) is `found`: [`profile_to_send`] (its warning
/// second) and the [`ForwardTarget::local`].
pub fn local_job(
    instance: &str,
    url: &str,
    found: Option<(&str, &Instance)>,
    allowed: impl FnOnce(&Instance) -> Option<Vec<String>>,
) -> (OpenJob, Option<String>) {
    let (profile, warning) = profile_to_send(instance, url, found.map(|(_, i)| i), allowed);
    let job = OpenJob { instance: instance.to_string(), url: url.to_string(), profile, target: ForwardTarget::local(found) };
    (job, warning)
}

/// A loopback port a URL needs reachable on this host: a container's
/// `localhost` is its own, so the opener forwards it first
/// (docs/architecture/helper/bridges.md, *Loopback forwards*).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Loopback {
    pub port: u16,
    /// An OAuth callback (`redirect_uri` and the like): the provider
    /// redirects to exactly this port, so it can't be moved.
    pub callback: bool,
}

/// The query keys whose value may be a loopback callback URL.
const CALLBACK_KEYS: [&str; 3] = ["redirect_uri", "redirect_url", "callback"];

/// A `scheme://[userinfo@]host[:port]...` URL cut into byte ranges.
struct Parts<'a> {
    scheme: &'a str,
    /// Where the host starts (after any userinfo).
    host_at: usize,
    host: &'a str,
    /// The text after `:`, when there is one (may be empty or garbage).
    port: Option<&'a str>,
    /// Where the authority ends (the path, query or fragment, if any).
    rest_at: usize,
}

fn parts(url: &str) -> Option<Parts<'_>> {
    let (scheme, after) = url.split_once("://")?;
    let start = scheme.len() + 3;
    let authority = &after[..after.find(['/', '?', '#']).unwrap_or(after.len())];
    let host_at = start + authority.rfind('@').map_or(0, |i| i + 1);
    let host_port = &url[host_at..start + authority.len()];
    let (host, port) = match host_port.strip_prefix('[') {
        Some(v6) => {
            let end = v6.find(']')? + 2;
            let port = &host_port[end..];
            let port = if port.is_empty() { None } else { Some(port.strip_prefix(':')?) };
            (&host_port[..end], port)
        }
        None => match host_port.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (host_port, None),
        },
    };
    Some(Parts { scheme, host_at, host, port, rest_at: start + authority.len() })
}

fn is_loopback(host: &str) -> bool {
    ["localhost", "127.0.0.1", "[::1]", "0.0.0.0"].iter().any(|h| host.eq_ignore_ascii_case(h))
}

/// The port of an http(s) URL on a loopback host: given, else the scheme's
/// default. `None`: not http(s), not loopback, or a port that isn't 1-65535.
fn loopback_port(url: &str) -> Option<u16> {
    let p = parts(url)?;
    if !is_loopback(p.host) {
        return None;
    }
    let default = match p.scheme.to_ascii_lowercase().as_str() {
        "http" => 80,
        "https" => 443,
        _ => return None,
    };
    match p.port {
        None | Some("") => Some(default),
        Some(port) => port.parse().ok().filter(|&p| p != 0),
    }
}

/// `%XX` decoded (a malformed escape stays as it is).
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = || std::str::from_utf8(b.get(i + 1..i + 3)?).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
        match (b[i], hex()) {
            (b'%', Some(byte)) => {
                out.push(byte);
                i += 3;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The loopback ports `url` needs forwarded: its own port when its host is
/// `localhost`, `127.0.0.1`, `[::1]` or `0.0.0.0` (any case), and the port
/// of each loopback http(s) URL in its query under [`CALLBACK_KEYS`]
/// (percent-decoded). Only a key's first value counts, so repeating a key
/// can't make the host bind many ports; nested URLs aren't followed. One
/// entry per port, the direct one first; a port that is both is a callback
/// (it needs that exact port).
pub fn loopback_ports(url: &str) -> Vec<Loopback> {
    let mut out: Vec<Loopback> = loopback_port(url).map(|port| Loopback { port, callback: false }).into_iter().collect();
    let query = parts(url).map_or("", |p| {
        let rest = &url[p.rest_at..];
        let rest = &rest[..rest.find('#').unwrap_or(rest.len())];
        rest.split_once('?').map_or("", |(_, q)| q)
    });
    let mut seen = Vec::new();
    for (key, value) in query.split('&').filter_map(|kv| kv.split_once('=')) {
        let key = percent_decode(key);
        if !CALLBACK_KEYS.contains(&key.as_str()) || seen.contains(&key) {
            continue;
        }
        seen.push(key);
        let Some(port) = loopback_port(&percent_decode(value)) else { continue };
        match out.iter_mut().find(|l| l.port == port) {
            Some(l) => l.callback = true,
            None => out.push(Loopback { port, callback: true }),
        }
    }
    out
}

/// `url` with its loopback host and port made right for this host: the port
/// set to `port` (added, or replaced; userinfo, path, query and fragment
/// kept), and a `0.0.0.0` host made `localhost`, also with `port` `None`
/// (Chrome refuses to navigate to 0.0.0.0, and a server listening on it
/// takes localhost too). A URL that doesn't parse is returned as is.
pub fn rewrite_direct(url: &str, port: Option<u16>) -> String {
    let Some(p) = parts(url) else { return url.to_string() };
    let host = if p.host == "0.0.0.0" { "localhost" } else { p.host };
    let port = match (port, p.port) {
        (Some(n), _) => format!(":{n}"),
        (None, Some(text)) => format!(":{text}"),
        (None, None) => String::new(),
    };
    format!("{}{host}{port}{}", &url[..p.host_at], &url[p.rest_at..])
}

/// Forward one [`Loopback`] port of the instance at a [`ForwardTarget`] →
/// the bound host port. `Err(Some(why))` for the opener's log line,
/// `Err(None)` when the forwarder already logged why (an OAuth callback
/// port it couldn't bind). The daemon's is the forwards registry's open
/// forwards (`serve::opens`); a seam, so this module stays free of `serve`
/// (unix-only) and tests fake it.
pub type Forwarder = Arc<dyn Fn(&ForwardTarget, Loopback) -> Result<u16, Option<String>> + Send + Sync>;

/// The forwarding step before an open: each of `url`'s [`loopback_ports`]
/// forwarded to `target` through `forward`, and the URL to open: rewritten
/// to the direct port's bound host port when that differs, and off a
/// `0.0.0.0` host ([`rewrite_direct`]). Anything that fails (no target, a
/// forward refused) leaves that port as it was and is a log line naming
/// `instance` and `url` (unless the forwarder logged it, `Err(None)`); the
/// URL still opens. Third: every port that didn't forward, for the
/// [`Outcome::Opened`] line.
pub fn forward_loopbacks(
    instance: &str,
    url: &str,
    target: Result<&ForwardTarget, &str>,
    forward: impl Fn(&ForwardTarget, Loopback) -> Result<u16, Option<String>>,
) -> (String, Vec<String>, Vec<Loopback>) {
    let wanted = loopback_ports(url);
    let direct = loopback_port(url);
    let mut lines = Vec::new();
    let mut unforwarded = Vec::new();
    let mut bound = None;
    if !wanted.is_empty() {
        match target {
            Err(why) => {
                let ports: Vec<String> = wanted.iter().map(|l| l.port.to_string()).collect();
                let s = if ports.len() > 1 { "s" } else { "" };
                lines.push(format!("{instance}: cannot forward port{s} {} for {url} ({why}); opening it as is", ports.join(", ")));
                unforwarded = wanted;
            }
            Ok(target) => {
                for lb in wanted {
                    match forward(target, lb) {
                        Ok(host_port) if Some(lb.port) == direct => bound = Some(host_port),
                        Ok(_) => {}
                        Err(None) => unforwarded.push(lb),
                        Err(Some(why)) => {
                            let what = if lb.callback { "callback port" } else { "port" };
                            lines.push(format!("{instance}: cannot forward {what} {} for {url}: {why}", lb.port));
                            unforwarded.push(lb);
                        }
                    }
                }
            }
        }
    }
    let rewrite = bound.filter(|&b| Some(b) != direct);
    let opened = if rewrite.is_some() || parts(url).is_some_and(|p| p.host == "0.0.0.0") && direct.is_some() {
        rewrite_direct(url, rewrite)
    } else {
        url.to_string()
    };
    (opened, lines, unforwarded)
}

/// The session variables a browser on this host needs: the display
/// (X11 or Wayland) and X11's auth cookie (GNOME's Xwayland puts it under
/// `$XDG_RUNTIME_DIR`, so `~/.Xauthority` isn't it).
const DISPLAY_ENV: [&str; 3] = ["DISPLAY", "WAYLAND_DISPLAY", "XAUTHORITY"];

/// Whether this host can show a browser: on Linux, `DISPLAY` or
/// `WAYLAND_DISPLAY` set (non-empty), read through `env`. Other OSes always
/// say yes (macOS's `open` needs no env).
pub fn has_display(os: &str, env: impl Fn(&str) -> Option<OsString>) -> bool {
    os != "linux" || DISPLAY_ENV[..2].iter().any(|k| env(k).is_some_and(|v| !v.is_empty()))
}

/// [`DISPLAY_ENV`] out of `systemctl --user show-environment`'s output
/// (`KEY=value` lines). A shell-quoted value (systemd prints one with special
/// characters as `$'...'`) is skipped: display names and the auth path never
/// need quoting.
pub fn session_display_env(show_environment: &str) -> Vec<(String, String)> {
    show_environment
        .lines()
        .filter_map(|l| l.split_once('='))
        .filter(|(k, v)| DISPLAY_ENV.contains(k) && !v.is_empty() && !v.starts_with("$'"))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// The display env for the opener on `os`: `Some(empty)` when the daemon's
/// own env has a display (the child inherits it); on Linux otherwise the
/// user manager's current env, which the desktop session imports into at
/// login (GNOME and Plasma do; `systemctl --user import-environment` in
/// other sessions). A `serve install`ed daemon starts with `default.target`,
/// before the session does that, so its own env never has one; reading the
/// manager's at open time also follows a session that came (or went) since.
/// `None`: no display anywhere.
fn display_env(os: &str) -> Option<Vec<(String, String)>> {
    if has_display(os, |k| std::env::var_os(k)) {
        return Some(Vec::new());
    }
    if os != "linux" {
        return None;
    }
    let out = Command::new("systemctl")
        .args(["--user", "show-environment"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let env = session_display_env(&String::from_utf8_lossy(&out.stdout));
    let lookup = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| OsString::from(v));
    has_display(os, lookup).then_some(env)
}

/// The instance's host's half of an open from `instance` (`instance_id`, so
/// a rename doesn't lose it): its `state.toml` entry, read on every open,
/// made into an [`OpenJob`] ([`local_job`]; the drift warning logged). An
/// unreadable `state.toml` is logged and the job goes on with no profile
/// and no forward target.
pub fn prepare(instance: &str, instance_id: &str, url: &str, log: fn(&str)) -> OpenJob {
    let state = State::load().unwrap_or_else(|e| {
        log(&format!("{instance}: {e:#}; opening {url} with the default opener"));
        State::default()
    });
    let found = state.instances.iter().find(|(_, i)| i.instance_id == instance_id).map(|(k, i)| (k.as_str(), i));
    let allowed = |i: &Instance| crate::snapshot::allowed_now(&mut BTreeMap::new(), i);
    let (job, warning) = local_job(instance, url, found, allowed);
    if let Some(warning) = &warning {
        log(warning);
    }
    job
}

/// The opening host's half: open `job`'s URL here with the command
/// [`open_argv`] maps its profile to in this host's `profiles.toml` (read on
/// every open, so an edit applies without a daemon restart; unreadable, it
/// is logged and the system opener used) and the [`display_env`], all stdio
/// null, after [`forward_loopbacks`] through `forward` (the URL the command
/// gets is the rewritten one). Blocks until the child exits (it reaps it):
/// callers run it on a thread of its own, so a bridge's handler or an API
/// request never waits on it (nor on the forwards), like
/// `desktop::notify_desktop`. No display, no known opener or a failed spawn
/// is one `log` line naming the instance and the URL. Every open ends in
/// one `done` ([`Outcome::Opened`] once the child is spawned, else
/// [`Outcome::Failed`]), naming a delivered open's instance `machine/name`.
pub fn open_here(job: OpenJob, forward: &Forwarder, log: fn(&str), done: &OnDone) {
    let OpenJob { instance, url, profile, target } = job;
    let who = match &target {
        Ok(ForwardTarget { machine: Some(m), .. }) => format!("{m}/{instance}"),
        _ => instance.clone(),
    };
    let finish = |url: &str, outcome: Outcome| done(OpenDone { instance: who.clone(), url: url.to_string(), profile: profile.clone(), outcome });
    let os = std::env::consts::OS;
    let Some(env) = display_env(os) else {
        log(&format!("{instance}: no display to open {url} on"));
        return finish(&url, Outcome::Failed("no display on this host".into()));
    };
    let profiles = profiles_path().and_then(|p| load_profiles(&p)).unwrap_or_else(|e| {
        log(&format!("{instance}: {e:#}; opening {url} with the default opener"));
        HostProfiles::new()
    });
    let (url, lines, unforwarded) = forward_loopbacks(&instance, &url, target.as_ref().map_err(String::as_str), &**forward);
    for line in &lines {
        log(line);
    }
    let Some(argv) = open_argv(os, profile.as_deref(), &profiles, &url) else {
        log(&format!("{instance}: no opener on {os} for {url}"));
        return finish(&url, Outcome::Failed(format!("no opener on {os}")));
    };
    let spawned = Command::new(&argv[0])
        .args(&argv[1..])
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match spawned {
        Ok(mut child) => {
            // Told before the wait: `xdg-open` may not exit until the
            // browser does.
            finish(&url, Outcome::Opened { unforwarded });
            let _ = child.wait();
        }
        Err(e) => {
            log(&format!("{instance}: cannot run `{}` to open {url}: {e}", argv[0]));
            finish(&url, Outcome::Failed(format!("cannot run `{}`: {e}", argv[0])));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devsbd::desktop::RateLimit;
    use std::time::Instant;

    const AT: u64 = 1_790_000_000;

    fn open_now(url: &str) -> Verdict {
        decide(url, AT, AT, || true)
    }

    #[test]
    fn decide_allows_http_and_https_only() {
        for url in ["https://example.com/a?b=c", "http://localhost:8080/", "HTTPS://EXAMPLE.COM"] {
            assert_eq!(open_now(url), Verdict::Open, "{url}");
        }
        let bad = |url: &str, scheme: &str| assert_eq!(open_now(url), Verdict::BadScheme(scheme.into()), "{url}");
        bad("ssh://host", "ssh");
        bad("file:///etc/passwd", "file");
        bad("javascript:alert(1)", "javascript");
        bad("mailto:a@b.c", "mailto");
        bad("vscode://file/x", "vscode");
        bad("https:example.com", "https");
        bad("/etc/passwd", "");
        bad("report.pdf", "");
        bad("", "");
        bad("-https://x", "-https");
    }

    #[test]
    fn decide_drops_records_past_the_ttl() {
        let ttl = OPEN_TTL.as_secs();
        assert_eq!(decide("https://x", AT, AT + ttl, || true), Verdict::Open, "exactly the TTL still opens");
        assert_eq!(decide("https://x", AT, AT + ttl + 1, || true), Verdict::Expired { age: ttl + 1 });
        assert_eq!(decide("https://x", AT + 5, AT, || true), Verdict::Open, "skewed into the future");
    }

    #[test]
    fn decide_spends_the_limit_only_on_otherwise_good_opens() {
        let mut rl = RateLimit::new(OPEN_RATE_BURST, OPEN_RATE_REFILL);
        let t0 = Instant::now();
        let mut try_open = |url: &str, at: u64, t: Instant| decide(url, at, AT, || rl.allow("web-1", None, t));
        // Refused and expired records don't spend tokens.
        for _ in 0..10 {
            assert_eq!(try_open("ssh://x", AT, t0), Verdict::BadScheme("ssh".into()));
            assert!(matches!(try_open("https://x", AT - 60, t0), Verdict::Expired { .. }));
        }
        for _ in 0..OPEN_RATE_BURST {
            assert_eq!(try_open("https://x", AT, t0), Verdict::Open);
        }
        assert_eq!(try_open("https://x", AT, t0), Verdict::Limited);
        assert_eq!(try_open("https://x", AT, t0 + OPEN_RATE_REFILL), Verdict::Open, "one refilled");
        // Another instance has its own bucket.
        assert!(rl.allow("web-2", None, t0));
    }

    #[test]
    fn refusals_log_the_instance_and_url() {
        assert_eq!(Verdict::Open.log_line("web-1", "https://x"), None);
        let line = |v: Verdict| v.log_line("web-1", "ssh://x").unwrap();
        assert_eq!(line(Verdict::BadScheme("ssh".into())), "web-1: open refused, scheme `ssh` (http and https only): ssh://x");
        assert_eq!(line(Verdict::BadScheme(String::new())), "web-1: open refused, not a URL with a scheme: ssh://x");
        assert_eq!(line(Verdict::Expired { age: 45 }), "web-1: open dropped, queued 45s ago (over 30s): ssh://x");
        assert_eq!(line(Verdict::Limited), "web-1: open refused, rate limit (5 per 10s): ssh://x");
    }

    #[test]
    fn opener_argv_per_os() {
        assert_eq!(opener_argv("linux", "https://x/?a=b c"), Some(vec!["xdg-open".into(), "https://x/?a=b c".into()]));
        assert_eq!(opener_argv("macos", "https://x"), Some(vec!["open".into(), "https://x".into()]));
        assert_eq!(opener_argv("windows", "https://x"), None);
    }

    #[test]
    fn profiles_toml_parses_open_commands() {
        let profiles = parse_profiles(
            "[work]\nopen = [\"google-chrome\", \"--profile-directory=Profile 2\"]\n\n[personal]\nopen = [\"firefox\"]\n",
        )
        .unwrap();
        assert_eq!(profiles.len(), 2);
        assert_eq!(profiles["work"].open, vec!["google-chrome", "--profile-directory=Profile 2"]);
        assert_eq!(profiles["personal"].open, vec!["firefox"]);
        assert!(parse_profiles("").unwrap().is_empty(), "an empty file is no profiles");
        assert!(parse_profiles("# nothing yet\n").unwrap().is_empty());
    }

    #[test]
    fn profiles_toml_refuses_bad_entries() {
        let err = |s: &str| format!("{:#}", parse_profiles(s).unwrap_err());
        assert!(err("[work]\nopen = []\n").contains("[work]: `open` is empty"));
        err("[work]\nopen = [\"chrome\", 2]\n");
        err("[work]\nopen = \"chrome\"\n");
        err("[work]\n");
        assert!(err("[work]\nopen = [\"chrome\"]\nicon = \"x\"\n").contains("icon"), "unknown keys refused");
        err("open = [\"chrome\"]\n");
    }

    #[test]
    fn a_missing_profiles_toml_is_none() {
        let dir = std::env::temp_dir().join(format!("devsandbox-open-test-{}", std::process::id()));
        assert!(load_profiles(&dir.join("profiles.toml")).unwrap().is_empty());
    }

    fn profiles() -> HostProfiles {
        parse_profiles("[work]\nopen = [\"chrome\", \"--profile-directory=Work\"]\n").unwrap()
    }

    #[test]
    fn open_argv_uses_a_mapped_profile_else_the_system_opener() {
        let url = "https://x/?a=b";
        let argv = |v: &[&str]| Some(v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(open_argv("linux", Some("work"), &profiles(), url), argv(&["chrome", "--profile-directory=Work", url]));
        assert_eq!(open_argv("macos", Some("work"), &profiles(), url), argv(&["chrome", "--profile-directory=Work", url]));
        assert_eq!(open_argv("linux", Some("personal"), &profiles(), url), argv(&["xdg-open", url]), "unmapped");
        assert_eq!(open_argv("macos", None, &profiles(), url), argv(&["open", url]), "no profile");
        assert_eq!(open_argv("linux", None, &HostProfiles::new(), url), argv(&["xdg-open", url]));
    }

    /// One state entry, as `state.toml` holds it, with `profile`.
    fn instance(profile: Option<&str>) -> Instance {
        let profile = profile.map_or(String::new(), |p| format!("profile = \"{p}\"\n"));
        let state: State = toml::from_str(&format!(
            "[instance.web-1]\nsandbox = \"web\"\ninstance_id = \"web-1\"\ncontainer = \"devsandbox-web-1\"\n\
             folder = \"/src/web\"\nworkspace = \"/workspaces/web\"\ncreated_unix = 1\n{profile}"
        ))
        .unwrap();
        state.instances.into_values().next().unwrap()
    }

    #[test]
    fn the_instances_profile_is_sent_unless_the_config_dropped_it() {
        let url = "https://x";
        let allowed = |set: &'static [&'static str]| {
            move |_: &Instance| Some(set.iter().map(|s| s.to_string()).collect::<Vec<_>>())
        };
        let send = |inst: Option<&Instance>, set: &'static [&'static str]| profile_to_send("web-1", url, inst, allowed(set));
        let named = |p: &str| (Some(p.to_string()), None);

        let work = instance(Some("work"));
        assert_eq!(send(Some(&work), &["work", "personal"]), named("work"));
        assert_eq!(send(Some(&instance(Some("personal"))), &["work", "personal"]), named("personal"), "mapped or not: the opener's call");
        assert_eq!(send(Some(&instance(None)), &["work"]), (None, None), "no profile");
        assert_eq!(send(None, &["work"]), (None, None), "no instance");
        assert_eq!(profile_to_send("web-1", url, Some(&work), |_| None), named("work"), "an allowed set that can't be told doesn't flag");
        assert_eq!(
            send(Some(&work), &["personal"]),
            (
                None,
                Some("web-1: profile `work` no longer allowed by the config; opening https://x with the default opener".into())
            ),
            "disallowed"
        );
        let unasked = |_: &Instance| -> Option<Vec<String>> { panic!("no profile: the config isn't loaded") };
        profile_to_send("web-1", url, Some(&instance(None)), unasked);
    }

    #[test]
    fn a_local_job_carries_the_profile_and_the_instances_target() {
        let inst = Instance { config_dir: Some("/cfg".into()), ..instance(Some("work")) };
        let (job, warning) = local_job("web", "https://x", Some(("web-1", &inst)), |_| None);
        assert_eq!(warning, None);
        assert_eq!(
            job,
            OpenJob {
                instance: "web".into(),
                url: "https://x".into(),
                profile: Some("work".into()),
                target: Ok(ForwardTarget { machine: None, dir: "/cfg".into(), instance: "web-1".into() }),
            }
        );
        let bare = instance(None);
        assert_eq!(local_job("web", "https://x", Some(("web-1", &bare)), |_| None).0.target, Err("no config root in state.toml".into()));
        assert_eq!(local_job("web", "https://x", None, |_| None).0.target, Err("not in state.toml".into()));
    }

    fn direct(port: u16) -> Loopback {
        Loopback { port, callback: false }
    }

    fn callback(port: u16) -> Loopback {
        Loopback { port, callback: true }
    }

    #[test]
    fn loopback_ports_of_direct_urls() {
        let cases: &[(&str, &[Loopback])] = &[
            ("http://localhost:3000/app?x=1#top", &[direct(3000)]),
            ("http://127.0.0.1:8080", &[direct(8080)]),
            ("http://[::1]:5173/", &[direct(5173)]),
            ("http://0.0.0.0:8000/", &[direct(8000)]),
            ("HTTP://LOCALHOST:3000/", &[direct(3000)]),
            ("http://[::1]/", &[direct(80)]),
            ("http://localhost/", &[direct(80)]),
            ("https://localhost", &[direct(443)]),
            ("http://localhost:/", &[direct(80)]),
            ("http://user:pw@localhost:9000/", &[direct(9000)]),
            ("http://localhost@example.com:3000/", &[]),
            ("https://example.com/", &[]),
            ("http://localhost.evil.com:3000/", &[]),
            ("http://127.0.0.2:3000/", &[]),
            ("http://[::2]:3000/", &[]),
            ("http://localhost:0/", &[]),
            ("http://localhost:65536/", &[]),
            ("http://localhost:3x/", &[]),
            ("http://[::1]x/", &[]),
            ("not a url", &[]),
        ];
        for (url, want) in cases {
            assert_eq!(loopback_ports(url), *want, "{url}");
        }
    }

    #[test]
    fn loopback_ports_of_callbacks() {
        let cases: &[(&str, &[Loopback])] = &[
            (
                "https://github.com/login/oauth/authorize?client_id=x&redirect_uri=http%3A%2F%2Flocalhost%3A8085%2Fcb&state=y",
                &[callback(8085)],
            ),
            ("https://idp.example/auth?redirect_uri=http://127.0.0.1:9004/cb", &[callback(9004)]),
            ("https://idp.example/auth?redirect_url=http%3a%2f%2f%5b%3a%3a1%5d%3a7000", &[callback(7000)]),
            ("https://idp.example/auth?callback=http://localhost/cb", &[callback(80)]),
            (
                "https://idp.example/auth?redirect_uri=http://localhost:1&redirect_url=http://localhost:2&callback=http://localhost:3",
                &[callback(1), callback(2), callback(3)],
            ),
            // Only a key's first value, even when it isn't a loopback one.
            ("https://idp.example/a?redirect_uri=http://localhost:1&redirect_uri=http://localhost:2", &[callback(1)]),
            ("https://idp.example/a?redirect_uri=https://example.com&redirect_uri=http://localhost:2", &[]),
            ("https://idp.example/a?callback=ssh://localhost:22", &[]),
            ("https://idp.example/a?callback=localhost:22", &[]),
            ("https://idp.example/a?redirect_uri=http://localhost.evil.com:1", &[]),
            ("https://idp.example/a?return_to=http://localhost:1", &[]),
            ("https://idp.example/a#redirect_uri=http://localhost:1", &[]),
            ("https://idp.example/a?x=1#redirect_uri=http://localhost:1", &[]),
            // Nested URLs aren't followed.
            ("https://idp.example/a?next=https%3A%2F%2Fb%2F%3Fredirect_uri%3Dhttp%3A%2F%2Flocalhost%3A1", &[]),
            // Direct first; the same port both ways is a callback.
            ("http://localhost:3000/login?redirect_uri=http://localhost:4000/cb", &[direct(3000), callback(4000)]),
            ("http://localhost:3000/login?redirect_uri=http://localhost:3000/cb", &[callback(3000)]),
        ];
        for (url, want) in cases {
            assert_eq!(loopback_ports(url), *want, "{url}");
        }
    }

    #[test]
    fn rewrite_direct_sets_the_port_and_leaves_the_rest() {
        let cases: &[(&str, Option<u16>, &str)] = &[
            ("http://localhost:3000/a?b=c#d", Some(3001), "http://localhost:3001/a?b=c#d"),
            ("http://localhost/a", Some(3001), "http://localhost:3001/a"),
            ("https://localhost", Some(8443), "https://localhost:8443"),
            ("http://localhost?q=http://x:1", Some(5), "http://localhost:5?q=http://x:1"),
            ("http://[::1]:3000/", Some(3001), "http://[::1]:3001/"),
            ("http://[::1]/", Some(3001), "http://[::1]:3001/"),
            ("http://u:p@127.0.0.1:3000/", Some(3001), "http://u:p@127.0.0.1:3001/"),
            ("http://0.0.0.0:8000/x", None, "http://localhost:8000/x"),
            ("http://0.0.0.0/x", None, "http://localhost/x"),
            ("http://0.0.0.0:8000/x", Some(8001), "http://localhost:8001/x"),
            ("http://localhost:3000/", None, "http://localhost:3000/"),
            ("not a url", Some(1), "not a url"),
        ];
        for (url, port, want) in cases {
            assert_eq!(rewrite_direct(url, *port), *want, "{url} {port:?}");
        }
    }

    /// The local target of state key `web-1` with config root `/cfg`.
    fn local() -> ForwardTarget {
        ForwardTarget::local(Some(("web-1", &Instance { config_dir: Some("/cfg".into()), ..instance(None) }))).unwrap()
    }

    type Call = (ForwardTarget, Loopback);

    /// A fake forwarder: bind `bound(port)`, recording each call.
    fn forwarding(
        calls: &std::cell::RefCell<Vec<Call>>,
        bound: impl Fn(Loopback) -> Result<u16, Option<String>>,
    ) -> impl Fn(&ForwardTarget, Loopback) -> Result<u16, Option<String>> {
        move |t, lb| {
            calls.borrow_mut().push((t.clone(), lb));
            bound(lb)
        }
    }

    #[test]
    fn forward_loopbacks_rewrites_a_moved_direct_port() {
        let calls = Default::default();
        let t = local();
        let got = forward_loopbacks("web", "http://localhost:3000/x", Ok(&t), forwarding(&calls, |_| Ok(3001)));
        assert_eq!(got, ("http://localhost:3001/x".to_string(), vec![], vec![]));
        assert_eq!(calls.into_inner(), vec![(t.clone(), direct(3000))]);

        let got = forward_loopbacks("web", "http://localhost:3000/x", Ok(&t), |_: &ForwardTarget, lb: Loopback| Ok(lb.port));
        assert_eq!(got, ("http://localhost:3000/x".to_string(), vec![], vec![]), "bound on the same port");

        let got = forward_loopbacks("web", "http://0.0.0.0:8000/", Ok(&t), |_: &ForwardTarget, lb: Loopback| Ok(lb.port));
        assert_eq!(got.0, "http://localhost:8000/", "0.0.0.0 made localhost");
        let got = forward_loopbacks("web", "http://0.0.0.0:8000/", Ok(&t), |_: &ForwardTarget, _| Ok(8001));
        assert_eq!(got.0, "http://localhost:8001/");
    }

    #[test]
    fn forward_loopbacks_hands_a_machines_target_through() {
        let calls = Default::default();
        let t = ForwardTarget { machine: Some("box".into()), dir: "/home/u/cfg".into(), instance: "web-1".into() };
        let got = forward_loopbacks("web-1", "http://localhost:3000/", Ok(&t), forwarding(&calls, |_| Err(Some("no machine named `box`".into()))));
        assert_eq!(got.1, vec!["web-1: cannot forward port 3000 for http://localhost:3000/: no machine named `box`".to_string()]);
        assert_eq!(got.2, vec![direct(3000)]);
        assert_eq!(calls.into_inner(), vec![(t, direct(3000))]);
    }

    #[test]
    fn forward_loopbacks_leaves_a_callback_url_as_is() {
        let calls = Default::default();
        let t = local();
        let url = "https://github.com/login?redirect_uri=http%3A%2F%2Flocalhost%3A8085%2Fcb";
        let got = forward_loopbacks("web", url, Ok(&t), forwarding(&calls, |lb| Ok(lb.port)));
        assert_eq!(got, (url.to_string(), vec![], vec![]));
        assert_eq!(calls.into_inner(), vec![(t.clone(), callback(8085))]);

        // A callback the forwarder already reported: no second log line,
        // still unforwarded.
        let got = forward_loopbacks("web", url, Ok(&t), |_: &ForwardTarget, _| Err(None));
        assert_eq!(got, (url.to_string(), vec![], vec![callback(8085)]));
        let got = forward_loopbacks("web", url, Ok(&t), |_: &ForwardTarget, _| Err(Some("registry stopped".into())));
        assert_eq!(got.1, vec![format!("web: cannot forward callback port 8085 for {url}: registry stopped")]);
        assert_eq!(got.2, vec![callback(8085)]);
    }

    #[test]
    fn forward_loopbacks_falls_back_to_the_url_as_is() {
        let t = local();
        let url = "http://localhost:3000/";
        let got = forward_loopbacks("web", url, Ok(&t), |_: &ForwardTarget, _| Err(Some("port 3000: nope".into())));
        assert_eq!(got, (url.to_string(), vec![format!("web: cannot forward port 3000 for {url}: port 3000: nope")], vec![direct(3000)]));

        let never = |_: &ForwardTarget, _| -> Result<u16, Option<String>> { panic!("nothing to forward to") };
        let got = forward_loopbacks("web", url, Err("no config root in state.toml"), never);
        assert_eq!(
            got,
            (url.to_string(), vec![format!("web: cannot forward port 3000 for {url} (no config root in state.toml); opening it as is")], vec![direct(3000)])
        );
        let got = forward_loopbacks("web", "http://0.0.0.0:3000/?callback=http://localhost:9", Err("not in state.toml"), never);
        assert_eq!(
            got,
            (
                "http://localhost:3000/?callback=http://localhost:9".to_string(),
                vec!["web: cannot forward ports 3000, 9 for http://0.0.0.0:3000/?callback=http://localhost:9 (not in state.toml); opening it as is".to_string()],
                vec![direct(3000), callback(9)],
            ),
            "no target; 0.0.0.0 still made localhost"
        );
        assert_eq!(forward_loopbacks("web", "https://example.com/", Err("x"), never), ("https://example.com/".to_string(), vec![], vec![]));
    }

    fn done(profile: Option<&str>, outcome: Outcome) -> OpenDone {
        OpenDone { instance: "web".into(), url: "https://x.example/a".into(), profile: profile.map(Into::into), outcome }
    }

    #[test]
    fn each_outcome_is_one_status_line() {
        let opened = Outcome::Opened { unforwarded: vec![] };
        assert_eq!(done(Some("work"), opened.clone()).line(), "web: opened https://x.example/a (work)");
        assert_eq!(done(None, opened.clone()).line(), "web: opened https://x.example/a");
        assert_eq!((done(None, opened).kind(), done(None, Outcome::Opened { unforwarded: vec![] }).why()), ("opened", None));
        let blocked = done(Some("work"), Outcome::Opened { unforwarded: vec![callback(8085), direct(3000)] });
        assert_eq!(blocked.line(), "web: opened https://x.example/a (work); cannot forward callback port 8085, port 3000");
        assert_eq!(blocked.why().as_deref(), Some("cannot forward callback port 8085, port 3000"));

        let why = Verdict::BadScheme("ssh".into()).refusal().unwrap();
        let refused = done(None, Outcome::Refused(why));
        assert_eq!((refused.kind(), refused.line().as_str()), ("refused", "web: open refused: scheme `ssh` (http and https only)"));
        let failed = done(Some("work"), Outcome::Failed("no display on this host".into()));
        assert_eq!((failed.kind(), failed.line().as_str()), ("failed", "web: open failed: no display on this host"));
        assert_eq!(failed.why().as_deref(), Some("no display on this host"));
    }

    #[test]
    fn long_urls_and_reasons_are_shortened_in_the_line_only() {
        let url = format!("https://idp.example/auth?{}", "q".repeat(500));
        let d = OpenDone { url: url.clone(), ..done(Some("work"), Outcome::Opened { unforwarded: vec![] }) };
        let line = d.line();
        let shown = line.strip_prefix("web: opened ").unwrap().strip_suffix(" (work)").unwrap();
        assert_eq!(shown.chars().count(), LINE_URL);
        assert!(shown.ends_with('…') && url.starts_with(shown.trim_end_matches('…')));
        assert_eq!(d.url, url, "the wire keeps it whole");
        let d = done(None, Outcome::Failed("é".repeat(400)));
        assert_eq!(d.line().strip_prefix("web: open failed: ").unwrap().chars().count(), LINE_WHY);
        assert_eq!(shorten("abc", 3), "abc");
        assert_eq!(shorten("abcd", 3), "ab…");
    }

    #[test]
    fn display_comes_from_the_env_on_linux() {
        let env = |set: &'static [(&'static str, &'static str)]| {
            move |k: &str| set.iter().find(|(n, _)| *n == k).map(|(_, v)| OsString::from(v))
        };
        assert!(!has_display("linux", env(&[])));
        assert!(!has_display("linux", env(&[("DISPLAY", "")])));
        assert!(has_display("linux", env(&[("DISPLAY", ":0")])));
        assert!(has_display("linux", env(&[("WAYLAND_DISPLAY", "wayland-0")])));
        assert!(!has_display("linux", env(&[("XAUTHORITY", "/run/user/1000/a")])), "auth alone isn't a display");
        assert!(has_display("macos", env(&[])));
    }

    #[test]
    fn session_display_env_picks_the_display_vars() {
        let out = "DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus\n\
                   DISPLAY=:0\n\
                   HOME=/home/u\n\
                   WAYLAND_DISPLAY=wayland-0\n\
                   XAUTHORITY=/run/user/1000/.mutter-Xwaylandauth.AB12\n\
                   XDG_CURRENT_DESKTOP=GNOME\n";
        let pair = |k: &str, v: &str| (k.to_string(), v.to_string());
        assert_eq!(
            session_display_env(out),
            vec![
                pair("DISPLAY", ":0"),
                pair("WAYLAND_DISPLAY", "wayland-0"),
                pair("XAUTHORITY", "/run/user/1000/.mutter-Xwaylandauth.AB12"),
            ]
        );
        // A headless manager: nothing; quoted and empty values are skipped.
        assert!(session_display_env("HOME=/home/u\nPATH=/usr/bin\n").is_empty());
        assert!(session_display_env("DISPLAY=\nXAUTHORITY=$'/tmp/a b'\n").is_empty());
    }
}
