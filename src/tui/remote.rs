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

use crate::serve::machines::{Where, parse_name};
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

/// The devsandbox executable on this host, which a machine's tabs run
/// ([`exec_argv`]'s `exe`).
pub type Exe<'a> = &'a dyn Fn() -> Result<String, String>;

/// This host's `devsandbox <flags> -- <m>/<name> [command…]` (an `exec`)
/// for instance `name` on machine `m`: the CLI routes the qualified name
/// onto the machine's link (`machines::cli`: the instance's root resolved
/// there with `instances.resolve`, then a `PTY` dial through
/// `fleet::pty_client`), so the terminal goes through the machine's trust
/// like any other use of it.
fn on_machine(m: &SnapshotMachine, flags: &[&str], name: &str, command: &[String], exe: Exe) -> Result<Vec<String>, String> {
    let mut args = vec![exe()?];
    args.extend(flags.iter().map(|s| s.to_string()));
    args.extend(["--".to_string(), qualify(Some(&m.name), name)]);
    args.extend(command.iter().cloned());
    Ok(args)
}

/// A remote instance's terminal (and `:exec`): `devsandbox exec -i -t --
/// <m>/<name> [command…]` ([`on_machine`]).
pub fn exec_argv(m: &SnapshotMachine, name: &str, command: &[String], exe: Exe) -> Result<Vec<String>, String> {
    on_machine(m, &["exec", "-i", "-t"], name, command, exe)
}

/// A remote instance's terminal tab, first dial: [`exec_argv`]'s with
/// `--reserve-255 --session=<id>`, so the shell runs in helper session `id`
/// the tab re-dials ([`attach_argv`]) after a lost connection
/// (`tui::app::terminal::Redial`).
pub fn session_exec_argv(m: &SnapshotMachine, name: &str, id: &str, exe: Exe) -> Result<Vec<String>, String> {
    let session = format!("--session={id}");
    on_machine(m, &["exec", "-i", "-t", "--reserve-255", &session], name, &[], exe)
}

/// A remote instance tab's re-dial: `devsandbox exec --reserve-255
/// --attach=<id> -- <m>/<name>`, attach only (the `=` form, as
/// `--attach`'s value is optional and the name comes after `--`). A link
/// still down when it runs exits 255 like a lost dial (`machines::cli`), so
/// the tab keeps re-dialing through the outage.
pub fn attach_argv(m: &SnapshotMachine, name: &str, id: &str, exe: Exe) -> Result<Vec<String>, String> {
    let attach = format!("--attach={id}");
    on_machine(m, &["exec", "--reserve-255", &attach], name, &[], exe)
}

/// The instance a tab on service `row`'s `container` goes through on its
/// machine (the one whose service it is there): for an isolated
/// service the `used_by` entry at the container's index (the snapshot
/// lists one container per using instance, in `used_by`'s order), for a
/// global one any user, the first.
pub fn service_instance<'a>(row: &'a ServiceRow, container: &str) -> Option<&'a str> {
    if row.scope == "isolated" {
        let at = row.containers.iter().position(|(c, _)| c == container)?;
        return row.used_by.get(at).map(String::as_str);
    }
    row.used_by.first().map(String::as_str)
}

/// A remote service container's terminal (`container`, one of `row`'s):
/// this host's `devsandbox exec --service=<name> -- <m>/<instance>`
/// ([`service_instance`]), whose service dial the machine resolves to the
/// container itself. A global service no instance uses has no such
/// instance: refused (the `PTY` service header names one).
pub fn service_argv(m: &SnapshotMachine, row: &ServiceRow, container: &str, exe: Exe) -> Result<Vec<String>, String> {
    let instance = service_instance(row, container).ok_or_else(|| {
        format!("no instance on `{}` uses service `{}`: a machine's service terminal goes through one", m.name, row.name)
    })?;
    let service = format!("--service={}", row.name);
    on_machine(m, &["exec", &service], instance, &[], exe)
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
            kind: Default::default(),
            target: Some(format!("me@{name}")),
            state,
            error: None,
            version: None,
            runtime: None,
            command: None,
            agent: false,
            capacity: None,
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

    /// Every machine's tabs, ssh or inbound, run this host's devsandbox on
    /// the qualified name: its `exec` goes over the machine's link.
    #[test]
    fn terminal_argvs_run_the_local_cli_on_the_qualified_name() {
        use clap::Parser;
        let parsed = |args: &[String]| {
            let args = args[1..].iter().map(String::as_str);
            crate::Cli::try_parse_from(std::iter::once("devsandbox").chain(args)).unwrap().command.unwrap()
        };
        let id = "1790000000-a1b2";
        let exe = || Ok("/opt/devsandbox".to_string());
        let exe: Exe = &exe;
        let mut ssh = sm("box", LinkState::Connected);
        ssh.target = Some("ssh://me@box:2222".into());
        ssh.command = Some("~/bin/devsandbox".into());
        let lap = SnapshotMachine { kind: crate::serve::machines::MachineKind::Inbound, target: None, ..sm("lap", LinkState::Connected) };
        for (m, w) in [(&ssh, "box/w"), (&lap, "lap/w")] {
            assert_eq!(exec_argv(m, "w", &[], exe).unwrap(), ["/opt/devsandbox", "exec", "-i", "-t", "--", w]);
            assert_eq!(exec_argv(m, "w", &["ls".into()], exe).unwrap(), ["/opt/devsandbox", "exec", "-i", "-t", "--", w, "ls"]);
            let first = session_exec_argv(m, "w", id, exe).unwrap();
            assert_eq!(first, ["/opt/devsandbox", "exec", "-i", "-t", "--reserve-255", "--session=1790000000-a1b2", "--", w]);
            assert!(matches!(
                parsed(&first),
                crate::Command::Exec { reserve_255: true, session: Some(ref s), ref name, .. } if s == id && name == w
            ));
            let again = attach_argv(m, "w", id, exe).unwrap();
            assert_eq!(again, ["/opt/devsandbox", "exec", "--reserve-255", "--attach=1790000000-a1b2", "--", w]);
            assert!(matches!(
                parsed(&again),
                crate::Command::Exec { reserve_255: true, attach: Some(Some(ref a)), ref name, ref command, .. } if a == id && name == w && command.is_empty()
            ));
        }
        let no = || Err("no exe".to_string());
        assert_eq!(exec_argv(&ssh, "w", &[], &no), Err("no exe".into()));
        // A service tab goes through the instance whose service it is.
        let iso = service_row("isolated", &["devsandbox-svc-p-a-db", "devsandbox-svc-p-b-db"], &["a", "b"]);
        for (m, b) in [(&ssh, "box/b"), (&lap, "lap/b")] {
            let argv = service_argv(m, &iso, "devsandbox-svc-p-b-db", exe).unwrap();
            assert_eq!(argv, ["/opt/devsandbox", "exec", "--service=db", "--", b]);
            assert!(matches!(
                parsed(&argv),
                crate::Command::Exec { service: Some(ref s), ref name, ref command, .. } if s == "db" && name == b && command.is_empty()
            ));
        }
        let global = service_row("global", &["devsandbox-svc-p-db"], &["b", "a"]);
        assert_eq!(service_argv(&lap, &global, "devsandbox-svc-p-db", exe).unwrap()[4], "lap/b", "any user: the first");
        let unused = service_row("global", &["devsandbox-svc-p-db"], &[]);
        assert!(service_argv(&ssh, &unused, "devsandbox-svc-p-db", exe).unwrap_err().contains("no instance on `box` uses service `db`"));
        assert_eq!(service_instance(&iso, "devsandbox-svc-other"), None);
    }

    /// Service `db`'s row: its scope, containers and users.
    fn service_row(scope: &str, containers: &[&str], used_by: &[&str]) -> ServiceRow {
        ServiceRow {
            name: "db".into(),
            scope: scope.into(),
            source: "image postgres".into(),
            ports: Vec::new(),
            containers: containers.iter().map(|c| (c.to_string(), crate::snapshot::ContainerStatus::Missing)).collect(),
            used_by: used_by.iter().map(|s| s.to_string()).collect(),
            env_len: 0,
            command: None,
            config_hash: String::new(),
            drift: false,
            root: None,
            machine: Some("lap".into()),
        }
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
