//! The lent API (docs/architecture/daemon/fleet.md, *The Worker table*):
//! what a fleet hub may do on this worker over its link's `API` streams.
//! Each such connection is served with a [`Scope`] (the daemon's
//! `Table::Worker`); every request on it goes through [`admit`] first and
//! its answer through [`filter_answer`], every `instances.snapshot` pushed
//! to it through [`filter_snapshot`].
//!
//! - [`decide`] (pure, over the method and params): hello, `subscribe` /
//!   `unsubscribe` to `instances` only, `instances.watch` / `unwatch`, the
//!   quick calls (`machines::forward::QUICK`), `capacity.get` and the verbs; never a
//!   `machine` param or a federated (`machines: true`) request, never a
//!   verb's `all`; everything else (`machines.*`, `fleet.*`, `lend.*`, `shutdown`,
//!   `helper.call`, `forwards.*`, the Inbox, `roots.list`, …) is `denied`.
//! - [`admit`] then confines the params: every `root` / `dir` canonicalized
//!   and under the fleet root (rewritten to its canonical form), every
//!   instance or container named an instance whose `config_dir` is under
//!   the fleet root, and a verb's `env` replaced by this host's.
//! - [`refuse_new_work`]: while lending `drain`, a `run` is `denied`
//!   ([`DRAINING`]); verbs on instances already here go on.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use crate::serve::api::ApiError;
use crate::serve::machines::forward as fwd;
use crate::serve::verbs;
use crate::state::State;

/// A hub's view of this host: one per lending link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    /// The hub's ssh target, for the log.
    pub hub: String,
    /// The fleet root, canonical: all the hub reaches.
    pub root: PathBuf,
    /// The env a hub's verbs run with: this host's own (the daemon's), never
    /// what the hub sent.
    pub env: BTreeMap<String, String>,
}

/// What kind of request [`decide`] let through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    Hello,
    Subscribe,
    Watch,
    Unwatch,
    Quick,
    /// `capacity.get`: this host's report, for the hub's placement.
    Capacity,
    Verb,
}

/// The refusal of a hub's `run` while this host drains.
pub const DRAINING: &str = "this worker is draining: no new work";

/// `drain` is the one hard no for new work (pure): a `run` while
/// `draining` is `denied` with [`DRAINING`]; everything else (`start`,
/// `stop`, `rm`, `rebuild` of what's placed here) is left to [`admit`].
pub fn refuse_new_work(method: &str, draining: bool) -> Result<(), ApiError> {
    if draining && method == "run" {
        return Err(ApiError::denied(DRAINING));
    }
    Ok(())
}

/// Whether a hub may send `method` with `params` at all (pure).
pub fn decide(method: &str, params: &Value) -> Result<Gate, ApiError> {
    if fwd::machine_of(params)?.is_some() {
        return Err(ApiError::denied("a fleet hub can't reach past this worker (`machine`)"));
    }
    if fwd::federated(params) {
        return Err(ApiError::denied("a fleet hub can't reach past this worker (`machines`)"));
    }
    let gate = match method {
        "hello" => Gate::Hello,
        "subscribe" | "unsubscribe" => {
            let topics = params.get("topics").and_then(Value::as_array);
            if topics.is_some_and(|t| t.iter().any(|t| t.as_str() != Some("instances"))) {
                return Err(ApiError::denied("a fleet hub subscribes to `instances` only"));
            }
            Gate::Subscribe
        }
        "instances.watch" => Gate::Watch,
        "instances.unwatch" => Gate::Unwatch,
        m if fwd::is_quick(m) => Gate::Quick,
        "capacity.get" => Gate::Capacity,
        m if verbs::is_verb(m) => {
            // `--all` is every instance in `state.toml`, not the root's.
            if params.get("all").and_then(Value::as_bool) == Some(true) {
                return Err(ApiError::denied(format!("`{m}` with `all` isn't lent to a fleet hub")));
            }
            Gate::Verb
        }
        m => return Err(ApiError::denied(format!("`{m}` isn't lent to a fleet hub"))),
    };
    Ok(gate)
}

/// The params naming an instance (an exact name or id, as the quick calls
/// take it), per method.
fn instance_keys(method: &str) -> &'static [&'static str] {
    match method {
        "instances.cancel" | "instances.signal" | "instances.rename" | "instances.done" | "instances.buildLog"
        | "sessions.list" | "sessions.rm" => &["instance"],
        "rebuild" | "start" | "stop" | "rm" => &["instance"],
        _ => &[],
    }
}

/// The verbs that act on an existing instance, which a hub must name.
fn needs_instance(method: &str) -> bool {
    matches!(method, "rebuild" | "start" | "stop" | "rm")
}

/// `path` checked against fleet root `root` (canonical): absolute, no `..`,
/// and canonicalized (symlinks resolved) under `root`. A path that doesn't
/// exist is `not-found`, unless `!exists` (an unwatch of a removed root),
/// where it is checked as written.
pub fn confine_path(root: &Path, path: &Path, exists: bool) -> Result<PathBuf, ApiError> {
    if !path.is_absolute() {
        return Err(ApiError::invalid(format!("`{}` must be absolute", path.display())));
    }
    if path.components().any(|c| c == Component::ParentDir) {
        return Err(ApiError::denied(format!("`{}` has a `..`", path.display())));
    }
    let real = match std::fs::canonicalize(path) {
        Ok(real) => real,
        Err(_) if !exists => path.to_path_buf(),
        Err(_) => return Err(ApiError::not_found(format!("no directory {}", path.display()))),
    };
    if !real.starts_with(root) {
        return Err(ApiError::denied(format!("`{}` is outside this worker's fleet root", path.display())));
    }
    Ok(real)
}

/// Whether recorded config root `dir` is under `root`: canonicalized when it
/// still exists, else as recorded; never with a `..`.
pub(crate) fn root_under(dir: &Path, root: &Path) -> bool {
    if dir.components().any(|c| c == Component::ParentDir) {
        return false;
    }
    std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf()).starts_with(root)
}

/// The instances of `state` the hub may name: names and ids of those whose
/// `config_dir` is under `root`.
pub fn lent_instances(state: &State, root: &Path) -> Lent {
    let mut lent = Lent::default();
    for (name, i) in &state.instances {
        if i.config_dir.as_deref().is_some_and(|d| root_under(d, root)) {
            lent.names.insert(name.clone());
            lent.ids.insert(i.instance_id.clone());
            lent.containers.insert(i.container.clone());
            if let Some(dir) = &i.config_dir {
                lent.roots.insert(dir.clone());
            }
        }
    }
    lent
}

/// [`lent_instances`]' answer.
#[derive(Debug, Default)]
pub struct Lent {
    pub names: BTreeSet<String>,
    pub ids: BTreeSet<String>,
    pub containers: BTreeSet<String>,
    /// Their config roots, as recorded.
    pub roots: BTreeSet<PathBuf>,
}

impl Lent {
    /// The `project_id`s of [`Lent::roots`] (`commands::services`): what
    /// their service containers are labelled and named by. A root that no
    /// longer resolves has none.
    fn projects(&self) -> BTreeSet<String> {
        self.roots.iter().filter_map(|d| crate::commands::services::project_id(d).ok()).collect()
    }
}

/// Gate, then confine, a hub's request (see the module doc): the params
/// to run it with. `state` is this host's `state.toml`, read only when the
/// request names an instance or a container.
pub fn admit(method: &str, mut params: Value, scope: &Scope, state: &Path) -> Result<Value, ApiError> {
    let gate = decide(method, &params)?;
    let Some(obj) = params.as_object_mut() else {
        return Ok(params);
    };
    for key in ["root", "dir"] {
        let Some(v) = obj.get(key) else { continue };
        let Some(path) = v.as_str() else {
            return Err(ApiError::invalid(format!("`{key}` must be a path")));
        };
        let real = confine_path(&scope.root, Path::new(path), gate != Gate::Unwatch)?;
        obj.insert(key.into(), Value::String(real.to_string_lossy().into_owned()));
    }
    // The methods whose path is how they find what they act on: without
    // one they'd fall back to something outside the scope, or fail anyway.
    let path_key = match gate {
        Gate::Watch | Gate::Unwatch => Some("dir"),
        Gate::Verb => Some("root"),
        Gate::Quick if method == "instances.list" => Some("dir"),
        Gate::Quick
            if method.starts_with("sandboxes.")
                || method.starts_with("config.")
                || method.starts_with("sessions.")
                || method == "services.get" =>
        {
            Some("root")
        }
        _ => None,
    };
    if let Some(key) = path_key
        && !obj.contains_key(key)
    {
        return Err(ApiError::invalid(format!("`{method}` needs `{key}`")));
    }

    let keys = instance_keys(method);
    let named_container = matches!(method, "containers.logs" | "containers.inspect");
    if !keys.is_empty() || method == "instances.procs" || named_container {
        let state = State::load_at(state).map_err(ApiError::internal)?;
        let lent = lent_instances(&state, &scope.root);
        let unknown = |what: &str| ApiError::not_found(format!("no instance `{what}`"));
        for key in keys {
            let Some(name) = obj.get(*key).and_then(Value::as_str) else {
                if needs_instance(method) {
                    return Err(ApiError::invalid(format!("`{method}` from a fleet hub needs `{key}`")));
                }
                // Not a string: the method's own parse refuses it.
                continue;
            };
            if gate == Gate::Verb {
                // Resolved as the CLI resolves it (`commands::resolve_instance`):
                // every candidate must be lent.
                let matches = crate::commands::instance_matches(&state, name);
                if matches.iter().any(|m| !lent.names.contains(m)) {
                    return Err(unknown(name));
                }
            } else if !lent.names.contains(name) && !lent.ids.contains(name) {
                return Err(unknown(name));
            }
        }
        if method == "instances.procs"
            && let Some(names) = obj.get("instances").and_then(Value::as_array)
            && let Some(bad) = names.iter().filter_map(Value::as_str).find(|n| !lent.names.contains(*n))
        {
            return Err(unknown(bad));
        }
        if named_container
            && let Some(c) = obj.get("container").and_then(Value::as_str)
            && !lent.containers.contains(c)
        {
            return Err(ApiError::not_found(format!("no container `{c}` lent to this hub")));
        }
    }
    if gate == Gate::Verb {
        obj.insert("env".into(), serde_json::to_value(&scope.env).map_err(|e| ApiError::internal(e.into()))?);
    }
    Ok(params)
}

/// A local answer to a hub's request, cut to the fleet root: an
/// `instances.list` like a snapshot ([`filter_snapshot`]); an
/// `instances.resolve`'s matches to lent instances (none left is
/// `not-found`); `containers.list` / `containers.stats` rows to the lent
/// instances' containers and their config roots' services
/// ([`cut_containers`]; `state` is this host's `state.toml`, read for
/// those two only). Others pass as they are: their params were confined.
pub fn filter_answer(method: &str, mut answer: Value, root: &Path, state: &Path) -> Result<Value, ApiError> {
    match method {
        "instances.list" => filter_snapshot(&mut answer, root),
        "containers.list" | "containers.stats" => {
            let state = State::load_at(state).map_err(ApiError::internal)?;
            let lent = lent_instances(&state, root);
            cut_containers(method, &mut answer, &lent.containers, &lent.projects());
        }
        "instances.resolve" => {
            let found: Vec<Value> = answer
                .get("found")
                .and_then(Value::as_array)
                .map(|f| f.iter().filter(|f| row_lent(f, root)).cloned().collect())
                .unwrap_or_default();
            if found.is_empty() {
                return Err(ApiError::not_found("no instance matches"));
            }
            let names: Vec<Value> = found.iter().filter_map(|f| f.get("name").cloned()).collect();
            answer["found"] = Value::Array(found);
            answer["matches"] = Value::Array(names);
        }
        _ => {}
    }
    Ok(answer)
}

/// A `containers.list` / `containers.stats` answer's `rows` cut, in place,
/// to `containers` (the lent instances') and the service containers of
/// `projects` (the lent instances' config roots' `project_id`s): a
/// listing row by its `devsandbox.project` label (none: dropped), a stats
/// row, which has no labels, by its name (`services::service_container`'s
/// `devsandbox-svc-<project>-` prefix, isolated ones' too).
pub fn cut_containers(method: &str, answer: &mut Value, containers: &BTreeSet<String>, projects: &BTreeSet<String>) {
    let Some(rows) = answer.get_mut("rows").and_then(Value::as_array_mut) else { return };
    let prefixes: Vec<String> = projects.iter().map(|p| crate::commands::services::service_container(p, "")).collect();
    rows.retain(|r| {
        let Some(name) = r.get("name").and_then(Value::as_str) else { return false };
        if containers.contains(name) {
            return true;
        }
        if method == "containers.stats" {
            return prefixes.iter().any(|p| name.starts_with(p.as_str()));
        }
        let project = r.get("labels").and_then(|l| l.get("devsandbox.project")).and_then(Value::as_str);
        project.is_some_and(|p| projects.contains(p))
    });
}

/// Whether a row (`InstanceRow`, a resolve's `found`) has its `root` under
/// `root`; a row without one isn't lent.
fn row_lent(row: &Value, root: &Path) -> bool {
    row.get("root").and_then(Value::as_str).is_some_and(|r| root_under(Path::new(r), root))
}

/// An `instances.list` answer / `instances.snapshot` params cut to the
/// instances under `root` (by their rows' `root`), in place: other rows
/// gone, and their names from services' `used_by` and from
/// `thread_children`. The snapshot lists every instance in `state.toml`,
/// whatever `dir`, so this is what keeps this host's own out.
pub fn filter_snapshot(snapshot: &mut Value, root: &Path) {
    let mut kept = BTreeSet::new();
    let mut ids = BTreeSet::new();
    if let Some(rows) = snapshot.get_mut("instances").and_then(Value::as_array_mut) {
        rows.retain(|r| row_lent(r, root));
        kept.extend(rows.iter().filter_map(|r| r.get("name").and_then(Value::as_str).map(String::from)));
        ids.extend(rows.iter().filter_map(|r| r.get("instance_id").and_then(Value::as_str).map(String::from)));
    }
    let keep = |v: &Value| v.as_str().is_some_and(|n| kept.contains(n));
    // An isolated service's container is per instance
    // (`services::isolated_service_container`: `…svc-<project>-<instance id>-<name>`),
    // so another root's instance of a same-named sandbox would show by name.
    let lent_container = |c: &Value| {
        let name = c.get(0).and_then(Value::as_str).unwrap_or_default();
        ids.iter().any(|id| name.contains(&format!("-{id}-")))
    };
    if let Some(services) = snapshot.get_mut("services").and_then(Value::as_array_mut) {
        for s in services {
            if let Some(used) = s.get_mut("used_by").and_then(Value::as_array_mut) {
                used.retain(keep);
            }
            if s.get("scope").and_then(Value::as_str) == Some("isolated")
                && let Some(containers) = s.get_mut("containers").and_then(Value::as_array_mut)
            {
                containers.retain(lent_container);
            }
        }
    }
    if let Some(owners) = snapshot.get_mut("thread_children").and_then(Value::as_object_mut) {
        for children in owners.values_mut() {
            if let Some(c) = children.as_object_mut() {
                c.retain(|_, child| keep(child));
            }
        }
        owners.retain(|_, c| c.as_object().is_some_and(|c| !c.is_empty()));
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dsv-lent-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    #[test]
    fn the_allow_list() {
        use Gate::*;
        let p = json!({});
        let cases: &[(&str, Value, Result<Gate, &str>)] = &[
            ("hello", p.clone(), Ok(Hello)),
            ("subscribe", json!({"topics": ["instances"]}), Ok(Subscribe)),
            ("unsubscribe", json!({"topics": ["instances"]}), Ok(Subscribe)),
            ("subscribe", json!({"topics": ["instances", "inbox"]}), Err("denied")),
            ("subscribe", json!({"topics": ["forwards"]}), Err("denied")),
            ("instances.watch", json!({"dir": "/x"}), Ok(Watch)),
            ("instances.watch", json!({"dir": "/x", "machines": true}), Err("denied")),
            ("instances.unwatch", json!({"dir": "/x"}), Ok(Unwatch)),
            ("instances.list", json!({"dir": "/x"}), Ok(Quick)),
            ("instances.resolve", json!({"name": "a"}), Ok(Quick)),
            ("instances.resolve", json!({"name": "a", "machines": true}), Err("denied")),
            ("containers.logs", json!({"container": "c"}), Ok(Quick)),
            ("containers.list", json!({"all": true}), Ok(Quick)),
            ("containers.stats", p.clone(), Ok(Quick)),
            ("containers.list", json!({"machine": "m"}), Err("denied")),
            ("sandboxes.list", json!({"root": "/x"}), Ok(Quick)),
            ("config.migrate", json!({"root": "/x"}), Ok(Quick)),
            ("capacity.get", p.clone(), Ok(Capacity)),
            ("capacity.get", json!({"machine": "m"}), Err("denied")),
            ("run", json!({"root": "/x"}), Ok(Verb)),
            ("rm", json!({"root": "/x"}), Ok(Verb)),
            ("services.rebuild", json!({"root": "/x"}), Ok(Verb)),
            ("stop", json!({"root": "/x", "all": true}), Err("denied")),
            ("stop", json!({"root": "/x", "machine": "m"}), Err("denied")),
            ("instances.list", json!({"dir": "/x", "machine": "m"}), Err("denied")),
            ("instances.list", json!({"machine": 3}), Err("invalid")),
            ("machines.list", p.clone(), Err("denied")),
            ("machines.hold", json!({"name": "m"}), Err("denied")),
            ("fleet.offer", p.clone(), Err("denied")),
            ("fleet.agent", json!({"worker": "w"}), Err("denied")),
            ("lend.get", p.clone(), Err("denied")),
            ("lend.set", json!({"state": "off"}), Err("denied")),
            ("shutdown", p.clone(), Err("denied")),
            ("helper.call", p.clone(), Err("denied")),
            ("forwards.add", p.clone(), Err("denied")),
            ("forwards.list", p.clone(), Err("denied")),
            ("roots.list", p.clone(), Err("denied")),
            ("bridges.ensure", p.clone(), Err("denied")),
            ("inbox.threads.list", p.clone(), Err("denied")),
            ("nope", p.clone(), Err("denied")),
        ];
        for (method, params, want) in cases {
            let got = decide(method, params).map_err(|e| e.code);
            assert_eq!(got, *want, "{method} {params}");
        }
        // Every quick call and verb is lent (with confined params).
        for m in fwd::QUICK {
            assert_eq!(decide(m, &p).unwrap(), Quick, "{m}");
        }
    }

    #[test]
    fn draining_refuses_new_work_only() {
        let e = refuse_new_work("run", true).unwrap_err();
        assert_eq!((e.code, e.message.as_str()), ("denied", DRAINING));
        for m in ["start", "stop", "rm", "rebuild", "services.rebuild", "capacity.get", "instances.list"] {
            assert!(refuse_new_work(m, true).is_ok(), "{m}");
        }
        assert!(refuse_new_work("run", false).is_ok());
    }

    #[test]
    fn paths_are_confined_to_the_fleet_root() {
        let base = scratch("paths");
        let root = base.join("fleet/hub");
        std::fs::create_dir_all(root.join("cfg/sub")).unwrap();
        std::fs::create_dir_all(base.join("mine")).unwrap();
        std::os::unix::fs::symlink(base.join("mine"), root.join("escape")).unwrap();
        std::os::unix::fs::symlink(root.join("cfg"), base.join("into")).unwrap();

        assert_eq!(confine_path(&root, &root.join("cfg"), true).unwrap(), root.join("cfg"));
        assert_eq!(confine_path(&root, &root, true).unwrap(), root, "the root itself");
        // A symlink from outside into the root resolves inside: fine.
        assert_eq!(confine_path(&root, &base.join("into/sub"), true).unwrap(), root.join("cfg/sub"));
        let code = |p: &Path, exists| confine_path(&root, p, exists).unwrap_err().code;
        assert_eq!(code(&base.join("mine"), true), "denied", "outside");
        assert_eq!(code(&root.join("escape"), true), "denied", "a symlink out");
        assert_eq!(code(&root.join("cfg/../../../mine"), true), "denied", "`..`");
        assert_eq!(code(&root.join("cfg/.."), true), "denied", "`..` even when it stays inside");
        assert_eq!(code(Path::new("rel"), true), "invalid");
        assert_eq!(code(&root.join("gone"), true), "not-found");
        assert_eq!(confine_path(&root, &root.join("gone"), false).unwrap(), root.join("gone"), "an unwatch");
        assert_eq!(code(&base.join("gone"), false), "denied");
        // A sibling whose name extends the root's isn't under it.
        std::fs::create_dir_all(base.join("fleet/hub2")).unwrap();
        assert_eq!(code(&base.join("fleet/hub2"), true), "denied");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// `state.toml` with `in` under the fleet root and `own` outside.
    fn state_with(base: &Path, root: &Path) -> PathBuf {
        let state = base.join("state.toml");
        let inst = |name: &str, dir: &Path| {
            format!(
                "[instance.{name}]\ninstance_id = \"id-{name}\"\nsandbox = \"web\"\ncontainer = \"devsandbox-{name}\"\n\
                 folder = \"/w\"\nworkspace = \"/w\"\ncreated_unix = 1\nconfig_dir = {:?}\n",
                dir.to_str().unwrap()
            )
        };
        std::fs::write(&state, inst("in", &root.join("cfg")) + &inst("own", &base.join("mine"))).unwrap();
        state
    }

    fn scope(root: &Path) -> Scope {
        Scope { hub: "me@hub".into(), root: root.to_path_buf(), env: BTreeMap::from([("PATH".into(), "/worker/bin".into())]) }
    }

    #[test]
    fn admit_confines_paths_instances_and_containers() {
        let base = scratch("admit");
        let root = base.join("fleet/hub");
        std::fs::create_dir_all(root.join("cfg")).unwrap();
        std::fs::create_dir_all(base.join("mine")).unwrap();
        let state = state_with(&base, &root);
        let s = scope(&root);
        let ok = |m: &str, p: Value| admit(m, p, &s, &state).unwrap();
        let code = |m: &str, p: Value| admit(m, p, &s, &state).unwrap_err().code;

        let cfg = root.join("cfg");
        assert_eq!(ok("instances.list", json!({"dir": cfg})), json!({"dir": cfg}));
        assert_eq!(code("instances.list", json!({"dir": base.join("mine")})), "denied");
        assert_eq!(code("instances.list", json!({})), "invalid", "a path is needed");
        assert_eq!(code("sandboxes.list", json!({"root": 3})), "invalid");
        assert_eq!(code("config.findings", json!({"root": base.join("mine")})), "denied");

        assert!(admit("instances.done", json!({"instance": "in", "done": true}), &s, &state).is_ok());
        assert!(admit("instances.buildLog", json!({"instance": "id-in"}), &s, &state).is_ok(), "by id");
        assert_eq!(code("instances.done", json!({"instance": "own", "done": true})), "not-found");
        assert_eq!(code("instances.buildLog", json!({"instance": "id-own"})), "not-found");
        assert_eq!(code("instances.signal", json!({"instance": "own", "pid": "1", "signal": "TERM"})), "not-found");
        assert!(admit("instances.procs", json!({"instances": ["in"]}), &s, &state).is_ok());
        assert_eq!(code("instances.procs", json!({"instances": ["in", "own"]})), "not-found");
        assert!(admit("containers.logs", json!({"container": "devsandbox-in"}), &s, &state).is_ok());
        assert_eq!(code("containers.inspect", json!({"container": "devsandbox-own"})), "not-found");

        // Sessions: the root confined and needed, the instance lent.
        assert_eq!(ok("sessions.list", json!({"root": cfg, "instance": "id-in"})), json!({"root": cfg, "instance": "id-in"}));
        assert!(admit("sessions.rm", json!({"root": cfg, "instance": "in", "id": "1700000000-ab12"}), &s, &state).is_ok());
        assert_eq!(code("sessions.list", json!({"root": cfg, "instance": "own"})), "not-found");
        assert_eq!(code("sessions.rm", json!({"root": base.join("mine"), "instance": "own", "id": "x"})), "denied");
        assert_eq!(code("sessions.list", json!({"instance": "in"})), "invalid", "a root is needed");

        // Verbs: the root confined, the instance resolved (a sandbox name
        // matching both is refused), and named.
        assert_eq!(code("stop", json!({"root": base.join("mine"), "instance": "in"})), "denied");
        assert_eq!(code("stop", json!({"root": cfg, "instance": "own"})), "not-found");
        assert_eq!(code("stop", json!({"root": cfg, "instance": "web"})), "not-found", "matches `own` too");
        assert_eq!(code("rm", json!({"root": cfg})), "invalid");
        assert_eq!(code("start", json!({"root": cfg, "instance": null})), "invalid");
        assert!(admit("run", json!({"root": cfg, "sandbox": "web", "env": {}}), &s, &state).is_ok(), "run names none");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_verbs_env_is_this_hosts() {
        let base = scratch("env");
        let root = base.join("fleet/hub");
        std::fs::create_dir_all(root.join("cfg")).unwrap();
        let state = state_with(&base, &root);
        let p = json!({"root": root.join("cfg"), "instance": "in", "env": {"PATH": "/hub/bin", "SSH_AUTH_SOCK": "/hub/agent"}});
        let got = admit("stop", p, &scope(&root), &state).unwrap();
        assert_eq!(got["env"], json!({"PATH": "/worker/bin"}));
        // Missing too: the verb gets it all the same.
        let got = admit("run", json!({"root": root.join("cfg"), "sandbox": "web"}), &scope(&root), &state).unwrap();
        assert_eq!(got["env"], json!({"PATH": "/worker/bin"}));
        // Not a verb: params untouched.
        let got = admit("sandboxes.list", json!({"root": root.join("cfg"), "env": {"A": "b"}}), &scope(&root), &state).unwrap();
        assert_eq!(got["env"], json!({"A": "b"}));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn rows_are_cut_to_the_fleet_root() {
        let root = Path::new("/nonexistent/fleet/hub");
        let mut snap = json!({
            "instances": [
                {"name": "in", "root": "/nonexistent/fleet/hub/cfg", "instance_id": "i1"},
                {"name": "own", "root": "/nonexistent/mine", "instance_id": "o1"},
                {"name": "sneaky", "root": "/nonexistent/fleet/hub/../../mine"},
                {"name": "old", "root": null},
            ],
            "services": [
                {"name": "db", "used_by": ["in", "own"]},
                {"name": "cache", "scope": "isolated", "used_by": ["in", "own"], "containers": [
                    ["devsandbox-svc-p-i1-cache", {"Running": "Up"}], ["devsandbox-svc-p-o1-cache", {"Running": "Up"}]]},
                {"name": "pg", "scope": "global", "used_by": ["in"], "containers": [["devsandbox-svc-p-pg", {"Running": "Up"}]]},
            ],
            "sandboxes": [{"name": "web"}],
            "thread_children": {"id-own": {"k": "own"}, "id-in": {"k": "in", "j": "own"}},
        });
        filter_snapshot(&mut snap, root);
        assert_eq!(snap["instances"], json!([{"name": "in", "root": "/nonexistent/fleet/hub/cfg", "instance_id": "i1"}]));
        assert_eq!(snap["services"][0]["used_by"], json!(["in"]));
        assert_eq!(snap["services"][1]["containers"], json!([["devsandbox-svc-p-i1-cache", {"Running": "Up"}]]), "another root's isolated copy cut");
        assert_eq!(snap["services"][2]["containers"].as_array().unwrap().len(), 1, "a global one kept");
        assert_eq!(snap["thread_children"], json!({"id-in": {"k": "in"}}));
        assert_eq!(snap["sandboxes"], json!([{"name": "web"}]), "the root's own config, as is");

        let resolved = json!({"matches": ["in", "own"], "found": [
            {"name": "in", "root": "/nonexistent/fleet/hub/cfg"}, {"name": "own", "root": "/nonexistent/mine"}]});
        let got = filter_answer("instances.resolve", resolved, root, Path::new("/nonexistent/state.toml")).unwrap();
        assert_eq!(got, json!({"matches": ["in"], "found": [{"name": "in", "root": "/nonexistent/fleet/hub/cfg"}]}));
        let own = json!({"matches": ["own"], "found": [{"name": "own", "root": "/nonexistent/mine"}]});
        assert_eq!(filter_answer("instances.resolve", own, root, Path::new("/nonexistent/state.toml")).unwrap_err().code, "not-found");
        assert_eq!(filter_answer("sandboxes.list", json!({"a": 1}), root, Path::new("/nonexistent/state.toml")).unwrap(), json!({"a": 1}));
    }

    #[test]
    fn container_rows_are_cut_to_lent_instances_and_their_services() {
        let containers = BTreeSet::from(["devsandbox-in".to_string()]);
        let projects = BTreeSet::from(["aaaa1111".to_string()]);
        let svc = |name: &str, project: &str| json!({"name": name, "labels": {"devsandbox.project": project}});
        let mut list = json!({"rows": [
            {"name": "devsandbox-in", "labels": {}},
            {"name": "devsandbox-own", "labels": {}},
            svc("devsandbox-svc-aaaa1111-db", "aaaa1111"),
            svc("devsandbox-svc-bbbb2222-db", "bbbb2222"),
            // Named like a lent service, labelled otherwise: the label decides.
            svc("devsandbox-svc-aaaa1111-x", "bbbb2222"),
            {"name": "devsandbox-svc-aaaa1111-nolabels"},
            {"labels": {"devsandbox.project": "aaaa1111"}},
        ]});
        cut_containers("containers.list", &mut list, &containers, &projects);
        let names: Vec<&str> = list["rows"].as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["devsandbox-in", "devsandbox-svc-aaaa1111-db"]);

        let row = |name: &str| json!({"name": name, "cpu": "1%", "mem": "2MiB"});
        let mut stats = json!({"rows": [row("devsandbox-in"), row("devsandbox-own"), row("devsandbox-svc-aaaa1111-in-db"),
            row("devsandbox-svc-bbbb2222-db"), row("devsandbox-svc-aaaa11112-db")]});
        cut_containers("containers.stats", &mut stats, &containers, &projects);
        assert_eq!(stats["rows"], json!([row("devsandbox-in"), row("devsandbox-svc-aaaa1111-in-db")]));
    }

    /// The projects come from the lent instances' config roots in `state.toml`.
    #[test]
    fn container_answers_are_cut_by_the_state() {
        let base = scratch("containers");
        let root = base.join("fleet/hub");
        std::fs::create_dir_all(root.join("cfg")).unwrap();
        std::fs::create_dir_all(base.join("mine")).unwrap();
        let state = state_with(&base, &root);
        let lent = crate::commands::services::project_id(&root.join("cfg")).unwrap();
        let mine = crate::commands::services::project_id(&base.join("mine")).unwrap();
        let svc = |p: &str| json!({"name": format!("devsandbox-svc-{p}-db"), "labels": {"devsandbox.project": p}});
        let answer = json!({"rows": [{"name": "devsandbox-in", "labels": {}}, {"name": "devsandbox-own", "labels": {}}, svc(&lent), svc(&mine)]});
        let got = filter_answer("containers.list", answer, &root, &state).unwrap();
        assert_eq!(got, json!({"rows": [{"name": "devsandbox-in", "labels": {}}, svc(&lent)]}));
        let stats = json!({"rows": [{"name": "devsandbox-own"}, {"name": format!("devsandbox-svc-{lent}-db")}]});
        let got = filter_answer("containers.stats", stats, &root, &state).unwrap();
        assert_eq!(got, json!({"rows": [{"name": format!("devsandbox-svc-{lent}-db")}]}));
        assert_eq!(filter_answer("containers.list", json!({"rows": []}), &root, &base.join("bad")).unwrap(), json!({"rows": []}), "no state: none lent");
        let _ = std::fs::remove_dir_all(&base);
    }
}
