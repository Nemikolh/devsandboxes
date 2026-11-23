//! Permission requests (credential-broker, *Permission requests*): a call
//! policy answers with [`Action::Ask`](super::policy::Action::Ask) is held
//! on its connection's thread while a request waits in the daemon's one
//! [`Registry`] for an answer. Requests are keyed by [`AskKey`]: a call
//! with the key of an unanswered request joins it, so a retry never opens
//! a second one. An instance with [`MAX_OPEN_SCOPE_ASKS`] scope requests
//! (or [`MAX_OPEN_PRESENCE_ASKS`] presence ones) still held is refused at
//! once, so volume can't wear an approver down. A presence request
//! ([`AskKey::presence`]) is never answered *always*. A
//! request no call waits on any more stays listed for [`LATE_GRANT`]: a
//! `once` answer then becomes a one-shot grant for the instance's next
//! matching call ([`Registry::granted`]). *This instance* and deny answers
//! are kept in `access.json` ([`answers`]), *always* answers as a rule on
//! the credential in `credentials.toml` ([`home::Rule`]), before the
//! waiters wake. In the container, `devsbd access wait` waits on its
//! instance's request without holding a call ([`serve_wait`]).

use std::cell::RefCell;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, MutexGuard, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::refusal::Refusal;
use crate::credentials::answers::{self, Kind, Remembered};
use crate::credentials::home::{self, Rule};
use crate::devsbd::control::{self, CROCKFORD, Op, Response, Status};

/// A claim without `hold`'s.
pub const DEFAULT_HOLD: Duration = Duration::from_secs(120);
/// How long a request outlives its hold, answerable, and how long the
/// one-shot grant a late `once` leaves lasts.
pub const LATE_GRANT: Duration = Duration::from_secs(300);
/// Held scope requests per instance; a new one past it is refused.
pub const MAX_OPEN_SCOPE_ASKS: usize = 3;
/// Held presence requests per instance, counted apart from scope ones.
pub const MAX_OPEN_PRESENCE_ASKS: usize = 1;

/// What one answer covers.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AskKey {
    pub instance_id: String,
    pub credential: String,
    pub scope: String,
    /// The repo a repo-attributed scope's call names (lowercase
    /// `owner/name`); `None` for a claim without a repo fence.
    pub repo: Option<String>,
    /// A `presence` call's: asked even when its scope is allowed, and
    /// answered for this one route and target only.
    pub presence: Option<PresenceAt>,
}

/// What a presence answer covers beside its scope.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresenceAt {
    /// `<METHOD> <path>` as the call sent it (query dropped), or
    /// `graphql mutation <name>`.
    pub route: String,
    /// The ref or tag it acts on, ids resolved by the home.
    pub target: String,
}

/// [`Registry::answer`]'s error for an *always* answer to a presence
/// request: a client's mistake, not the registry's.
#[derive(Debug)]
pub struct NotAlways(pub String);

impl std::fmt::Display for NotAlways {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NotAlways {}

/// A call policy holds for an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    pub key: AskKey,
    /// The instance's key in `state.toml`.
    pub instance: String,
    /// The instance's sandbox: what an *always* answer's rule names.
    pub sandbox: String,
    pub kind: String,
    pub method: String,
    /// The path, query dropped.
    pub route: String,
    pub hold: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// The held calls go through, each once.
    Once,
    /// The instance's matching calls go through until its `rm`.
    Instance,
    /// A rule on the credential: matching calls of the sandbox go through.
    Always,
    /// The instance's matching calls are refused until revoked.
    Deny,
}

impl Answer {
    /// The `access.log` `decided` marker.
    pub fn marker(self) -> &'static str {
        match self {
            Answer::Once => "once",
            Answer::Instance => "instance",
            Answer::Always => "always",
            Answer::Deny => "deny",
        }
    }

    pub fn allows(self) -> bool {
        self != Answer::Deny
    }

    /// The answer [`Answer::marker`] names: what `access.decide` takes.
    pub fn parse(s: &str) -> Option<Answer> {
        [Answer::Once, Answer::Instance, Answer::Always, Answer::Deny].into_iter().find(|a| a.marker() == s)
    }
}

/// A listed request, as approvers see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// `R-` and 12 Crockford base32 characters.
    pub id: String,
    pub instance: String,
    pub sandbox: String,
    pub key: AskKey,
    pub kind: String,
    pub method: String,
    pub route: String,
    pub opened_at: Instant,
    /// Unix seconds.
    pub opened_unix: u64,
    /// The latest of its waiters' holds; past it, the request waits only
    /// for a late answer.
    pub held_until: Instant,
    /// Calls held on it now.
    pub waiting: usize,
}

/// One call's place on a request; [`Registry::wait`] must be called with
/// it, as it counts among the request's waiters until then.
#[derive(Debug)]
pub struct Ticket {
    id: String,
    deadline: Instant,
    opened: bool,
}

impl Ticket {
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The call opened its request rather than joining one: what raises
    /// the desktop popup, once per request.
    pub fn opened(&self) -> bool {
        self.opened
    }
}

struct Entry {
    request: Request,
    /// Set while answered waiters are still to wake; never listed then.
    answer: Option<Answer>,
}

impl Entry {
    /// Whether the request is still answerable (or still to wake its
    /// answered waiters) at `now`.
    fn live(&self, now: Instant) -> bool {
        self.answer.is_some() || self.request.waiting > 0 || now < self.request.held_until + LATE_GRANT
    }
}

/// An answered request's answer, for [`Registry::wait_answer`] callers
/// that come after its waiters woke (or when it had none).
struct Decided {
    id: String,
    instance_id: String,
    answer: Answer,
    until: Instant,
}

#[derive(Default)]
struct Inner {
    entries: Vec<Entry>,
    /// Late `once` answers, each until its expiry.
    late: Vec<(AskKey, Instant)>,
    /// Every answer, for [`LATE_GRANT`].
    decided: Vec<Decided>,
}

/// What [`Registry::wait_answer`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Answered(Answer),
    /// Still unanswered at the deadline.
    Pending,
    /// No such request for the instance: never one, another instance's, or
    /// gone past its late window.
    Unknown,
}

type Hook = Box<dyn Fn() + Send + Sync>;

pub struct Registry {
    inner: Mutex<Inner>,
    answered: Condvar,
    clock: Box<dyn Fn() -> Instant + Send + Sync>,
    on_change: RwLock<Hook>,
    /// Where kept answers go: `access.json` and `credentials.toml`.
    data: PathBuf,
}

impl Registry {
    pub fn new(data: PathBuf) -> Registry {
        Registry::with_clock(data, Box::new(Instant::now))
    }

    pub fn with_clock(data: PathBuf, clock: Box<dyn Fn() -> Instant + Send + Sync>) -> Registry {
        Registry { inner: Mutex::default(), answered: Condvar::new(), clock, on_change: RwLock::new(Box::new(|| {})), data }
    }

    /// Called after every change [`Registry::pending`] would show, outside
    /// the registry's lock. A request outliving [`LATE_GRANT`] is dropped
    /// by the next call that locks the registry ([`Registry::prune`] when
    /// nothing else does).
    pub fn on_change(&self, hook: Hook) {
        *self.on_change.write().unwrap_or_else(|e| e.into_inner()) = hook;
    }

    fn changed(&self) {
        (self.on_change.read().unwrap_or_else(|e| e.into_inner()))();
    }

    /// The registry, its outlived requests dropped; `true` when one was,
    /// so the caller calls the hook once the guard is gone.
    fn lock(&self, now: Instant) -> (MutexGuard<'_, Inner>, bool) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let listed = g.entries.len();
        g.entries.retain(|e| e.live(now));
        g.late.retain(|(_, until)| now < *until);
        g.decided.retain(|d| now < d.until);
        let dropped = g.entries.len() < listed;
        (g, dropped)
    }

    fn changed_if(&self, dropped: bool) {
        if dropped {
            self.changed();
        }
    }

    /// Drops the requests that outlived [`LATE_GRANT`]: for a caller that
    /// polls, so a request nobody calls about still leaves the listing.
    pub fn prune(&self) {
        let (g, dropped) = self.lock((self.clock)());
        drop(g);
        self.changed_if(dropped);
    }

    /// The call's place on the unanswered request with its key, else on a
    /// new one; refused when that would be one too many held for the
    /// instance.
    pub fn ask(&self, ask: &Ask) -> Result<Ticket, Refusal> {
        let now = (self.clock)();
        let deadline = now + ask.hold;
        let (mut g, dropped) = self.lock(now);
        if let Some(e) = g.entries.iter_mut().find(|e| e.answer.is_none() && e.request.key == ask.key) {
            e.request.waiting += 1;
            e.request.held_until = e.request.held_until.max(deadline);
            let id = e.request.id.clone();
            drop(g);
            self.changed();
            return Ok(Ticket { id, deadline, opened: false });
        }
        let presence = ask.key.presence.is_some();
        let held: Vec<&Entry> = g
            .entries
            .iter()
            .filter(|e| {
                e.answer.is_none()
                    && e.request.key.instance_id == ask.key.instance_id
                    && e.request.key.presence.is_some() == presence
                    && now < e.request.held_until
            })
            .collect();
        let cap = if presence { MAX_OPEN_PRESENCE_ASKS } else { MAX_OPEN_SCOPE_ASKS };
        if held.len() >= cap {
            let refusal = pending(&held[0].request.id, format!("another request pending for this instance: {}", held[0].request.id));
            drop(g);
            self.changed_if(dropped);
            return Err(refusal);
        }
        let id = loop {
            let id = fresh_id().map_err(|why| Refusal::new(500, "error", why))?;
            if !g.entries.iter().any(|e| e.request.id == id) {
                break id;
            }
        };
        g.entries.push(Entry {
            request: Request {
                id: id.clone(),
                instance: ask.instance.clone(),
                sandbox: ask.sandbox.clone(),
                key: ask.key.clone(),
                kind: ask.kind.clone(),
                method: ask.method.clone(),
                route: ask.route.clone(),
                opened_at: now,
                opened_unix: crate::state::Instance::now(),
                held_until: deadline,
                waiting: 1,
            },
            answer: None,
        });
        drop(g);
        self.changed();
        Ok(Ticket { id, deadline, opened: true })
    }

    /// Blocks until the request is answered or the ticket's hold ends
    /// (`None`).
    pub fn wait(&self, ticket: Ticket) -> Option<Answer> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let answer = loop {
            let e = g.entries.iter().find(|e| e.request.id == ticket.id)?;
            if let Some(a) = e.answer {
                break Some(a);
            }
            let now = (self.clock)();
            if now >= ticket.deadline {
                break None;
            }
            g = self.answered.wait_timeout(g, ticket.deadline - now).unwrap_or_else(|e| e.into_inner()).0;
        };
        if let Some(i) = g.entries.iter().position(|e| e.request.id == ticket.id) {
            let e = &mut g.entries[i];
            e.request.waiting -= 1;
            if e.answer.is_some() && e.request.waiting == 0 {
                g.entries.remove(i);
            }
        }
        drop(g);
        self.changed();
        answer
    }

    /// Answers listed request `id`: keeps the answer ([`remember`]), then
    /// wakes its waiters; with none left, a `once` leaves a one-shot grant
    /// instead. `Ok(None)`: no such request. A failed write leaves the
    /// request unanswered; so does *always* to a presence request, a
    /// [`NotAlways`] error.
    pub fn answer(&self, id: &str, answer: Answer) -> Result<Option<Request>> {
        let now = (self.clock)();
        let (mut g, dropped) = self.lock(now);
        let Some(i) = g.entries.iter().position(|e| e.request.id == id && e.answer.is_none()) else {
            drop(g);
            self.changed_if(dropped);
            return Ok(None);
        };
        let request = g.entries[i].request.clone();
        if answer == Answer::Always && request.key.presence.is_some() {
            drop(g);
            self.changed_if(dropped);
            return Err(NotAlways(format!(
                "request {id} is a presence request (a call the claim has a person approve on each target): \
                 answer it once or for this instance, never always"
            ))
            .into());
        }
        // Under the lock: a second answer to the same request finds it
        // answered, never writes again.
        if let Err(e) = remember(&self.data, &request, answer) {
            drop(g);
            self.changed_if(dropped);
            return Err(e);
        }
        g.decided.push(Decided {
            id: request.id.clone(),
            instance_id: request.key.instance_id.clone(),
            answer,
            until: now + LATE_GRANT,
        });
        // Always: `wait_answer` callers aren't counted in `waiting`.
        self.answered.notify_all();
        if request.waiting > 0 {
            g.entries[i].answer = Some(answer);
        } else {
            g.entries.remove(i);
            if answer == Answer::Once {
                g.late.push((request.key.clone(), now + LATE_GRANT));
            }
        }
        drop(g);
        self.changed();
        Ok(Some(request))
    }

    /// Blocks until request `id` of instance `instance_id` is answered, or
    /// until `deadline` (on the registry's clock, [`Registry::now`]). Not a
    /// waiter of the request: a `once` answered meanwhile with no call held
    /// still leaves its one-shot grant, for the retry the waiting agent then
    /// makes. An answer reads for [`LATE_GRANT`] after it was given.
    pub fn wait_answer(&self, id: &str, instance_id: &str, deadline: Instant) -> Outcome {
        let (mut g, dropped) = self.lock((self.clock)());
        let outcome = loop {
            if let Some(d) = g.decided.iter().find(|d| d.id == id && d.instance_id == instance_id) {
                break Outcome::Answered(d.answer);
            }
            let now = (self.clock)();
            let found = g.entries.iter().find(|e| e.request.id == id && e.request.key.instance_id == instance_id);
            match found.filter(|e| e.live(now)) {
                None => break Outcome::Unknown,
                Some(Entry { answer: Some(a), .. }) => break Outcome::Answered(*a),
                Some(_) if now >= deadline => break Outcome::Pending,
                Some(_) => {}
            }
            g = self.answered.wait_timeout(g, deadline - now).unwrap_or_else(|e| e.into_inner()).0;
        };
        drop(g);
        self.changed_if(dropped);
        outcome
    }

    /// The registry's clock: what [`Registry::wait_answer`]'s deadline is on.
    pub fn now(&self) -> Instant {
        (self.clock)()
    }

    /// Takes a late `once`'s grant for `key`, if one is left.
    pub fn granted(&self, key: &AskKey) -> bool {
        let (mut g, dropped) = self.lock((self.clock)());
        let taken = g.late.iter().position(|(k, _)| k == key).map(|i| g.late.remove(i)).is_some();
        drop(g);
        self.changed_if(dropped);
        taken
    }

    /// Unanswered requests, oldest first.
    pub fn pending(&self) -> Vec<Request> {
        let (g, dropped) = self.lock((self.clock)());
        let listed = g.entries.iter().filter(|e| e.answer.is_none()).map(|e| e.request.clone()).collect();
        drop(g);
        self.changed_if(dropped);
        listed
    }
}

/// One call's grants across its re-decisions: the answers it was held
/// for, then the registry's late grants, each taken at most once.
pub struct Grants<'a> {
    registry: &'a Registry,
    held: RefCell<Vec<AskKey>>,
}

impl<'a> Grants<'a> {
    pub fn new(registry: &'a Registry) -> Grants<'a> {
        Grants { registry, held: RefCell::default() }
    }

    pub fn allow(&self, key: AskKey) {
        self.held.borrow_mut().push(key);
    }

    pub fn granted(&self, key: &AskKey) -> bool {
        if self.held.borrow().contains(key) {
            return true;
        }
        // Kept, so a call re-decided after another of its asks doesn't
        // need this one's grant twice.
        let late = self.registry.granted(key);
        if late {
            self.allow(key.clone());
        }
        late
    }
}

/// Writes what outlives the request: an *always* answer's rule on its
/// credential, a *this instance* or deny answer into `access.json`.
fn remember(data: &Path, request: &Request, answer: Answer) -> Result<()> {
    let key = &request.key;
    let kind = match answer {
        Answer::Once => return Ok(()),
        Answer::Always => {
            return home::update(&home::path(data), |h| {
                let entry = h.get_mut(&key.credential).with_context(|| format!("credential `{}` is gone", key.credential))?;
                entry.rules.push(Rule {
                    scope: key.scope.clone(),
                    repo: key.repo.clone(),
                    sandbox: Some(request.sandbox.clone()),
                    profile: None,
                    id: Some(request.id.clone()),
                });
                Ok(())
            });
        }
        Answer::Instance => Kind::Instance,
        Answer::Deny => Kind::Deny,
    };
    let kept = Remembered {
        id: request.id.clone(),
        instance_id: key.instance_id.clone(),
        instance: request.instance.clone(),
        credential: key.credential.clone(),
        scope: key.scope.clone(),
        repo: key.repo.clone(),
        presence: key.presence.clone(),
        answer: kind,
        at: crate::inbox::rfc3339(crate::state::Instance::now()),
    };
    answers::update(&answers::path(data), |a| answers::record(a, kept))
}

/// A `403` `denied` for a call matching request `id`'s kept deny.
pub fn denied(id: &str) -> Refusal {
    Refusal { request: Some(id.to_string()), ..Refusal::new(403, "denied", format!("denied for this instance (request {id})")) }
        .todo(format!("devsandbox access revoke {id}"))
}

/// A `403` `pending` naming request `id`.
pub fn pending(id: &str, reason: String) -> Refusal {
    Refusal { request: Some(id.to_string()), ..Refusal::new(403, "pending", reason) }
        .todo(format!("devsandbox access approve {id}"))
}

/// Serves `access-wait` request `req` from instance `instance_id` (its
/// bridge's, whatever the container says) on `out`, the bridge's control
/// stream: a [`control::FOLLOW_PING_LINE`] every `ping` while
/// [`Registry::wait_answer`] waits, then one response (`control`'s module
/// doc). No dispatcher or Inbox check: the instance only ever reaches its
/// own requests, and an id it doesn't own reads as `unknown`. Without a
/// registry (no broker) no request exists. Ends early when a ping can't be
/// written: the client left.
pub fn serve_wait(asks: Option<&Registry>, instance_id: &str, req: &control::Request, out: &mut dyn Write, ping: Duration) {
    let resp = match wait_params(req) {
        Err(why) => Response::new(Status::Usage, why),
        Ok((id, timeout)) => {
            let outcome = match asks {
                None => Outcome::Unknown,
                Some(asks) => {
                    let deadline = asks.now() + timeout;
                    loop {
                        let step = deadline.min(asks.now() + ping);
                        match asks.wait_answer(id, instance_id, step) {
                            Outcome::Pending if step < deadline => {
                                let line = format!("{}\n", control::FOLLOW_PING_LINE);
                                if out.write_all(line.as_bytes()).and_then(|()| out.flush()).is_err() {
                                    return;
                                }
                            }
                            outcome => break outcome,
                        }
                    }
                }
            };
            let body = match outcome {
                Outcome::Answered(a) => a.marker(),
                Outcome::Pending => control::ACCESS_PENDING,
                Outcome::Unknown => control::ACCESS_UNKNOWN,
            };
            Response::new(Status::Ok, body)
        }
    };
    let _ = out.write_all(control::encode_response(&resp).as_bytes());
}

/// An `access-wait` request's id and timeout; it takes nothing else.
fn wait_params(req: &control::Request) -> Result<(&str, Duration), String> {
    let Some(id) = req.id.as_deref() else { return Err("`access-wait` needs `id`".into()) };
    if !control::valid_request_id(id) {
        return Err(format!("bad request id `{id}`"));
    }
    let bare = control::Request { id: req.id.clone(), timeout: req.timeout, ..control::Request::new(Op::AccessWait) };
    if *req != bare {
        return Err("`access-wait` takes only `id` and `timeout`".into());
    }
    let secs = req.timeout.unwrap_or(control::MAX_WAIT).min(control::MAX_WAIT);
    Ok((id, Duration::from_secs(secs)))
}

/// The shape of [`fresh_id`]'s ids, shared with the helper.
pub fn is_request_id(s: &str) -> bool {
    control::valid_request_id(s)
}

/// `R-` and 60 random bits: unguessable to another instance, and never a
/// credential name, which has no uppercase letter.
fn fresh_id() -> Result<String, String> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).map_err(|e| format!("cannot read the OS random source: {e}"))?;
    let bits = u64::from_le_bytes(bytes);
    let chars: String = (0..12).map(|i| CROCKFORD[((bits >> (i * 5)) & 31) as usize] as char).collect();
    Ok(format!("R-{chars}"))
}

#[cfg(test)]
mod tests;
