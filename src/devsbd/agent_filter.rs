//! The agent relay's key filter while a git-host claim applies
//! (docs/architecture/helper/ssh-agent.md, *Key filtering*): the sandbox
//! sees only the host agent's keys its config's `ssh-keys` names, a key
//! with `destinations` signs only logins on a connection bound by
//! `session-bind@openssh.com` to one of their host keys (OpenSSH's
//! destination constraints, `ssh-add -h`), a `signing-only` key only git's
//! ssh signatures. Without it, anything in the sandbox could push to
//! GitHub over ssh with the user's key, past the gateway's repo fence.
//!
//! [`Filter`] is the pure per-connection state machine over agent
//! messages (draft-miller-ssh-agent); [`relay`] runs it between a client
//! and the agent; [`Gate`] is what a bridge holds for an instance with a
//! git-host claim. It runs where the relay ends, on the instance's host, so
//! a remote instance's agent arriving over `AGENT` streams is filtered
//! there, against that host's `known_hosts`.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::broker::redact::base64_decode;
use crate::config::{Config, SshKey, fingerprint_b64};
use crate::state::Instance;

pub const FAILURE: u8 = 5;
const REQUEST_IDENTITIES: u8 = 11;
const IDENTITIES_ANSWER: u8 = 12;
const SIGN_REQUEST: u8 = 13;
const EXTENSION: u8 = 27;
const SESSION_BIND: &[u8] = b"session-bind@openssh.com";
const USERAUTH_REQUEST: u8 = 50;
const SSHSIG_MAGIC: &[u8] = b"SSHSIG";
const GIT_NAMESPACE: &[u8] = b"git";
/// OpenSSH's `AGENT_MAX_LEN`.
const MAX_MESSAGE: usize = 256 * 1024;
/// OpenSSH's `AGENT_MAX_SESSION_IDS`.
const MAX_BINDS: usize = 16;

/// SSH wire reads (RFC 4251 §5) over a borrowed buffer.
struct Wire<'a>(&'a [u8]);

impl<'a> Wire<'a> {
    fn u8(&mut self) -> Option<u8> {
        let (&b, rest) = self.0.split_first()?;
        self.0 = rest;
        Some(b)
    }

    fn u32(&mut self) -> Option<u32> {
        let (head, rest) = self.0.split_at_checked(4)?;
        self.0 = rest;
        Some(u32::from_be_bytes(head.try_into().ok()?))
    }

    fn string(&mut self) -> Option<&'a [u8]> {
        let n = self.u32()? as usize;
        let (s, rest) = self.0.split_at_checked(n)?;
        self.0 = rest;
        Some(s)
    }

    fn bool(&mut self) -> Option<bool> {
        self.u8().map(|b| b != 0)
    }

    fn done(&self) -> bool {
        self.0.is_empty()
    }
}

fn put_string(out: &mut Vec<u8>, s: &[u8]) {
    out.extend_from_slice(&(s.len() as u32).to_be_bytes());
    out.extend_from_slice(s);
}

/// Where a destination's host keys come from: the filtering host's own
/// `known_hosts`, never the container's, where the sandbox could plant a
/// key under the destination's name.
pub trait KnownHosts {
    /// `host`'s key blobs, revoked ones dropped; `Err` when the lookup
    /// couldn't run.
    fn keys(&self, host: &str) -> Result<Vec<Vec<u8>>, String>;
}

/// `ssh-keygen -F <host> -f <file>` over each file that exists, run per
/// lookup so a rotated host key is followed.
pub struct SshKeygen {
    pub program: PathBuf,
    pub files: Vec<PathBuf>,
}

impl SshKeygen {
    /// The files OpenSSH's client reads by default.
    pub fn for_host_user() -> SshKeygen {
        let mut files: Vec<PathBuf> =
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".ssh/known_hosts")).into_iter().collect();
        files.push("/etc/ssh/ssh_known_hosts".into());
        SshKeygen { program: "ssh-keygen".into(), files }
    }
}

impl KnownHosts for SshKeygen {
    fn keys(&self, host: &str) -> Result<Vec<Vec<u8>>, String> {
        let mut found = String::new();
        for file in self.files.iter().filter(|f| f.exists()) {
            let out = Command::new(&self.program)
                .args(["-F", host, "-f"])
                .arg(file)
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
                .map_err(|e| format!("`ssh-keygen -F` didn't run: {e}"))?;
            // Exit 1 is "not found"; its output is empty either way.
            found.push_str(&String::from_utf8_lossy(&out.stdout));
            found.push('\n');
        }
        Ok(parse_known_hosts(&found))
    }
}

/// The key blobs in `ssh-keygen -F`'s output (`known_hosts` lines, `#`
/// comments), but `@revoked` ones; `@cert-authority` lines are skipped:
/// a certificate host key never matches a plain one.
pub fn parse_known_hosts(out: &str) -> Vec<Vec<u8>> {
    let (mut keys, mut revoked) = (Vec::new(), Vec::new());
    for line in out.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let mut words = line.split_whitespace().peekable();
        let marker = words.next_if(|w| w.starts_with('@'));
        let (Some(_hosts), Some(_kind), Some(b64)) = (words.next(), words.next(), words.next()) else { continue };
        let Some(blob) = base64_decode(b64.as_bytes()) else { continue };
        match marker {
            None => keys.push(blob),
            Some("@revoked") => revoked.push(blob),
            Some(_) => {}
        }
    }
    keys.retain(|k| !revoked.contains(k));
    keys
}

/// Checks `signature` (SSH signature encoding) by `hostkey` (a key blob)
/// over `data`. ed25519, ecdsa nistp256/384 and rsa-sha2-256/512; the rest
/// (nistp521, ssh-rsa's SHA-1, certificates, security keys) is refused with
/// its name: OpenSSH negotiates one of the checked ones by default.
pub fn verify(hostkey: &[u8], data: &[u8], signature: &[u8]) -> Result<(), String> {
    use ring::signature as sig;
    let mut key = Wire(hostkey);
    let kind = key.string().ok_or("a malformed host key")?;
    let mut s = Wire(signature);
    let (Some(alg), Some(bytes)) = (s.string(), s.string()) else { return Err("a malformed signature".into()) };
    if !s.done() {
        return Err("a malformed signature".into());
    }
    let bad = || "the host key's signature doesn't check".to_string();
    match kind {
        b"ssh-ed25519" => {
            let pk: [u8; 32] = key.string().and_then(|k| k.try_into().ok()).ok_or("a malformed ed25519 key")?;
            let sig: [u8; 64] = bytes.try_into().map_err(|_| bad())?;
            if alg != kind || !key.done() {
                return Err(bad());
            }
            let pk = ed25519_dalek::VerifyingKey::from_bytes(&pk).map_err(|_| "a malformed ed25519 key")?;
            pk.verify_strict(data, &ed25519_dalek::Signature::from_bytes(&sig)).map_err(|_| bad())
        }
        b"ecdsa-sha2-nistp256" | b"ecdsa-sha2-nistp384" => {
            let (algorithm, width): (&dyn sig::VerificationAlgorithm, usize) = if kind == b"ecdsa-sha2-nistp256" {
                (&sig::ECDSA_P256_SHA256_FIXED, 32)
            } else {
                (&sig::ECDSA_P384_SHA384_FIXED, 48)
            };
            let (Some(_curve), Some(point)) = (key.string(), key.string()) else {
                return Err("a malformed ecdsa key".into());
            };
            let mut rs = Wire(bytes);
            let fixed = match (rs.string(), rs.string()) {
                (Some(r), Some(s)) if rs.done() && alg == kind && key.done() => {
                    let (r, s) = (fixed_width(r, width).ok_or_else(bad)?, fixed_width(s, width).ok_or_else(bad)?);
                    [r, s].concat()
                }
                _ => return Err(bad()),
            };
            sig::UnparsedPublicKey::new(algorithm, point).verify(data, &fixed).map_err(|_| bad())
        }
        b"ssh-rsa" => {
            let (Some(e), Some(n)) = (key.string(), key.string()) else { return Err("a malformed rsa key".into()) };
            let params = match alg {
                b"rsa-sha2-256" => &sig::RSA_PKCS1_2048_8192_SHA256,
                b"rsa-sha2-512" => &sig::RSA_PKCS1_2048_8192_SHA512,
                other => return Err(format!("`{}` signatures aren't checked", String::from_utf8_lossy(other))),
            };
            let strip = |m: &'_ [u8]| -> Vec<u8> { m.iter().skip_while(|&&b| b == 0).copied().collect() };
            if !key.done() {
                return Err("a malformed rsa key".into());
            }
            sig::RsaPublicKeyComponents { n: strip(n), e: strip(e) }.verify(params, data, bytes).map_err(|_| bad())
        }
        other => Err(format!("`{}` host keys aren't checked", String::from_utf8_lossy(other))),
    }
}

/// An mpint as `width` big-endian bytes.
fn fixed_width(mpint: &[u8], width: usize) -> Option<Vec<u8>> {
    let digits: Vec<u8> = mpint.iter().skip_while(|&&b| b == 0).copied().collect();
    let pad = width.checked_sub(digits.len())?;
    Some([vec![0; pad], digits].concat())
}

/// One verified `session-bind@openssh.com`.
#[derive(Debug, Clone)]
struct Bind {
    hostkey: Vec<u8>,
    session_id: Vec<u8>,
    forwarding: bool,
}

/// What to do with one client request.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// Pass it to the agent, its answer back as is.
    Forward,
    /// Pass it to the agent, its answer back through [`Filter::identities`].
    Identities,
    /// Answer [`FAILURE`]; the reason goes to the user.
    Refuse(String),
}

/// One agent connection's filter.
pub struct Filter<K> {
    keys: Vec<SshKey>,
    git_hosts: Vec<String>,
    known: K,
    binds: Vec<Bind>,
    looked_up: HashMap<String, Result<Vec<Vec<u8>>, String>>,
}

impl<K: KnownHosts> Filter<K> {
    /// `keys`: the sandbox's `ssh-keys`; `git_hosts`: its claims' git hosts,
    /// for messages.
    pub fn new(keys: Vec<SshKey>, git_hosts: Vec<String>, known: K) -> Filter<K> {
        Filter { keys, git_hosts, known, binds: Vec::new(), looked_up: HashMap::new() }
    }

    /// `msg`: one request, its type byte first.
    pub fn request(&mut self, msg: &[u8]) -> Step {
        let mut w = Wire(msg);
        match w.u8() {
            Some(REQUEST_IDENTITIES) => Step::Identities,
            Some(SIGN_REQUEST) => self.sign(w),
            Some(EXTENSION) => self.extension(w),
            Some(t) => Step::Refuse(format!(
                "agent request {t} isn't relayed while a git-host claim applies: only listing keys, signing and session-bind are"
            )),
            None => Step::Refuse("an empty agent request".into()),
        }
    }

    /// The agent's answer to a key listing, keeping the named keys. Not an
    /// identities answer (a failure): as is; malformed: [`FAILURE`].
    pub fn identities(&self, answer: &[u8]) -> Vec<u8> {
        let mut w = Wire(answer);
        if w.u8() != Some(IDENTITIES_ANSWER) {
            return answer.to_vec();
        }
        let mut parse = || -> Option<Vec<(&[u8], &[u8])>> {
            let n = w.u32()?;
            let mut kept = Vec::new();
            for _ in 0..n {
                let (blob, comment) = (w.string()?, w.string()?);
                if self.named(blob).next().is_some() {
                    kept.push((blob, comment));
                }
            }
            w.done().then_some(kept)
        };
        let Some(kept) = parse() else { return vec![FAILURE] };
        let mut out = vec![IDENTITIES_ANSWER];
        out.extend_from_slice(&(kept.len() as u32).to_be_bytes());
        for (blob, comment) in kept {
            put_string(&mut out, blob);
            put_string(&mut out, comment);
        }
        out
    }

    fn named<'a>(&'a self, blob: &'a [u8]) -> impl Iterator<Item = &'a SshKey> + 'a {
        self.keys.iter().filter(move |k| k.key.matches(blob))
    }

    fn sign(&mut self, mut w: Wire) -> Step {
        let (Some(blob), Some(data), Some(_flags)) = (w.string(), w.string(), w.u32()) else {
            return Step::Refuse("a malformed sign request".into());
        };
        let fp = format!("SHA256:{}", fingerprint_b64(blob));
        let named: Vec<SshKey> = self.named(blob).cloned().collect();
        if named.is_empty() {
            return Step::Refuse(format!(
                "{fp} isn't in the sandbox's `ssh-keys`, and its git-host claim ({}) hides every other key",
                self.git_hosts.join(", ")
            ));
        }
        let anywhere = named.iter().any(|k| !k.signing_only && k.destinations.is_none());
        if let Some(namespace) = sshsig_namespace(data) {
            if anywhere || (namespace == GIT_NAMESPACE && named.iter().any(|k| k.signing_only)) {
                return Step::Forward;
            }
            if named.iter().any(|k| k.signing_only) {
                return Step::Refuse(format!(
                    "{fp} is `signing-only`: it signs in namespace `git`, not `{}`",
                    String::from_utf8_lossy(namespace)
                ));
            }
            return Step::Refuse(format!(
                "{fp} has `destinations`: it signs logins to them, not ssh signatures (name it `signing-only` too)"
            ));
        }
        if anywhere {
            return Step::Forward;
        }
        let destinations: Vec<String> =
            named.iter().filter(|k| !k.signing_only).flat_map(|k| k.destinations.iter().flatten().cloned()).collect();
        if destinations.is_empty() {
            return Step::Refuse(format!("{fp} is `signing-only`: it signs git's ssh signatures, never logins"));
        }
        self.login(blob, data, &destinations, &fp)
    }

    fn login(&mut self, blob: &[u8], data: &[u8], destinations: &[String], fp: &str) -> Step {
        let Some(bind) = self.binds.last().cloned() else {
            return Step::Refuse(format!(
                "{fp} signs only on a connection bound to its destinations' host keys (`session-bind@openssh.com`, OpenSSH 8.9+), and this one isn't"
            ));
        };
        if bind.forwarding {
            return Step::Refuse(format!("{fp}: the connection's last session-bind is for forwarding, not a login"));
        }
        let Some(auth) = parse_userauth(data) else {
            return Step::Refuse(format!("{fp} signs only ssh logins to its destinations, and this isn't one"));
        };
        if auth.session_id != bind.session_id {
            return Step::Refuse(format!("{fp}: the login's session id isn't the one the connection bound"));
        }
        if auth.key != blob || auth.hostkey.is_some_and(|h| h != bind.hostkey) {
            return Step::Refuse(format!("{fp}: the login names another key than the one it asks to sign with"));
        }
        let (mut missing, mut failed) = (Vec::new(), Vec::new());
        for host in destinations {
            match self.lookup(host) {
                Ok(keys) if keys.contains(&bind.hostkey) => return Step::Forward,
                Ok(keys) if keys.is_empty() => missing.push(format!("`{host}`")),
                Ok(_) => {}
                Err(e) => failed.push(e),
            }
        }
        if let Some(e) = failed.first() {
            return Step::Refuse(format!("{fp}: {e}"));
        }
        if !missing.is_empty() {
            return Step::Refuse(format!(
                "{fp}: {} not in this host's known_hosts: a destination's host key must be there (`ssh-keygen -F`) for the key to sign",
                missing.join(", ")
            ));
        }
        Step::Refuse(format!(
            "{fp} signs only for {}, and the connection is bound to another host's key",
            destinations.iter().map(|h| format!("`{h}`")).collect::<Vec<_>>().join(", ")
        ))
    }

    fn lookup(&mut self, host: &str) -> Result<Vec<Vec<u8>>, String> {
        if let Some(found) = self.looked_up.get(host) {
            return found.clone();
        }
        let found = self.known.keys(host);
        self.looked_up.insert(host.to_string(), found.clone());
        found
    }

    fn extension(&mut self, mut w: Wire) -> Step {
        let Some(name) = w.string() else { return Step::Refuse("a malformed agent extension".into()) };
        if name != SESSION_BIND {
            return Step::Refuse(format!(
                "agent extension `{}` isn't relayed while a git-host claim applies",
                String::from_utf8_lossy(name)
            ));
        }
        let (Some(hostkey), Some(session_id), Some(signature), Some(forwarding)) =
            (w.string(), w.string(), w.string(), w.bool())
        else {
            return Step::Refuse("a malformed session-bind".into());
        };
        if !w.done() {
            return Step::Refuse("a malformed session-bind".into());
        }
        // As OpenSSH's agent: a connection bound for a login is done binding.
        if self.binds.last().is_some_and(|b| !b.forwarding) {
            return Step::Refuse("a session-bind after one for a login".into());
        }
        if self.binds.len() >= MAX_BINDS {
            return Step::Refuse("too many session-binds on one connection".into());
        }
        if let Err(why) = verify(hostkey, session_id, signature) {
            return Step::Refuse(format!("a session-bind refused: {why}"));
        }
        self.binds.push(Bind { hostkey: hostkey.to_vec(), session_id: session_id.to_vec(), forwarding });
        Step::Forward
    }
}

/// An SSHSIG signed-data blob's namespace (`ssh-keygen -Y sign`'s
/// preamble), `None` for anything else.
fn sshsig_namespace(data: &[u8]) -> Option<&[u8]> {
    let mut w = Wire(data.strip_prefix(SSHSIG_MAGIC)?);
    let namespace = w.string()?;
    let (_reserved, _hash_alg, _hash) = (w.string()?, w.string()?, w.string()?);
    w.done().then_some(namespace)
}

struct Userauth<'a> {
    session_id: &'a [u8],
    key: &'a [u8],
    /// `publickey-hostbound-v00@openssh.com`'s server host key.
    hostkey: Option<&'a [u8]>,
}

/// A publickey `SSH_MSG_USERAUTH_REQUEST` as signed (RFC 4252 §7).
fn parse_userauth(data: &[u8]) -> Option<Userauth<'_>> {
    let mut w = Wire(data);
    let session_id = w.string()?;
    if w.u8()? != USERAUTH_REQUEST {
        return None;
    }
    let (_user, _service, method) = (w.string()?, w.string()?, w.string()?);
    let hostbound = match method {
        b"publickey" => false,
        b"publickey-hostbound-v00@openssh.com" => true,
        _ => return None,
    };
    if !w.bool()? {
        return None;
    }
    let (_alg, key) = (w.string()?, w.string()?);
    let hostkey = if hostbound { Some(w.string()?) } else { None };
    w.done().then_some(Userauth { session_id, key, hostkey })
}

/// One length-prefixed message; `None` at a clean end.
fn read_message(r: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let n = u32::from_be_bytes(len) as usize;
    if n == 0 || n > MAX_MESSAGE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "agent message length out of range"));
    }
    let mut msg = vec![0u8; n];
    r.read_exact(&mut msg)?;
    Ok(Some(msg))
}

fn write_message(w: &mut impl Write, msg: &[u8]) -> io::Result<()> {
    let mut out = (msg.len() as u32).to_be_bytes().to_vec();
    out.extend_from_slice(msg);
    w.write_all(&out)?;
    w.flush()
}

/// Serves `client`'s requests through `filter` until it hangs up: the
/// protocol is one answer per request, in order, so one at a time.
/// `report` gets each refusal's reason.
pub fn relay<K: KnownHosts>(
    client: &mut (impl Read + Write),
    agent: &mut (impl Read + Write),
    filter: &mut Filter<K>,
    report: &dyn Fn(String),
) -> io::Result<()> {
    fn ask(agent: &mut (impl Read + Write), msg: &[u8]) -> io::Result<Vec<u8>> {
        write_message(agent, msg)?;
        read_message(agent)?.ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "the agent hung up"))
    }
    while let Some(msg) = read_message(client)? {
        let answer = match filter.request(&msg) {
            Step::Forward => ask(agent, &msg)?,
            Step::Identities => filter.identities(&ask(agent, &msg)?),
            Step::Refuse(why) => {
                report(why);
                vec![FAILURE]
            }
        };
        write_message(client, &answer)?;
    }
    Ok(())
}

pub type Report = Arc<dyn Fn(String) + Send + Sync>;

/// A bridge's filter for an instance with a git-host claim: the sandbox's
/// `ssh-keys` are read from its config per connection, like the broker's
/// `access`, so an edit applies to the next connection without a rebuild.
pub struct Gate {
    git_hosts: Vec<String>,
    config_dir: Option<PathBuf>,
    sandbox: String,
    report: Report,
}

impl Gate {
    /// `None` without a git-host claim: the agent passes as is.
    pub fn for_instance(info: &Instance, report: Report) -> Option<Gate> {
        let git_hosts = crate::credentials::instance::git_hosts(&info.claims);
        (!git_hosts.is_empty()).then(|| Gate {
            git_hosts,
            config_dir: info.config_dir.clone(),
            sandbox: info.sandbox.clone(),
            report,
        })
    }

    fn keys(&self) -> Result<Vec<SshKey>> {
        let dir = self.config_dir.as_ref().context("the instance was recorded without its config root: rebuild it")?;
        let resolved = Config::load(dir)?.resolve_sandbox(&self.sandbox)?;
        Ok(resolved.properties.ssh_keys.unwrap_or_default())
    }

    /// One agent connection, on its own thread, until either end hangs up.
    pub fn serve(&self, client: UnixStream, agent: UnixStream) {
        let keys = self.keys().unwrap_or_else(|e| {
            (self.report)(format!("ssh-agent: offering no keys, the sandbox's `ssh-keys` didn't load: {e:#}"));
            Vec::new()
        });
        let mut filter = Filter::new(keys, self.git_hosts.clone(), SshKeygen::for_host_user());
        let report = |why: String| (self.report)(format!("ssh-agent: {why}"));
        let _ = relay(&mut &client, &mut &agent, &mut filter, &report);
    }
}

#[cfg(test)]
mod tests;
