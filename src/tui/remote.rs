//! Machines in the dashboard (docs/architecture/daemon/machines.md, *Dashboard*): the tree merges
//! every registered machine's rows (`instances.watch {machines: true}`), so
//! an instance is (machine, name), not a name.
//!
//! The one key everywhere the dashboard keys by name (the tree's groups,
//! the process cache, the in-flight sets, the pending actions, the terminal
//! tabs) is [`qualify`]'s: the bare name for this host's rows, `box/web-2`
//! for machine `box`'s. A name can't hold `/` (docker container names
//! can't), so the key is unambiguous and a local one is exactly what it was
//! before machines. `App` queues keys; the I/O edges ([`route`] in the event
//! loop, the daemon worker, the poll) split them back into the request's
//! `machine` and the row's remote `root`.
//!
//! Pure over the snapshot and its `machines` entries, tested here.

use crate::commands::exec::SHELL_FALLBACK_CMD;
use crate::serve::machines::{Entry, Where, parse_name, ssh};
use crate::serve::snapshots::{LinkState, SnapshotMachine};
use crate::snapshot::{InstanceRow, SandboxRow, ServiceRow, Snapshot};

/// `name` on `machine` as the dashboard keys it: bare when local.
pub fn qualify(machine: Option<&str>, name: &str) -> String {
    match machine {
        Some(m) => format!("{m}/{name}"),
        None => name.to_string(),
    }
}

/// A [`qualify`] key back into (machine, name).
pub fn split(key: &str) -> (Option<&str>, &str) {
    match key.split_once('/') {
        Some((m, name)) => (Some(m), name),
        None => (None, key),
    }
}

pub fn instance_key(r: &InstanceRow) -> String {
    qualify(r.machine.as_deref(), &r.name)
}

pub fn sandbox_key(r: &SandboxRow) -> String {
    qualify(r.machine.as_deref(), &r.name)
}

pub fn service_key(r: &ServiceRow) -> String {
    qualify(r.machine.as_deref(), &r.name)
}

/// The instance row keyed `key`.
pub fn find_instance<'a>(s: &'a Snapshot, key: &str) -> Option<&'a InstanceRow> {
    s.instances.iter().find(|r| instance_key(r) == key)
}

/// Machine `name`'s entry in the snapshot's `machines`.
pub fn machine<'a>(machines: &'a [SnapshotMachine], name: &str) -> Option<&'a SnapshotMachine> {
    machines.iter().find(|m| m.name == name)
}

/// Whether rows of `machine` show as `unreachable`: a machine whose link
/// isn't `connected` (or one no longer listed). Local rows never are.
pub fn unreachable(machines: &[SnapshotMachine], row_machine: Option<&str>) -> bool {
    row_machine.is_some_and(|m| machine(machines, m).is_none_or(|m| m.state != LinkState::Connected))
}

/// Where a request about a row goes: the `machine` param (`None`: this
/// host) and, for a remote row, the remote config root it's in.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Route {
    pub machine: Option<String>,
    pub root: Option<String>,
}

/// The first root `machine` showed rows from: what an instance without a
/// recorded root is taken under.
pub fn machine_root(s: &Snapshot, machine: &str) -> Option<String> {
    let on = |m: &Option<String>| m.as_deref() == Some(machine);
    let sandboxes = s.sandboxes.iter().filter(|r| on(&r.machine)).map(|r| &r.root);
    let services = s.services.iter().filter(|r| on(&r.machine)).map(|r| &r.root);
    sandboxes.chain(services).find_map(Clone::clone)
}

/// The route of the instance keyed `key`: its machine, and its own root
/// there, else the machine's first. Local keys route nowhere special.
pub fn instance_route(s: &Snapshot, key: &str) -> Route {
    let (machine, _) = split(key);
    let Some(machine) = machine else { return Route::default() };
    let root = find_instance(s, key).and_then(|r| r.root.clone()).or_else(|| machine_root(s, machine));
    Route { machine: Some(machine.to_string()), root }
}

/// The route of the sandbox keyed `key` (`run`, its config): the root its
/// row was read from.
pub fn sandbox_route(s: &Snapshot, key: &str) -> Route {
    let (machine, _) = split(key);
    let Some(machine) = machine else { return Route::default() };
    let root = s
        .sandboxes
        .iter()
        .find(|r| sandbox_key(r) == key)
        .and_then(|r| r.root.clone())
        .or_else(|| machine_root(s, machine));
    Route { machine: Some(machine.to_string()), root }
}

/// The route of the service keyed `key`.
pub fn service_route(s: &Snapshot, key: &str) -> Route {
    let (machine, _) = split(key);
    let Some(machine) = machine else { return Route::default() };
    let root = s
        .services
        .iter()
        .find(|r| service_key(r) == key)
        .and_then(|r| r.root.clone())
        .or_else(|| machine_root(s, machine));
    Route { machine: Some(machine.to_string()), root }
}

/// An ssh [`Entry`] from a snapshot's machine entry (its roots aren't
/// needed for an argv).
fn entry(m: &SnapshotMachine) -> Entry {
    Entry { name: m.name.clone(), target: m.target.clone(), roots: Vec::new(), command: m.command.clone(), agent: m.agent }
}

fn lossy(argv: Vec<std::ffi::OsString>) -> Vec<String> {
    argv.into_iter().map(|a| a.to_string_lossy().into_owned()).collect()
}

/// A remote instance's terminal (and `:exec`): `ssh -t … devsandbox -C
/// <root> exec -i -t -- <name> [command…]` on its machine.
pub fn exec_argv(m: &SnapshotMachine, root: &str, name: &str, command: &[String]) -> Result<Vec<String>, String> {
    let mut args: Vec<String> = ["-C", root, "exec", "-i", "-t", "--", name].map(String::from).to_vec();
    args.extend(command.iter().cloned());
    ssh::tty_argv(&entry(m), &args).map(lossy).map_err(|e| format!("{e:#}"))
}

/// A remote service container's terminal: `ssh -t … <runtime> exec -i -t
/// <container> <login shell>`, the runtime the machine's snapshot named
/// (`docker` before one came).
pub fn service_argv(m: &SnapshotMachine, container: &str) -> Result<Vec<String>, String> {
    let runtime = m.runtime.as_deref().unwrap_or("docker");
    let mut args: Vec<String> = ["exec", "-i", "-t", container].map(String::from).to_vec();
    args.extend(SHELL_FALLBACK_CMD.iter().map(|s| s.to_string()));
    ssh::tty_argv_with(&entry(m), runtime, &args).map(lossy).map_err(|e| format!("{e:#}"))
}

/// The machines with no rows in `s` yet, one dim note each: `box:
/// connecting`, `box (agent): unreachable: <why>` ([`label`]). Joined with ` · `; `None` when
/// every machine has rows (or none is registered). Without `why`, an
/// unreachable one doesn't say why (the header's short of room).
pub fn pending_note(machines: &[SnapshotMachine], s: &Snapshot, why: bool) -> Option<String> {
    let has_rows = |name: &str| {
        let on = |m: &Option<String>| m.as_deref() == Some(name);
        s.instances.iter().any(|r| on(&r.machine))
            || s.sandboxes.iter().any(|r| on(&r.machine))
            || s.services.iter().any(|r| on(&r.machine))
    };
    let notes: Vec<String> = machines
        .iter()
        .filter(|m| !has_rows(&m.name))
        .filter_map(|m| match (m.state, &m.error) {
            (LinkState::Unreachable, Some(e)) if why => Some(format!("{}: unreachable: {}", label(m), short(&why_of(&m.name, e)))),
            (LinkState::Unreachable, _) => Some(format!("{}: unreachable", label(m))),
            // Connected with nothing in its roots: nothing to say.
            (LinkState::Connected, _) => None,
            _ => Some(format!("{}: connecting", label(m))),
        })
        .collect();
    (!notes.is_empty()).then(|| notes.join(" · "))
}

/// A machine's name as the header says it: `box (agent)` when this host's
/// ssh-agent is relayed there (its registry `agent`), else `box`.
fn label(m: &SnapshotMachine) -> String {
    if m.agent { format!("{} (agent)", m.name) } else { m.name.clone() }
}

/// A machine's `error` without the `<machine>: ` the daemon's messages
/// start with: the dashboard shows the name next to it already.
pub fn why_of(machine: &str, error: &str) -> String {
    error.strip_prefix(machine).and_then(|e| e.strip_prefix(": ")).unwrap_or(error).to_string()
}

/// The header's room for a reason: the first 40 characters, so a long ssh
/// error can't push the totals over the tab labels.
fn short(why: &str) -> String {
    match why.char_indices().nth(40) {
        Some((at, _)) => format!("{}…", &why[..at]),
        None => why.to_string(),
    }
}

/// What a `:` command's name argument names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Instance,
    Sandbox,
    Service,
}

/// A typed `<name>`, `<machine>/<name>` or `@/<name>` as a [`qualify`] key,
/// against the snapshot's rows of `kind` (exact names). A bare name matching
/// rows on one machine (or only here) is that one; on several it's an
/// error listing the qualified names. No exact match passes the name on as
/// typed (the daemon resolves sandbox / folder names, as before machines).
pub fn resolve(s: &Snapshot, arg: &str, kind: Kind) -> Result<String, String> {
    let (place, name) = parse_name(arg).map_err(|e| format!("{e:#}"))?;
    let rows: Vec<Option<&str>> = match kind {
        Kind::Instance => s.instances.iter().filter(|r| r.name == name).map(|r| r.machine.as_deref()).collect(),
        Kind::Sandbox => s.sandboxes.iter().filter(|r| r.name == name).map(|r| r.machine.as_deref()).collect(),
        Kind::Service => s.services.iter().filter(|r| r.name == name).map(|r| r.machine.as_deref()).collect(),
    };
    let mut found: Vec<Option<&str>> = rows
        .into_iter()
        .filter(|m| match &place {
            Where::Local => m.is_none(),
            Where::Machine(want) => *m == Some(want.as_str()),
            Where::Any => true,
        })
        .collect();
    found.sort();
    found.dedup();
    match (found.as_slice(), &place) {
        ([one], _) => Ok(qualify(*one, &name)),
        ([], Where::Machine(m)) => Ok(qualify(Some(m), &name)),
        ([], _) => Ok(name),
        (many, _) => {
            let listed: Vec<String> = many.iter().map(|m| qualify(Some(m.unwrap_or("@")), &name)).collect();
            Err(format!("`{name}` is on several machines: {}", listed.join(", ")))
        }
    }
}

/// Completion candidates for `kind`: every row's name, remote ones
/// qualified (a bare name is still accepted when unique).
pub fn candidates(s: &Snapshot, kind: Kind) -> Vec<String> {
    let mut out: Vec<String> = match kind {
        Kind::Instance => s.instances.iter().map(instance_key).collect(),
        Kind::Sandbox => s.sandboxes.iter().map(sandbox_key).collect(),
        Kind::Service => s.services.iter().map(service_key).collect(),
    };
    // A sandbox / service defined in two of a machine's roots: one key.
    let mut seen = std::collections::BTreeSet::new();
    out.retain(|k| seen.insert(k.clone()));
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn sm(name: &str, state: LinkState) -> SnapshotMachine {
        SnapshotMachine {
            name: name.into(),
            target: format!("me@{name}"),
            state,
            error: None,
            version: None,
            runtime: None,
            command: None,
            agent: false,
        }
    }

    fn snap(instances: &[(&str, Option<&str>, Option<&str>)], sandboxes: &[(&str, Option<&str>, Option<&str>)]) -> Snapshot {
        let inst: Vec<_> = instances
            .iter()
            .map(|(n, m, root)| {
                let mut r = crate::serve::snapshots::tests::row(n, true, None);
                r.machine = m.map(String::from);
                r.root = root.map(String::from);
                r
            })
            .collect();
        let mut s = crate::serve::snapshots::tests::snapshot(inst, false);
        s.sandboxes = sandboxes
            .iter()
            .map(|(n, m, root)| {
                serde_json::from_value(serde_json::json!({
                    "name": n, "source": "image x", "folder": null, "services": [], "extends": [],
                    "config_hash": "", "build_hash": "", "issues": [], "dispatcher": false,
                    "worktree_branch": null, "root": root, "machine": m,
                }))
                .unwrap()
            })
            .collect();
        s
    }

    #[test]
    fn keys_qualify_remote_names_only() {
        assert_eq!(qualify(None, "web"), "web");
        assert_eq!(qualify(Some("box"), "web-2"), "box/web-2");
        assert_eq!(split("box/web-2"), (Some("box"), "web-2"));
        assert_eq!(split("web"), (None, "web"));
    }

    #[test]
    fn routes_carry_the_rows_remote_root() {
        let s = snap(
            &[("web", None, Some("/here")), ("web", Some("box"), Some("/r/two")), ("old", Some("box"), None)],
            &[("web", Some("box"), Some("/r/one")), ("api", Some("box"), Some("/r/two"))],
        );
        assert_eq!(instance_route(&s, "web"), Route::default());
        assert_eq!(instance_route(&s, "box/web"), Route { machine: Some("box".into()), root: Some("/r/two".into()) });
        // No recorded root: the machine's first.
        assert_eq!(instance_route(&s, "box/old").root.as_deref(), Some("/r/one"));
        assert_eq!(sandbox_route(&s, "box/api").root.as_deref(), Some("/r/two"));
        assert_eq!(sandbox_route(&s, "web"), Route::default());
    }

    #[test]
    fn bare_names_resolve_when_unique_and_qualified_ones_where_they_say() {
        let s = snap(
            &[("web", None, None), ("web", Some("box"), None), ("api", Some("box"), None), ("db", None, None)],
            &[("web", None, None), ("web", Some("box"), None), ("web", Some("box"), None)],
        );
        assert_eq!(resolve(&s, "api", Kind::Instance), Ok("box/api".into()));
        assert_eq!(resolve(&s, "db", Kind::Instance), Ok("db".into()));
        assert_eq!(resolve(&s, "web", Kind::Instance), Err("`web` is on several machines: @/web, box/web".into()));
        assert_eq!(resolve(&s, "@/web", Kind::Instance), Ok("web".into()));
        assert_eq!(resolve(&s, "box/web", Kind::Instance), Ok("box/web".into()));
        // Not a row's exact name: passed on for the daemon to resolve.
        assert_eq!(resolve(&s, "web-folder", Kind::Instance), Ok("web-folder".into()));
        assert_eq!(resolve(&s, "box/x", Kind::Instance), Ok("box/x".into()));
        assert!(resolve(&s, "b@d/x", Kind::Instance).is_err());
        // Sandboxes: one machine's two roots are one match.
        assert_eq!(resolve(&s, "box/web", Kind::Sandbox), Ok("box/web".into()));
        assert!(resolve(&s, "web", Kind::Sandbox).is_err());
        assert_eq!(candidates(&s, Kind::Sandbox), ["web", "box/web"]);
        assert_eq!(candidates(&s, Kind::Instance), ["web", "box/web", "box/api", "db"]);
    }

    #[test]
    fn terminal_argvs_go_over_ssh() {
        let mut m = sm("box", LinkState::Connected);
        m.target = "ssh://me@box:2222".into();
        m.command = Some("~/bin/devsandbox".into());
        assert_eq!(
            exec_argv(&m, "/r/one", "web-2", &[]).unwrap(),
            [
                "ssh", "-t", "-p", "2222", "--", "me@box",
                r#"exec "$SHELL" -lc 'exec ~/bin/devsandbox '\''-C'\'' '\''/r/one'\'' '\''exec'\'' '\''-i'\'' '\''-t'\'' '\''--'\'' '\''web-2'\'''"#,
            ]
        );
        m.runtime = Some("podman".into());
        let argv = service_argv(&m, "devsandbox-svc-db").unwrap();
        assert_eq!(argv[..6], ["ssh", "-t", "-p", "2222", "--", "me@box"]);
        assert!(argv[6].starts_with(r#"exec "$SHELL" -lc 'exec podman '\''exec'\'' '\''-i'\'' '\''-t'\'' '\''devsandbox-svc-db'\''"#), "{}", argv[6]);
        m.runtime = None;
        assert!(service_argv(&m, "c").unwrap()[6].contains("exec docker "));
    }

    #[test]
    fn unreachable_and_pending_machines() {
        let mut down = sm("down", LinkState::Unreachable);
        down.error = Some("Permission denied".into());
        let machines = [sm("up", LinkState::Connected), down, sm("new", LinkState::Connecting)];
        assert!(!unreachable(&machines, None));
        assert!(!unreachable(&machines, Some("up")));
        assert!(unreachable(&machines, Some("down")));
        assert!(unreachable(&machines, Some("new")));
        assert!(unreachable(&machines, Some("gone")));
        let s = snap(&[("web", Some("up"), None)], &[]);
        assert_eq!(pending_note(&machines, &s, true).as_deref(), Some("down: unreachable: Permission denied · new: connecting"));
        assert_eq!(pending_note(&machines, &s, false).as_deref(), Some("down: unreachable · new: connecting"));
        assert_eq!(pending_note(&machines[..1], &s, true), None);
        // `agent` on marks the name; the error's own `<machine>: ` still goes.
        let mut machines = machines;
        machines[1].agent = true;
        machines[1].error = Some("down: Permission denied".into());
        assert_eq!(pending_note(&machines, &s, true).as_deref(), Some("down (agent): unreachable: Permission denied · new: connecting"));
        assert_eq!(why_of("box", "box: ssh: refused"), "ssh: refused");
        assert_eq!(why_of("box", "boxer: x"), "boxer: x");
        assert_eq!(short(&"é".repeat(50)), format!("{}…", "é".repeat(40)));
    }
}
