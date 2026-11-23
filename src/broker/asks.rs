//! Permission requests (credential-broker, *Permission requests*): a call
//! policy answers with [`Action::Ask`](super::policy::Action::Ask) is held
//! on its connection's thread while a request waits in the daemon's one
//! [`Registry`] for an answer. Requests are keyed by [`AskKey`]: a call
//! with the key of an unanswered request joins it, so a retry never opens
//! a second one. An instance with [`MAX_OPEN_SCOPE_ASKS`] requests still
//! held is refused at once, so volume can't wear an approver down. A
//! request no call waits on any more stays listed for [`LATE_GRANT`]: a
//! `once` answer then becomes a one-shot grant for the instance's next
//! matching call ([`Registry::granted`]).

// Answering and listing have no caller but tests until the API serves them.
#![cfg_attr(not(test), allow(dead_code))]

use std::cell::RefCell;
use std::sync::{Condvar, Mutex, MutexGuard, RwLock};
use std::time::{Duration, Instant};

use super::refusal::Refusal;

/// A claim without `hold`'s.
pub const DEFAULT_HOLD: Duration = Duration::from_secs(120);
/// How long a request outlives its hold, answerable, and how long the
/// one-shot grant a late `once` leaves lasts.
pub const LATE_GRANT: Duration = Duration::from_secs(300);
/// Held scope requests per instance; a new one past it is refused.
pub const MAX_OPEN_SCOPE_ASKS: usize = 3;

/// What one answer covers.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AskKey {
    pub instance_id: String,
    pub credential: String,
    pub scope: String,
    /// The repo a repo-attributed scope's call names (lowercase
    /// `owner/name`); `None` for a claim without a repo fence.
    pub repo: Option<String>,
}

/// A call policy holds for an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    pub key: AskKey,
    /// The instance's key in `state.toml`.
    pub instance: String,
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
    Deny,
}

impl Answer {
    /// The `access.log` `decided` marker.
    pub fn marker(self) -> &'static str {
        match self {
            Answer::Once => "once",
            Answer::Deny => "deny",
        }
    }
}

/// A listed request, as approvers see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// `R-` and 12 Crockford base32 characters.
    pub id: String,
    pub instance: String,
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
}

impl Ticket {
    pub fn id(&self) -> &str {
        &self.id
    }
}

struct Entry {
    request: Request,
    /// Set while answered waiters are still to wake; never listed then.
    answer: Option<Answer>,
}

#[derive(Default)]
struct Inner {
    entries: Vec<Entry>,
    /// Late `once` answers, each until its expiry.
    late: Vec<(AskKey, Instant)>,
}

type Hook = Box<dyn Fn() + Send + Sync>;

pub struct Registry {
    inner: Mutex<Inner>,
    answered: Condvar,
    clock: Box<dyn Fn() -> Instant + Send + Sync>,
    on_change: RwLock<Hook>,
}

impl Default for Registry {
    fn default() -> Registry {
        Registry::with_clock(Box::new(Instant::now))
    }
}

impl Registry {
    pub fn with_clock(clock: Box<dyn Fn() -> Instant + Send + Sync>) -> Registry {
        Registry { inner: Mutex::default(), answered: Condvar::new(), clock, on_change: RwLock::new(Box::new(|| {})) }
    }

    /// Called after every change [`Registry::pending`] would show (pruning
    /// aside), outside the registry's lock.
    pub fn on_change(&self, hook: Hook) {
        *self.on_change.write().unwrap_or_else(|e| e.into_inner()) = hook;
    }

    fn changed(&self) {
        (self.on_change.read().unwrap_or_else(|e| e.into_inner()))();
    }

    fn lock(&self, now: Instant) -> MutexGuard<'_, Inner> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.entries.retain(|e| e.answer.is_some() || e.request.waiting > 0 || now < e.request.held_until + LATE_GRANT);
        g.late.retain(|(_, until)| now < *until);
        g
    }

    /// The call's place on the unanswered request with its key, else on a
    /// new one; refused when that would be one too many held for the
    /// instance.
    pub fn ask(&self, ask: &Ask) -> Result<Ticket, Refusal> {
        let now = (self.clock)();
        let deadline = now + ask.hold;
        let mut g = self.lock(now);
        if let Some(e) = g.entries.iter_mut().find(|e| e.answer.is_none() && e.request.key == ask.key) {
            e.request.waiting += 1;
            e.request.held_until = e.request.held_until.max(deadline);
            let id = e.request.id.clone();
            drop(g);
            self.changed();
            return Ok(Ticket { id, deadline });
        }
        let held: Vec<&Entry> = g
            .entries
            .iter()
            .filter(|e| e.answer.is_none() && e.request.key.instance_id == ask.key.instance_id && now < e.request.held_until)
            .collect();
        if held.len() >= MAX_OPEN_SCOPE_ASKS {
            return Err(pending(&held[0].request.id, format!("another request pending for this instance: {}", held[0].request.id)));
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
        Ok(Ticket { id, deadline })
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

    /// Answers listed request `id`, waking its waiters; with none left, a
    /// `once` leaves a one-shot grant instead. `None`: no such request.
    pub fn answer(&self, id: &str, answer: Answer) -> Option<Request> {
        let now = (self.clock)();
        let mut g = self.lock(now);
        let i = g.entries.iter().position(|e| e.request.id == id && e.answer.is_none())?;
        let request = g.entries[i].request.clone();
        if request.waiting > 0 {
            g.entries[i].answer = Some(answer);
            self.answered.notify_all();
        } else {
            g.entries.remove(i);
            if answer == Answer::Once {
                g.late.push((request.key.clone(), now + LATE_GRANT));
            }
        }
        drop(g);
        self.changed();
        Some(request)
    }

    /// Takes a late `once`'s grant for `key`, if one is left.
    pub fn granted(&self, key: &AskKey) -> bool {
        let mut g = self.lock((self.clock)());
        let Some(i) = g.late.iter().position(|(k, _)| k == key) else { return false };
        g.late.remove(i);
        true
    }

    /// Unanswered requests, oldest first.
    pub fn pending(&self) -> Vec<Request> {
        let g = self.lock((self.clock)());
        g.entries.iter().filter(|e| e.answer.is_none()).map(|e| e.request.clone()).collect()
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

/// A `403` `pending` naming request `id`.
pub fn pending(id: &str, reason: String) -> Refusal {
    Refusal { request: Some(id.to_string()), ..Refusal::new(403, "pending", reason) }
        .todo(format!("devsandbox access approve {id}"))
}

const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

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
