//! Tree/render helpers for the dashboard: flattening the (sandboxes,
//! instances, procs) forest into visible nodes and formatting the header
//! totals. The UI-independent snapshot machinery lives in [`crate::snapshot`];
//! this module re-exports the types the TUI renders so its import sites stay
//! stable.

use std::collections::{BTreeMap, BTreeSet};

use super::procs::{ProcState, MESSAGE_ROW};

pub use crate::snapshot::{ContainerStatus, InstanceRow, SandboxRow, ServiceRow, Snapshot};

/// Why a request to the daemon got no answer (`tui::poll`), or why the
/// tree's snapshot is stale (`tui::daemon`'s connection is down).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FetchError {
    /// No daemon answered, or the connection broke: the dashboard needs one.
    /// Otherwise the daemon answered with an error.
    pub unreachable: bool,
    pub reason: String,
}

impl FetchError {
    /// The full line, for where there's room (the empty tree).
    pub fn line(&self) -> String {
        if self.unreachable {
            format!("needs devsandbox serve: {}", self.reason)
        } else {
            self.reason.clone()
        }
    }

    /// A few words for the header's stale marker, which shares its row with
    /// the tabs: the connect error's full text is the daemon worker's
    /// status line.
    fn short(&self) -> String {
        if self.unreachable {
            return "needs devsandbox serve".into();
        }
        const MAX: usize = 40;
        match self.reason.char_indices().nth(MAX) {
            Some((cut, _)) => format!("{}…", &self.reason[..cut]),
            None => self.reason.clone(),
        }
    }
}

/// The literal sandbox name of the synthetic group holding instances whose
/// sandbox is not in config. Keyed by this string in the collapsed set.
pub const ORPHANS_NAME: &str = "(not in config)";

/// One visible row of the Instances tree, flattened from
/// (sandboxes, instances, collapsed) by [`visible_nodes`]. Payloads are indices
/// into `Snapshot::sandboxes` / `Snapshot::instances`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Node {
    /// A configured sandbox root (index into `sandboxes`).
    Sandbox(usize),
    /// An instance row (index into `instances`), a child of the sandbox or the
    /// orphan group above it.
    Instance(usize),
    /// The dim "no instances" child under an expanded, empty sandbox (index into
    /// `sandboxes`).
    Empty(usize),
    /// The synthetic `(not in config)` group header, holding orphan instances.
    /// Only emitted when such instances exist.
    Orphans,
    /// A process row under an expanded instance. `instance` indexes into
    /// `instances`; `row` indexes into that instance's [`ProcState::Rows`], or is
    /// [`MESSAGE_ROW`] for the single placeholder row
    /// of a [`ProcState::Message`] (not fetched / not running / error).
    Proc { instance: usize, row: usize },
}

/// The sandbox row instance `inst` groups under: the same machine's sandbox
/// of its name (docs/architecture/daemon/machines.md: a group per (machine, sandbox)), the one
/// from its own root when a machine has two roots defining it. `None`: an
/// orphan.
pub fn group_of(sandboxes: &[SandboxRow], inst: &InstanceRow) -> Option<usize> {
    let same = |s: &SandboxRow| s.machine == inst.machine && s.name == inst.sandbox;
    if inst.machine.is_some() && inst.root.is_some() {
        if let Some(i) = sandboxes.iter().position(|s| same(s) && s.root == inst.root) {
            return Some(i);
        }
    }
    sandboxes.iter().position(same)
}

/// The sandbox rows in tree order: this host's in config order, then each
/// machine's, by machine and name.
pub fn sandbox_order(sandboxes: &[SandboxRow]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..sandboxes.len()).collect();
    // Stable: local rows (`None` sorts first) keep their config order.
    order.sort_by(|&a, &b| {
        let key = |i: usize| match &sandboxes[i].machine {
            None => (None, ""),
            Some(m) => (Some(m.as_str()), sandboxes[i].name.as_str()),
        };
        key(a).cmp(&key(b))
    });
    order
}

/// Flatten the tree into the visible node list, in render/selection order.
///
/// Sandboxes appear in [`sandbox_order`]; an expanded sandbox lists its
/// instances ([`group_of`], in `instances` order) as children, or one
/// [`Node::Empty`] child when it has none. Instances with no sandbox row group
/// under a trailing [`Node::Orphans`] header (keyed by [`ORPHANS_NAME`] in
/// `collapsed`), which only appears when such instances exist: one group for
/// every machine, this host's first (a machine column says whose). A sandbox's
/// key (`remote::sandbox_key`) in `collapsed` hides its children; processes
/// are keyed by `remote::instance_key`.
pub fn visible_nodes(
    sandboxes: &[SandboxRow],
    instances: &[InstanceRow],
    collapsed: &BTreeSet<String>,
    expanded_procs: &BTreeSet<String>,
    procs: &BTreeMap<String, ProcState>,
) -> Vec<Node> {
    let mut nodes = Vec::new();
    let groups: Vec<Option<usize>> = instances.iter().map(|inst| group_of(sandboxes, inst)).collect();

    for si in sandbox_order(sandboxes) {
        nodes.push(Node::Sandbox(si));
        if collapsed.contains(&super::remote::sandbox_key(&sandboxes[si])) {
            continue;
        }
        let mut any = false;
        for (ii, inst) in instances.iter().enumerate() {
            if groups[ii] == Some(si) {
                nodes.push(Node::Instance(ii));
                push_proc_nodes(&mut nodes, ii, inst, expanded_procs, procs);
                any = true;
            }
        }
        if !any {
            nodes.push(Node::Empty(si));
        }
    }

    // Orphans: instances whose sandbox is not configured, grouped at the
    // bottom; this host's before the machines' (`instances`' order).
    let mut orphans: Vec<usize> = (0..instances.len()).filter(|&ii| groups[ii].is_none()).collect();
    orphans.sort_by_key(|&ii| instances[ii].machine.is_some());
    if !orphans.is_empty() {
        nodes.push(Node::Orphans);
        if !collapsed.contains(ORPHANS_NAME) {
            for ii in orphans {
                nodes.push(Node::Instance(ii));
                push_proc_nodes(&mut nodes, ii, &instances[ii], expanded_procs, procs);
            }
        }
    }

    nodes
}

/// Emit the process child nodes for one visible instance, if its procs are
/// expanded. A running container with a fetched forest emits one
/// [`Node::Proc`] per [`ProcRow`](super::procs::ProcRow); every other case
/// (not fetched yet, container not running, fetch error) emits the single
/// [`MESSAGE_ROW`] placeholder.
fn push_proc_nodes(
    nodes: &mut Vec<Node>,
    instance: usize,
    inst: &InstanceRow,
    expanded_procs: &BTreeSet<String>,
    procs: &BTreeMap<String, ProcState>,
) {
    let key = super::remote::instance_key(inst);
    if !expanded_procs.contains(&key) {
        return;
    }
    match procs.get(&key) {
        Some(ProcState::Rows { rows, .. }) => {
            for row in 0..rows.len() {
                nodes.push(Node::Proc { instance, row });
            }
        }
        // Message state, or not fetched yet: one placeholder row.
        _ => nodes.push(Node::Proc { instance, row: MESSAGE_ROW }),
    }
}

/// The dim suffix marking a dispatcher's child in the Instances tree
/// (docs/architecture/automations/dispatchers.md): `⇠ <dispatcher instance name>`, or `⇠ <id> (orphan)`
/// when no instance in `instances` has the recorded owner id any more. `None`
/// for user-created instances. Children stay under their own sandbox group
/// (the tree groups by sandbox) rather than being nested under the dispatcher.
pub fn dispatcher_label(row: &InstanceRow, instances: &[InstanceRow]) -> Option<String> {
    let id = row.dispatcher.as_deref()?;
    // Ids are per host: the owner is on the child's machine.
    Some(match instances.iter().find(|i| i.instance_id == id && i.machine == row.machine) {
        Some(owner) => format!("⇠ {}", owner.name),
        None => format!("⇠ {id} (orphan)"),
    })
}

/// The `N/M running` stats string for a sandbox row: `M` instances, `N` running.
/// Pure so the rendering layer and tests share one format.
pub fn sandbox_stats(instances: usize, running: usize) -> String {
    format!("{running}/{instances} running")
}

/// Humanize an elapsed-seconds duration: `3d4h`, `2h05m`, `12m`, `40s`.
pub fn humanize_secs(secs: u64) -> String {
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    let mins = (secs % 3_600) / 60;
    if days > 0 {
        format!("{days}d{hours}h")
    } else if hours > 0 {
        format!("{hours}h{mins:02}m")
    } else if mins > 0 {
        format!("{mins}m")
    } else {
        format!("{secs}s")
    }
}

/// How stale a snapshot may be before the header flags it, in seconds. The
/// daemon pushes every collection, changed or not, every 2s
/// (`serve::snapshots`), and the age counts from arrival; 10s means
/// collection is slow or failing.
pub const STALE_AFTER_SECS: u64 = 10;

/// Right-aligned totals summary for the header, e.g.
/// `3 sandboxes · 1 running / 2 stopped · 2 service containers · docker 24.0.7`.
/// A trailing ` (stale Ns)` is appended when `collected_at` is older than
/// [`STALE_AFTER_SECS`], or ` (stale Ns: why)` as soon as the last refresh
/// failed (`error`, e.g. the daemon is down). Pure over the snapshot so it is
/// unit-testable; `age` is passed in rather than read from the clock.
pub fn totals_line(snapshot: &Snapshot, age: std::time::Duration, error: Option<&FetchError>) -> String {
    let running = snapshot
        .instances
        .iter()
        .filter(|r| matches!(r.status, ContainerStatus::Running(_)))
        .count();
    // Not-yet-listed (`Unknown`) rows count as neither.
    let stopped = snapshot
        .instances
        .iter()
        .filter(|r| matches!(r.status, ContainerStatus::Exited(_) | ContainerStatus::Missing))
        .count();
    // Service containers that actually exist (any non-Missing backing container).
    let service_containers: usize = snapshot
        .services
        .iter()
        .flat_map(|s| &s.containers)
        .filter(|(_, st)| !matches!(st, ContainerStatus::Missing | ContainerStatus::Unknown))
        .count();
    let version = snapshot.runtime_version.as_deref().unwrap_or("?");

    let mut line = format!(
        "{} {} · {running} running / {stopped} stopped · {service_containers} service {} · {} {version}",
        snapshot.sandbox_count,
        plural(snapshot.sandbox_count, "sandbox", "sandboxes"),
        plural(service_containers, "container", "containers"),
        snapshot.runtime_name,
    );
    match error {
        Some(e) => line.push_str(&format!(" (stale {}s: {})", age.as_secs(), e.short())),
        None if age.as_secs() >= STALE_AFTER_SECS => line.push_str(&format!(" (stale {}s)", age.as_secs())),
        None => {}
    }
    line
}

/// The header's yellow identity line, e.g. `2 sandboxes use /root (U to
/// update)`: the sandboxes (every row, a machine's included) with at least
/// one un-acked identity note, and the key opening the offer. `None` when
/// there's none.
pub fn identity_line(snapshot: &Snapshot) -> Option<String> {
    let n = snapshot
        .sandboxes
        .iter()
        .filter(|s| s.identity.iter().any(|note| !note.acked))
        .count();
    (n > 0).then(|| format!("{n} {} /root (U to update)", plural(n, "sandbox uses", "sandboxes use")))
}

/// `n singular` / `n plural` word choice (the count itself is rendered by the
/// caller; this only returns the noun).
fn plural<'a>(n: usize, singular: &'a str, plural: &'a str) -> &'a str {
    if n == 1 { singular } else { plural }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn humanize_secs_scales() {
        assert_eq!(humanize_secs(0), "0s");
        assert_eq!(humanize_secs(40), "40s");
        assert_eq!(humanize_secs(60), "1m");
        assert_eq!(humanize_secs(12 * 60), "12m");
        assert_eq!(humanize_secs(2 * 3600 + 5 * 60), "2h05m");
        assert_eq!(humanize_secs(3 * 86_400 + 4 * 3600 + 30 * 60), "3d4h");
    }

    fn inst(name: &str, status: ContainerStatus) -> InstanceRow {
        InstanceRow {
            name: name.into(),
            sandbox: "s".into(),
            container: format!("devsandbox-{name}"),
            status,
            uptime_secs: 0,
            cpu: None,
            mem: None,
            folder: "/f".into(),
            worktree: false,
            services: Vec::new(),
            workspace: "/w".into(),
            remote_user: None,
            remote_env_len: 0,
            base_folder: "/f".into(),
            drift: false,
            instance_id: String::new(),
            dispatcher: None,
            done: false,
            phase: None,
            root: None,
            machine: None,
        }
    }

    fn svc_row(containers: Vec<ContainerStatus>) -> ServiceRow {
        ServiceRow {
            name: "db".into(),
            scope: "global".into(),
            source: "image x".into(),
            ports: Vec::new(),
            containers: containers
                .into_iter()
                .enumerate()
                .map(|(i, s)| (format!("c{i}"), s))
                .collect(),
            used_by: Vec::new(),
            env_len: 0,
            command: None,
            config_hash: String::new(),
            drift: false,
            root: None,
            machine: None,
        }
    }

    fn snap(
        instances: Vec<InstanceRow>,
        services: Vec<ServiceRow>,
        sandbox_count: usize,
        runtime_version: Option<&str>,
    ) -> Snapshot {
        Snapshot {
            instances,
            sandboxes: Vec::new(),
            services,
            sandbox_count,
            runtime_name: "docker".into(),
            runtime_version: runtime_version.map(str::to_string),
            collected_at: Instant::now(),
            stats: true,
            error: None,
        }
    }

    #[test]
    fn totals_line_counts_running_stopped_and_service_containers() {
        let s = snap(
            vec![
                inst("a", ContainerStatus::Running("Up".into())),
                inst("b", ContainerStatus::Exited("Exited".into())),
                inst("c", ContainerStatus::Missing),
            ],
            vec![svc_row(vec![
                ContainerStatus::Running("Up".into()),
                ContainerStatus::Missing,
            ])],
            3,
            Some("24.0.7"),
        );
        assert_eq!(
            totals_line(&s, std::time::Duration::from_secs(0), None),
            "3 sandboxes · 1 running / 2 stopped · 1 service container · docker 24.0.7",
        );
    }

    /// A disk-only snapshot (statuses `Unknown`) claims nothing running or stopped.
    #[test]
    fn totals_line_skips_unlisted_rows() {
        let s = snap(
            vec![inst("a", ContainerStatus::Unknown)],
            vec![svc_row(vec![ContainerStatus::Unknown])],
            1,
            None,
        );
        assert_eq!(
            totals_line(&s, std::time::Duration::from_secs(0), None),
            "1 sandbox · 0 running / 0 stopped · 0 service containers · docker ?",
        );
    }

    #[test]
    fn totals_line_pluralizes_and_marks_missing_version() {
        let s = snap(vec![inst("a", ContainerStatus::Running("Up".into()))], Vec::new(), 1, None);
        assert_eq!(
            totals_line(&s, std::time::Duration::from_secs(0), None),
            "1 sandbox · 1 running / 0 stopped · 0 service containers · docker ?",
        );
    }

    #[test]
    fn totals_line_flags_stale_data() {
        let s = snap(Vec::new(), Vec::new(), 0, Some("24.0.7"));
        let out = totals_line(&s, std::time::Duration::from_secs(STALE_AFTER_SECS + 2), None);
        assert!(out.ends_with(" (stale 12s)"), "got: {out}");
        // Fresh data has no staleness suffix.
        let fresh = totals_line(&s, std::time::Duration::from_secs(1), None);
        assert!(!fresh.contains("stale"), "got: {fresh}");
    }

    /// A failed refresh marks the kept snapshot at once, with why: the
    /// daemon being gone in a few words, its error cut short.
    #[test]
    fn totals_line_says_why_a_refresh_failed() {
        let s = snap(Vec::new(), Vec::new(), 0, Some("24.0.7"));
        let down = FetchError { unreachable: true, reason: "devsandbox serve did not answer on /run/x within 5s".into() };
        let out = totals_line(&s, std::time::Duration::from_secs(3), Some(&down));
        assert!(out.ends_with(" (stale 3s: needs devsandbox serve)"), "got: {out}");
        assert_eq!(down.line(), "needs devsandbox serve: devsandbox serve did not answer on /run/x within 5s");

        let refused = FetchError { unreachable: false, reason: "no directory /gone".into() };
        let out = totals_line(&s, std::time::Duration::from_secs(12), Some(&refused));
        assert!(out.ends_with(" (stale 12s: no directory /gone)"), "got: {out}");
        assert_eq!(refused.line(), "no directory /gone");
        let long = FetchError { unreachable: false, reason: "é".repeat(50) };
        let out = totals_line(&s, std::time::Duration::from_secs(12), Some(&long));
        assert!(out.ends_with(&format!(": {}…)", "é".repeat(40))), "got: {out}");
    }

    fn sb(name: &str) -> SandboxRow {
        SandboxRow {
            name: name.into(),
            source: "image x".into(),
            folder: None,
            services: Vec::new(),
            extends: Vec::new(),
            config_hash: "hash".into(),
            build_hash: String::new(),
            issues: Vec::new(),
            identity: Vec::new(),
            dispatcher: false,
            worktree_branch: None,
            root: None,
            machine: None,
        }
    }

    #[test]
    fn identity_line_counts_sandboxes_with_unacked_notes() {
        use crate::snapshot::IdentityNote;
        let note = |acked| IdentityNote { id: "i".into(), text: "t".into(), acked };
        let mut s = snap(Vec::new(), Vec::new(), 0, None);
        s.sandboxes = vec![sb("clean")];
        assert_eq!(identity_line(&s), None);
        let mut acked = sb("acked");
        acked.identity = vec![note(true)];
        s.sandboxes.push(acked);
        assert_eq!(identity_line(&s), None, "acked notes don't count");
        let mut one = sb("one");
        one.identity = vec![note(true), note(false)];
        s.sandboxes.push(one.clone());
        assert_eq!(identity_line(&s).as_deref(), Some("1 sandbox uses /root (U to update)"));
        s.sandboxes.push(one);
        assert_eq!(identity_line(&s).as_deref(), Some("2 sandboxes use /root (U to update)"));
    }

    #[test]
    fn dispatcher_label_names_the_owner_or_marks_an_orphan() {
        let mut owner = inst("pr-dispatcher", ContainerStatus::Missing);
        owner.instance_id = "d-1".into();
        let mut child = inst("web-pr-1", ContainerStatus::Missing);
        child.instance_id = "c-1".into();
        child.dispatcher = Some("d-1".into());
        let mut orphan = inst("web-pr-2", ContainerStatus::Missing);
        orphan.dispatcher = Some("gone-7".into());
        let rows = vec![owner.clone(), child.clone(), orphan.clone()];
        assert_eq!(dispatcher_label(&owner, &rows), None, "user-created");
        assert_eq!(dispatcher_label(&child, &rows).as_deref(), Some("⇠ pr-dispatcher"));
        assert_eq!(dispatcher_label(&orphan, &rows).as_deref(), Some("⇠ gone-7 (orphan)"));
        // The dispatcher itself removed: its children turn orphan.
        assert_eq!(dispatcher_label(&child, &rows[1..]).as_deref(), Some("⇠ d-1 (orphan)"));
    }

    /// An instance in a given sandbox (extends the `inst` helper, which pins
    /// sandbox to "s").
    fn inst_in(name: &str, sandbox: &str) -> InstanceRow {
        let mut row = inst(name, ContainerStatus::Missing);
        row.sandbox = sandbox.into();
        row
    }

    fn collapsed(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// The no-processes-expanded argument pair for `visible_nodes` callers that
    /// only exercise the sandbox/instance levels.
    fn no_procs() -> (BTreeSet<String>, BTreeMap<String, ProcState>) {
        (BTreeSet::new(), BTreeMap::new())
    }

    #[test]
    fn visible_nodes_expands_sandboxes_with_instances() {
        let sandboxes = vec![sb("a"), sb("b")];
        let instances = vec![
            inst_in("a1", "a"),
            inst_in("b1", "b"),
            inst_in("a2", "a"),
        ];
        let (ep, pm) = no_procs();
        let nodes = visible_nodes(&sandboxes, &instances, &BTreeSet::new(), &ep, &pm);
        assert_eq!(
            nodes,
            vec![
                Node::Sandbox(0),
                Node::Instance(0), // a1
                Node::Instance(2), // a2 (instances order preserved)
                Node::Sandbox(1),
                Node::Instance(1), // b1
            ],
        );
    }

    #[test]
    fn visible_nodes_empty_sandbox_gets_marker() {
        let sandboxes = vec![sb("a")];
        let (ep, pm) = no_procs();
        let nodes = visible_nodes(&sandboxes, &[], &BTreeSet::new(), &ep, &pm);
        assert_eq!(nodes, vec![Node::Sandbox(0), Node::Empty(0)]);
    }

    #[test]
    fn visible_nodes_collapsed_sandbox_hides_children() {
        let sandboxes = vec![sb("a"), sb("b")];
        let instances = vec![inst_in("a1", "a"), inst_in("b1", "b")];
        let (ep, pm) = no_procs();
        let nodes = visible_nodes(&sandboxes, &instances, &collapsed(&["a"]), &ep, &pm);
        // a is collapsed (no children, not even a marker); b stays expanded.
        assert_eq!(
            nodes,
            vec![Node::Sandbox(0), Node::Sandbox(1), Node::Instance(1)],
        );
    }

    #[test]
    fn visible_nodes_orphans_group_at_bottom() {
        let sandboxes = vec![sb("a")];
        let instances = vec![inst_in("a1", "a"), inst_in("x1", "gone")];
        let (ep, pm) = no_procs();
        let nodes = visible_nodes(&sandboxes, &instances, &BTreeSet::new(), &ep, &pm);
        assert_eq!(
            nodes,
            vec![
                Node::Sandbox(0),
                Node::Instance(0),
                Node::Orphans,
                Node::Instance(1),
            ],
        );

        // Collapsing the orphan group hides its children but keeps the header.
        let nodes = visible_nodes(&sandboxes, &instances, &collapsed(&[ORPHANS_NAME]), &ep, &pm);
        assert_eq!(
            nodes,
            vec![Node::Sandbox(0), Node::Instance(0), Node::Orphans],
        );
    }

    #[test]
    fn visible_nodes_no_orphan_group_when_all_configured() {
        let sandboxes = vec![sb("a")];
        let instances = vec![inst_in("a1", "a")];
        let (ep, pm) = no_procs();
        let nodes = visible_nodes(&sandboxes, &instances, &BTreeSet::new(), &ep, &pm);
        assert!(!nodes.contains(&Node::Orphans));
    }

    fn proc_row(pid: &str) -> super::super::procs::ProcRow {
        super::super::procs::ProcRow { pid: pid.into(), depth: 0, args: "x".into() }
    }

    #[test]
    fn visible_nodes_emits_proc_rows_under_expanded_instance() {
        let sandboxes = vec![sb("a")];
        let instances = vec![inst_in("a1", "a")];
        let expanded: BTreeSet<String> = ["a1".to_string()].into_iter().collect();
        let mut procs = BTreeMap::new();
        procs.insert(
            "a1".to_string(),
            ProcState::Rows { rows: vec![proc_row("1"), proc_row("2")], signalable: true, sessions: None },
        );
        let nodes = visible_nodes(&sandboxes, &instances, &BTreeSet::new(), &expanded, &procs);
        assert_eq!(
            nodes,
            vec![
                Node::Sandbox(0),
                Node::Instance(0),
                Node::Proc { instance: 0, row: 0 },
                Node::Proc { instance: 0, row: 1 },
            ],
        );
    }

    #[test]
    fn visible_nodes_message_row_when_not_fetched_or_error() {
        let sandboxes = vec![sb("a")];
        let instances = vec![inst_in("a1", "a")];
        let expanded: BTreeSet<String> = ["a1".to_string()].into_iter().collect();

        // Not fetched yet: one placeholder row.
        let nodes =
            visible_nodes(&sandboxes, &instances, &BTreeSet::new(), &expanded, &BTreeMap::new());
        assert_eq!(nodes.last(), Some(&Node::Proc { instance: 0, row: MESSAGE_ROW }));

        // Message state (e.g. not running / error): still one placeholder row.
        let mut procs = BTreeMap::new();
        procs.insert("a1".to_string(), ProcState::Message("(not running)".into()));
        let nodes = visible_nodes(&sandboxes, &instances, &BTreeSet::new(), &expanded, &procs);
        assert_eq!(
            nodes,
            vec![
                Node::Sandbox(0),
                Node::Instance(0),
                Node::Proc { instance: 0, row: MESSAGE_ROW },
            ],
        );
    }

    #[test]
    fn visible_nodes_collapsed_sandbox_hides_expanded_procs() {
        let sandboxes = vec![sb("a")];
        let instances = vec![inst_in("a1", "a")];
        let expanded: BTreeSet<String> = ["a1".to_string()].into_iter().collect();
        let mut procs = BTreeMap::new();
        procs.insert(
            "a1".to_string(),
            ProcState::Rows { rows: vec![proc_row("1")], signalable: true, sessions: None },
        );
        // Sandbox collapsed → its instance and procs are hidden entirely.
        let nodes = visible_nodes(&sandboxes, &instances, &collapsed(&["a"]), &expanded, &procs);
        assert_eq!(nodes, vec![Node::Sandbox(0)]);
    }

    #[test]
    fn sandbox_stats_formats_running_over_total() {
        assert_eq!(sandbox_stats(0, 0), "0/0 running");
        assert_eq!(sandbox_stats(3, 1), "1/3 running");
    }
}
