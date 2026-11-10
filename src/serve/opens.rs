//! Which host opens a container's URL (docs/architecture/daemon/fleet.md,
//! *Opening URLs*): the instance's host's opener, routing each admitted
//! `open` record to where the user last attached from, and the receiving
//! member's `open.deliver` check.
//!
//! - **Sending** ([`opener`], the bridges' `bridge::Opener`): on a thread of
//!   its own, the record becomes an `open::OpenJob` (`open::prepare`: the
//!   profile to send), then [`route`] over the instance's latest attach
//!   (`serve::attaches`): this host ([`Route::Here`]) opens it with
//!   `open::open_here`; a member ([`Route::Member`]) gets `open.deliver`
//!   ([`DeliverParams`]). A failed delivery is a `serve.log` line, never a
//!   local open: the user is at that member, not here.
//! - **Receiving** ([`receive`], the daemon's `open.deliver`, from a fleet
//!   peer only): strict params, the same `open::decide` policy again (with a
//!   per-`(peer, instance)` limit), and only for an instance this host
//!   attached to itself on that peer within `open::OPEN_ATTACH_WINDOW`, so
//!   no member can drive this desktop by naming an instance it was never
//!   asked to open. Only the profile's *name* comes over: the command is
//!   this host's `profiles.toml`'s.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::api::ApiError;
use super::attaches::Origin;
use super::forwards;
use crate::devsbd::bridge::Opener;
use crate::open::{self, ForwardTarget, Forwarder, Loopback, OpenJob};

/// Opens a job on this host (`open::open_here` on a thread of its own, in
/// production); a seam so the daemon's tests record the job instead.
pub type OpenHere = Arc<dyn Fn(OpenJob) + Send + Sync>;

/// `method` with `params` on fleet member `name`: its result, or why not.
pub type CallMember = Arc<dyn Fn(&str, &str, Value) -> Result<Value, String> + Send + Sync>;

/// `open.deliver`'s params: the instance by its current name and config
/// root on the sending host (the receiver's loopback forwards go into it),
/// the URL as the container gave it, and the profile name the sender chose
/// (`open::profile_to_send`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliverParams {
    pub instance: String,
    pub root: PathBuf,
    pub url: String,
    pub profile: Option<String>,
}

/// Where an instance's open goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    Here,
    Member(String),
}

/// The last attach decides: none yet, or this host's own, opens here; a
/// member's goes to that member.
pub fn route(latest: Option<Origin>) -> Route {
    match latest {
        None | Some(Origin::Here) => Route::Here,
        Some(Origin::Member(m)) => Route::Member(m),
    }
}

/// What the sending side runs on.
#[derive(Clone)]
pub struct Sender {
    /// `(instance, instance_id, url)` → the job (`open::prepare`).
    pub prepare: Arc<dyn Fn(&str, &str, &str) -> OpenJob + Send + Sync>,
    /// The latest attach to an instance id (`Attaches::latest`).
    pub latest: Arc<dyn Fn(&str) -> Option<Origin> + Send + Sync>,
    pub call_member: CallMember,
    pub open_here: OpenHere,
    pub log: fn(&str),
}

/// The bridges' opener: [`send`] on a thread of its own per record, so the
/// bridge's handler never waits on state, a link or a browser.
pub fn opener(sender: Sender) -> Opener {
    Arc::new(move |instance: &str, instance_id: &str, url: &str| {
        let log = sender.log;
        let (sender, instance, instance_id, url) = (sender.clone(), instance.to_string(), instance_id.to_string(), url.to_string());
        let spawned = std::thread::Builder::new()
            .name("serve-open".into())
            .spawn(move || send(&sender, &instance, &instance_id, &url));
        if let Err(e) = spawned {
            log(&format!("cannot start an open's thread: {e}"));
        }
    })
}

/// One admitted `open` record from `instance` (`instance_id`): prepared,
/// routed, opened here or delivered.
pub fn send(s: &Sender, instance: &str, instance_id: &str, url: &str) {
    let job = (s.prepare)(instance, instance_id, url);
    match route((s.latest)(instance_id)) {
        Route::Here => (s.open_here)(job),
        Route::Member(m) => {
            let delivered = deliver_params(&job).and_then(|p| {
                let params = serde_json::to_value(p).map_err(|e| e.to_string())?;
                (s.call_member)(&m, "open.deliver", params)
            });
            if let Err(why) = delivered {
                (s.log)(&format!("{instance}: cannot deliver {url} to {m}: {why}"));
            }
        }
    }
}

/// `open.deliver`'s params for `job` (its target is the local instance:
/// its state key and config root), or why it can't go.
fn deliver_params(job: &OpenJob) -> Result<DeliverParams, String> {
    let target = job.target.as_ref().map_err(Clone::clone)?;
    Ok(DeliverParams { instance: target.instance.clone(), root: target.dir.clone(), url: job.url.clone(), profile: job.profile.clone() })
}

/// The longest `open.deliver` URL taken: an `open` record's own cap.
const MAX_URL: usize = crate::devsbd::notify::MAX_RECORD;

/// The longest instance name taken.
const MAX_INSTANCE: usize = 256;

/// The longest profile name taken.
const MAX_PROFILE: usize = 64;

/// `open.deliver` `params` from fleet peer `peer`, checked in order: the
/// params (strict: a `machine` or any other key is refused), `attached`
/// (whether this host attached to `instance` on `peer` within the window),
/// then `open::decide` at `now` (unix seconds; `limit` spends this
/// `(peer, instance)`'s token). The job to open here: `instance` as `peer` named it
/// and its root there as the forward target.
pub fn receive(
    params: Value,
    peer: &str,
    now: u64,
    limit: impl FnOnce(&str) -> bool,
    attached: impl FnOnce(&str) -> bool,
) -> Result<OpenJob, ApiError> {
    let p: DeliverParams = super::api::parse(params)?;
    if p.instance.is_empty() || p.instance.len() > MAX_INSTANCE {
        return Err(ApiError::invalid(format!("`instance`: 1 to {MAX_INSTANCE} bytes")));
    }
    if p.url.len() > MAX_URL {
        return Err(ApiError::invalid(format!("`url`: at most {MAX_URL} bytes")));
    }
    if !p.root.is_absolute() {
        return Err(ApiError::invalid(format!("`root` must be absolute: {}", p.root.display())));
    }
    if let Some(profile) = &p.profile {
        if profile.len() > MAX_PROFILE {
            return Err(ApiError::invalid(format!("`profile`: at most {MAX_PROFILE} bytes")));
        }
        crate::config::check_profile_name(profile).map_err(|e| ApiError::invalid(format!("{e:#}")))?;
    }
    // Before the policy: a request for an instance this host never asked
    // about spends none of that instance's limit.
    if !attached(&p.instance) {
        return Err(ApiError::denied(format!(
            "`{peer}` asked to open {} for `{}`, which this host hasn't attached to in the last {} min",
            p.url,
            p.instance,
            open::OPEN_ATTACH_WINDOW.as_secs() / 60
        )));
    }
    let verdict = open::decide(&p.url, now, now, || limit(&p.instance));
    if let Some(line) = verdict.log_line(&p.instance, &p.url) {
        let code = if matches!(verdict, open::Verdict::BadScheme(_)) { "invalid" } else { "denied" };
        return Err(ApiError { code, message: line });
    }
    let target = ForwardTarget { machine: Some(peer.to_string()), dir: p.root, instance: p.instance.clone() };
    Ok(OpenJob { instance: p.instance, url: p.url, profile: p.profile, target: Ok(target) })
}

/// The opener's [`Forwarder`]: the registry's open forwards
/// (`forwards::Handle::open_forward`), an instance on another member's
/// `machine` handed through. A callback port the registry couldn't bind is
/// `Err(None)`: its status line already said which port blocked it.
pub fn forwarder(forwards: Option<forwards::Handle>) -> Forwarder {
    Arc::new(move |target: &ForwardTarget, lb: Loopback| {
        let Some(forwards) = &forwards else { return Err(Some("no forwards registry in this daemon".into())) };
        let req = forwards::OpenRequest {
            machine: target.machine.clone(),
            dir: target.dir.clone(),
            instance: target.instance.clone(),
            container_port: lb.port,
            callback: lb.callback,
        };
        forwards
            .open_forward(req)
            .map(|o| o.host_port)
            .map_err(|e| (!(lb.callback && e.code == "bind-failed")).then_some(e.message))
    })
}

/// The production [`OpenHere`]: `open::open_here` through `forward`, on a
/// thread of its own.
pub fn open_here(forward: Forwarder, log: fn(&str)) -> OpenHere {
    Arc::new(move |job: OpenJob| {
        let forward = Arc::clone(&forward);
        let spawned = std::thread::Builder::new().name("serve-open".into()).spawn(move || open::open_here(job, &forward, log));
        if let Err(e) = spawned {
            log(&format!("cannot start an open's thread: {e}"));
        }
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use serde_json::json;

    use super::*;

    #[test]
    fn the_last_attach_decides() {
        assert_eq!(route(None), Route::Here, "nothing attached yet");
        assert_eq!(route(Some(Origin::Here)), Route::Here);
        assert_eq!(route(Some(Origin::Member("lap".into()))), Route::Member("lap".into()));
    }

    fn job(target: Result<ForwardTarget, String>) -> OpenJob {
        OpenJob { instance: "web-1".into(), url: "http://localhost:3000/".into(), profile: Some("work".into()), target }
    }

    fn local() -> ForwardTarget {
        ForwardTarget { machine: None, dir: "/cfg".into(), instance: "web-1".into() }
    }

    type Calls = Arc<Mutex<Vec<(String, String, Value)>>>;

    /// A sender over fakes: `latest` is the instance's latest attach,
    /// `answer` every member call's. Records the calls and the local opens;
    /// log lines go to `LINES`.
    fn sender(
        target: Result<ForwardTarget, String>,
        latest: Option<Origin>,
        answer: Result<Value, String>,
    ) -> (Sender, Calls, Arc<Mutex<Vec<OpenJob>>>) {
        let (calls, opened): (Calls, Arc<Mutex<Vec<OpenJob>>>) = Default::default();
        let (c, o) = (Arc::clone(&calls), Arc::clone(&opened));
        let s = Sender {
            prepare: Arc::new(move |_, _, _| job(target.clone())),
            latest: Arc::new(move |id| {
                assert_eq!(id, "id-1");
                latest.clone()
            }),
            call_member: Arc::new(move |m, method, params| {
                c.lock().unwrap().push((m.into(), method.into(), params));
                answer.clone()
            }),
            open_here: Arc::new(move |j| o.lock().unwrap().push(j)),
            log: record,
        };
        (s, calls, opened)
    }

    static LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());

    fn record(line: &str) {
        LINES.lock().unwrap().push(line.to_string());
    }

    fn logged(needle: &str) -> bool {
        LINES.lock().unwrap().iter().any(|l| l.contains(needle))
    }

    #[test]
    fn an_open_with_no_member_attach_opens_here() {
        for latest in [None, Some(Origin::Here)] {
            let (s, calls, opened) = sender(Ok(local()), latest, Ok(json!({"ok": true})));
            send(&s, "web-1", "id-1", "http://localhost:3000/");
            assert_eq!(*opened.lock().unwrap(), vec![job(Ok(local()))]);
            assert!(calls.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn an_open_after_a_member_attach_is_delivered_there() {
        let (s, calls, opened) = sender(Ok(local()), Some(Origin::Member("lap".into())), Ok(json!({"ok": true})));
        send(&s, "web-1", "id-1", "http://localhost:3000/");
        let want = json!({"instance": "web-1", "root": "/cfg", "url": "http://localhost:3000/", "profile": "work"});
        assert_eq!(*calls.lock().unwrap(), vec![("lap".to_string(), "open.deliver".to_string(), want)]);
        assert!(opened.lock().unwrap().is_empty());
    }

    #[test]
    fn a_failed_delivery_is_logged_and_never_opens_here() {
        let (s, _, opened) =
            sender(Ok(local()), Some(Origin::Member("lap".into())), Err("open.deliver: nope (denied)".into()));
        send(&s, "web-1", "id-1", "https://a.example/");
        assert!(logged("web-1: cannot deliver https://a.example/ to lap: open.deliver: nope (denied)"));
        assert!(opened.lock().unwrap().is_empty(), "no fallback");

        let (s, calls, opened) = sender(Err("not in state.toml".into()), Some(Origin::Member("lap".into())), Ok(json!({})));
        send(&s, "gone", "id-1", "https://b.example/");
        assert!(logged("gone: cannot deliver https://b.example/ to lap: not in state.toml"));
        assert!(calls.lock().unwrap().is_empty() && opened.lock().unwrap().is_empty());
    }

    fn params() -> Value {
        json!({"instance": "web-1", "root": "/home/u/cfg", "url": "http://localhost:3000/", "profile": "work"})
    }

    fn with_none() -> Value {
        Value::Null
    }

    fn take(params: Value) -> Result<OpenJob, ApiError> {
        receive(params, "box", 1_790_000_000, |_| true, |i| i == "web-1")
    }

    #[test]
    fn a_delivery_for_an_attached_instance_is_taken() {
        let target = ForwardTarget { machine: Some("box".into()), dir: "/home/u/cfg".into(), instance: "web-1".into() };
        assert_eq!(
            take(params()).unwrap(),
            OpenJob {
                instance: "web-1".into(),
                url: "http://localhost:3000/".into(),
                profile: Some("work".into()),
                target: Ok(target)
            }
        );
        let mut p = params();
        p["profile"] = Value::Null;
        assert_eq!(take(p).unwrap().profile, None);
    }

    #[test]
    fn a_delivery_is_checked_in_order() {
        let code = |p: Value| take(p).unwrap_err().code;
        assert_eq!(code(with_none()), "invalid", "no params");
        let with = |k: &str, v: Value| {
            let mut p = params();
            p[k] = v;
            p
        };
        assert_eq!(code(with("machine", json!("lap"))), "invalid", "strict: no `machine`");
        assert_eq!(code(with("extra", json!(1))), "invalid");
        assert_eq!(code(json!({"instance": "web-1"})), "invalid");
        assert_eq!(code(with("instance", json!(""))), "invalid");
        assert_eq!(code(with("root", json!("rel"))), "invalid");
        assert_eq!(code(with("profile", json!("Work Profile"))), "invalid");
        assert_eq!(code(with("profile", json!("x".repeat(65)))), "invalid");
        assert_eq!(code(with("url", json!(format!("https://x/{}", "a".repeat(MAX_URL))))), "invalid");
        let e = take(with("url", json!("ssh://x"))).unwrap_err();
        assert_eq!((e.code, e.message.as_str()), ("invalid", "web-1: open refused, scheme `ssh` (http and https only): ssh://x"));
        let e = receive(params(), "box", 1, |_| false, |_| true).unwrap_err();
        assert_eq!(e.code, "denied", "rate-limited: {}", e.message);
        let e = take(with("instance", json!("web-2"))).unwrap_err();
        assert_eq!(
            (e.code, e.message.as_str()),
            ("denied", "`box` asked to open http://localhost:3000/ for `web-2`, which this host hasn't attached to in the last 10 min")
        );
        // The limit isn't spent on a refused scheme, nor on an instance
        // never attached to.
        receive(with("url", json!("ssh://x")), "box", 1, |_| panic!("not asked"), |_| true).unwrap_err();
        receive(params(), "box", 1, |_| panic!("not asked"), |_| false).unwrap_err();
    }
}
