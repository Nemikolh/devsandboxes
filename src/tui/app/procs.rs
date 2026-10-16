//! Per-instance process rows: expansion, selection, signalling, and which
//! instances the background proc fetch should poll.

use std::collections::BTreeMap;

use crate::tui::data::{ContainerStatus, Node};
use crate::tui::procs::ProcState;
use crate::tui::remote::{self, instance_key};

use super::{App, Tab};

/// A POSIX signal the process-row shortcuts can send. Kept small and typed so
/// the key handler, status text, and the event loop share one source of truth.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    Term,
    Kill,
}

impl Signal {
    /// The name `instances.signal` takes (the daemon maps it to the number
    /// `kill -<n>` gets).
    pub fn wire(self) -> &'static str {
        match self {
            Signal::Term => "TERM",
            Signal::Kill => "KILL",
        }
    }

    /// Display name for status messages.
    pub fn name(self) -> &'static str {
        match self {
            Signal::Term => "SIGTERM",
            Signal::Kill => "SIGKILL",
        }
    }
}

/// A pending `kill` the event loop should send as `instances.signal`: signal
/// `pid` inside `instance`'s container (the daemon resolves it). Set by the
/// SIGTERM/SIGKILL process-row shortcuts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingSignal {
    /// State key (a snapshot row name).
    pub instance: String,
    pub pid: String,
    pub signal: Signal,
}

impl App {
    /// The selected process row as `(instance, pid)`, when the cursor is on a
    /// real process (not a `(message)` placeholder) with a numeric pid. Backs the
    /// SIGTERM/SIGKILL shortcuts; `None` on every other selection.
    fn selected_proc(&self) -> Option<(String, String)> {
        let snapshot = self.snapshot.as_ref()?;
        let Node::Proc { instance, row } = self.selected_node()? else {
            return None;
        };
        if row == crate::tui::procs::MESSAGE_ROW {
            return None;
        }
        let inst = snapshot.instances.get(instance)?;
        let ProcState::Rows { rows, signalable } = self.procs.get(&instance_key(inst))? else {
            return None;
        };
        // A `top` fallback listing has host-namespace pids that `exec kill` can't
        // reach; don't offer signals for it.
        if !signalable {
            return None;
        }
        let proc = rows.get(row)?;
        // Defensive: `parse_top` only yields numeric pids, but never hand a
        // non-numeric token to `kill`.
        proc.pid.parse::<u32>().ok()?;
        Some((instance_key(inst), proc.pid.clone()))
    }

    /// Queue `signal` for the selected process, for the event loop to send to
    /// the daemon. No-op when the cursor is not on a signalable process.
    pub(super) fn signal_selected_proc(&mut self, signal: Signal) {
        let Some((instance, pid)) = self.selected_proc() else {
            return;
        };
        self.status = Some(format!("sending {} to pid {pid}…", signal.name()));
        self.pending_signal = Some(PendingSignal {
            instance,
            pid,
            signal,
        });
    }

    /// Take the pending process signal for the event loop to send, if any.
    pub fn take_pending_signal(&mut self) -> Option<PendingSignal> {
        self.pending_signal.take()
    }

    /// Whether the cursor is on a process row (Instances tab). Drives the help
    /// bar's process-specific legend.
    pub fn on_proc_row(&self) -> bool {
        self.tab == Tab::Instances && matches!(self.selected_node(), Some(Node::Proc { .. }))
    }

    /// Whether the selected process row can be signalled (its pids are
    /// container-namespace). False for a `(message)` row or a `top` fallback
    /// listing; drives whether the help bar advertises SIGTERM/SIGKILL.
    pub fn proc_row_signalable(&self) -> bool {
        self.selected_proc().is_some()
    }

    /// Agent-process count for a cached instance: the number of forest rows whose
    /// args name a known coding agent (see [`crate::tui::procs::is_agent`]). `None`
    /// when the instance has no fetched forest yet (not running, error, or the
    /// fetch is still in flight), which the Detail panel renders as `…`/`-`.
    pub fn agent_count(&self, instance: &str) -> Option<usize> {
        match self.procs.get(instance) {
            // Run rows (`RUN_PID`) repeat a live agent's argv: count processes only.
            Some(ProcState::Rows { rows, .. }) => Some(
                rows.iter()
                    .filter(|r| r.pid != crate::tui::procs::RUN_PID && crate::tui::procs::is_agent(&r.args))
                    .count(),
            ),
            _ => None,
        }
    }

    /// The selected instance's name as a fetch target, but only when its
    /// container is running. Drives the on-demand proc fetch that backs the
    /// Detail agent count for instances that aren't expanded. `None` on non-
    /// instance nodes or a non-running selection.
    fn selected_running_instance(&self) -> Option<String> {
        let snapshot = self.snapshot.as_ref()?;
        let idx = self.selected_instance_index()?;
        let row = snapshot.instances.get(idx)?;
        let reachable = !remote::unreachable(&self.machines, row.machine.as_deref());
        (reachable && matches!(row.status, ContainerStatus::Running(_))).then(|| instance_key(row))
    }

    /// Consume the "fetch now" signal set on expand.
    pub fn take_needs_proc_fetch(&mut self) -> bool {
        std::mem::take(&mut self.needs_proc_fetch)
    }

    /// The process-list fetch targets (`instances.procs` names): every
    /// expanded instance whose container is running. Expanded instances that are
    /// not running (or absent from the snapshot) get a `(not running)` message
    /// row stored directly here — no fetch — and are omitted from the returned
    /// list. Instances no longer in the snapshot are dropped from the cache. The
    /// selected running instance is appended (deduped) so the Detail agent count
    /// has a fresh forest even when its process layer isn't expanded. Empty off
    /// the Instances tab: nothing there shows processes, and each target costs
    /// two `docker exec`s.
    pub fn proc_fetch_targets(&mut self) -> Vec<String> {
        if self.tab != Tab::Instances {
            return Vec::new();
        }
        let Some(snapshot) = &self.snapshot else {
            return Vec::new();
        };
        let mut targets = Vec::new();
        let mut not_running: Vec<(String, &str)> = Vec::new();
        for name in &self.expanded_procs {
            match remote::find_instance(snapshot, name) {
                // A machine that isn't connected can't be asked.
                Some(row) if remote::unreachable(&self.machines, row.machine.as_deref()) => {
                    not_running.push((name.clone(), "(machine unreachable)"));
                }
                Some(row) if matches!(row.status, ContainerStatus::Running(_)) => {
                    targets.push(name.clone());
                }
                _ => not_running.push((name.clone(), "(not running)")),
            }
        }
        for (name, why) in not_running {
            self.procs.insert(name, ProcState::Message(why.to_string()));
        }
        // The selected running instance is fetched too (for the Detail agent
        // count), even when its process layer isn't expanded. Deduped against the
        // expanded targets so it's never fetched twice in one batch.
        if let Some(name) = self.selected_running_instance() {
            if !targets.contains(&name) {
                targets.push(name);
            }
        }
        targets
    }

    /// Merge a completed proc fetch into the cache, overwriting only fetched
    /// keys (other instances' cached rows are left untouched).
    pub fn apply_proc_fetch(&mut self, fetched: BTreeMap<String, ProcState>) {
        self.procs.extend(fetched);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::test_support::*;
    use crate::tui::app::*;

    /// Off the Instances tab nothing shows processes, so nothing is fetched;
    /// coming back asks for a fetch right away.
    #[test]
    fn proc_fetch_only_on_instances_tab() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with_status(1, running()));
        app.on_key(key(KeyCode::Down)); // onto inst0
        assert_eq!(app.proc_fetch_targets().len(), 1);
        app.set_tab(Tab::Ports);
        assert!(app.proc_fetch_targets().is_empty());
        app.take_needs_proc_fetch();
        app.set_tab(Tab::Instances);
        assert!(app.take_needs_proc_fetch());
        assert_eq!(app.proc_fetch_targets().len(), 1);
    }

    /// Processes are keyed per machine: the fetch targets are keys (the
    /// poll asks each machine for its own), a machine that isn't connected
    /// isn't asked, and a signal names its instance's key.
    #[test]
    fn procs_are_keyed_per_machine() {
        use crate::serve::snapshots::LinkState;
        let mut app = machine_app();
        app.expanded_procs.extend(["inst0".to_string(), "box/inst0".to_string()]);
        app.selected[Tab::Instances.index()] = 0;
        let mut targets = app.proc_fetch_targets();
        targets.sort();
        assert_eq!(targets, ["box/inst0", "inst0"]);
        app.machines[0].state = LinkState::Unreachable;
        assert_eq!(app.proc_fetch_targets(), ["inst0"]);
        assert_eq!(app.procs["box/inst0"], ProcState::Message("(machine unreachable)".into()));
        app.machines[0].state = LinkState::Connected;

        expand_with_rows(&mut app, "box/inst1", &["42"]);
        let at = app.visible_nodes().iter().position(|n| *n == Node::Proc { instance: 3, row: 0 }).unwrap();
        app.selected[Tab::Instances.index()] = at;
        app.signal_selected_proc(Signal::Term);
        assert_eq!(
            app.take_pending_signal(),
            Some(PendingSignal { instance: "box/inst1".into(), pid: "42".into(), signal: Signal::Term })
        );
    }

    /// A pushed snapshot is installed as is: the daemon carries cpu/mem over,
    /// so a row it left without them isn't filled from the last one.
    #[test]
    fn snapshots_are_installed_without_carrying_cpu_mem() {
        let mut app = new_app();
        let mut full = snapshot_with_status(2, running());
        for row in &mut full.instances {
            row.cpu = Some("1%".into());
            row.mem = Some("1MiB".into());
        }
        app.set_snapshot(full);
        let mut next = snapshot_with_status(2, running());
        next.stats = false;
        next.instances[0].cpu = Some("2%".into());
        next.instances[1].status = ContainerStatus::Exited("Exited (0)".into());
        app.set_snapshot(next);
        let rows = &app.snapshot.as_ref().unwrap().instances;
        assert_eq!(rows[0].cpu.as_deref(), Some("2%"));
        assert_eq!(rows[0].mem, None, "not filled from the last snapshot");
        assert_eq!(rows[1].cpu, None, "stopped: no stale cpu");
    }

    #[test]
    fn instance_shortcuts_disabled_on_proc_row() {
        let mut app = app_on_proc_row(&["10"]);
        // None of the instance-forwarding shortcuts fire on a process row.
        for code in [
            KeyCode::Char('s'),
            KeyCode::Char('o'),
            KeyCode::Char('r'),
            KeyCode::Char('l'),
            KeyCode::Char('e'),
            KeyCode::Enter,
            KeyCode::Char('T'),
        ] {
            app.on_key(key(code));
        }
        assert!(app.stopping.is_empty());
        assert!(app.take_pending_action().is_none());
        assert!(app.take_pending_signal().is_none());
        assert!(matches!(app.modal, Modal::None));
        assert!(app.prompt.is_none());
        assert!(app.terms.is_empty());
    }

    #[test]
    fn t_and_shift_k_signal_the_selected_proc() {
        let mut app = app_on_proc_row(&["10"]);
        app.on_key(key(KeyCode::Char('t')));
        assert_eq!(
            app.take_pending_signal(),
            Some(PendingSignal {
                instance: "inst0".into(),
                pid: "10".into(),
                signal: Signal::Term,
            }),
        );
        app.on_key(key(KeyCode::Char('K')));
        assert_eq!(
            app.take_pending_signal(),
            Some(PendingSignal {
                instance: "inst0".into(),
                pid: "10".into(),
                signal: Signal::Kill,
            }),
        );
    }

    #[test]
    fn signal_noop_on_message_placeholder_row() {
        use crate::tui::procs::ProcState;
        let mut app = new_app();
        app.set_snapshot(snapshot_with_status(1, running()));
        app.expanded_procs.insert("inst0".into());
        app.procs
            .insert("inst0".into(), ProcState::Message("(not running)".into()));
        app.on_key(key(KeyCode::Down)); // inst0
        app.on_key(key(KeyCode::Down)); // message row
        assert!(matches!(app.selected_node(), Some(Node::Proc { .. })));
        app.on_key(key(KeyCode::Char('t')));
        assert!(app.take_pending_signal().is_none());
    }

    #[test]
    fn help_bar_switches_to_signal_legend_on_proc_row() {
        let app = app_on_proc_row(&["10"]);
        assert!(app.on_proc_row());
        assert!(app.proc_row_signalable());
        // Instance row: not a proc.
        let mut app2 = new_app();
        app2.set_snapshot(snapshot_with_status(1, running()));
        app2.on_key(key(KeyCode::Down)); // inst0
        assert!(!app2.on_proc_row());
    }

    #[test]
    fn top_fallback_listing_is_not_signalable() {
        use crate::tui::procs::{ProcRow, ProcState};
        let mut app = new_app();
        app.set_snapshot(snapshot_with_status(1, running()));
        // Non-signalable rows: a host-side `top` fallback (pids aren't reachable
        // by `exec kill`).
        app.expanded_procs.insert("inst0".into());
        app.procs.insert(
            "inst0".into(),
            ProcState::Rows {
                rows: vec![ProcRow { pid: "660011".into(), depth: 0, args: "x".into() }],
                signalable: false,
            },
        );
        app.on_key(key(KeyCode::Down)); // inst0
        app.on_key(key(KeyCode::Down)); // proc row
        assert!(app.on_proc_row());
        // Help bar drops the signal keys, and the keys are inert.
        assert!(!app.proc_row_signalable());
        app.on_key(key(KeyCode::Char('t')));
        app.on_key(key(KeyCode::Char('K')));
        assert!(app.take_pending_signal().is_none());
    }

    #[test]
    fn proc_fetch_targets_running_and_not_running() {
        use crate::tui::data::{ContainerStatus, InstanceRow, SandboxRow};
        let mut app = new_app();
        let mk = |name: &str, status: ContainerStatus| InstanceRow {
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
        };
        let snap = Snapshot {
            instances: vec![
                mk("up", ContainerStatus::Running("Up".into())),
                mk("down", ContainerStatus::Exited("Exited".into())),
            ],
            sandboxes: vec![SandboxRow {
                name: "s".into(),
                source: "image x".into(),
                folder: None,
                services: Vec::new(),
                extends: Vec::new(),
                config_hash: "h".into(),
                build_hash: String::new(),
                issues: Vec::new(),
                dispatcher: false,
                worktree_branch: None,
                root: None,
                machine: None,
            }],
            services: Vec::new(),
            sandbox_count: 1,
            runtime_name: "docker".into(),
            runtime_version: None,
            collected_at: std::time::Instant::now(),
            stats: true,
            error: None,
        };
        app.set_snapshot(snap);
        app.expanded_procs.insert("up".into());
        app.expanded_procs.insert("down".into());

        let targets = app.proc_fetch_targets();
        assert_eq!(targets, vec!["up".to_string()]);
        // Non-running instance got a (not running) message row, no fetch.
        assert_eq!(
            app.procs.get("down"),
            Some(&crate::tui::procs::ProcState::Message("(not running)".into())),
        );
    }

    #[test]
    fn run_rows_are_not_signalable() {
        let mut app = app_on_proc_row(&[crate::tui::procs::RUN_PID]);
        assert!(app.on_proc_row());
        assert!(!app.proc_row_signalable());
        app.on_key(key(KeyCode::Char('t')));
        assert!(app.take_pending_signal().is_none());
    }

    #[test]
    fn agent_count_counts_agent_rows_in_cache() {
        use crate::tui::procs::{ProcRow, ProcState};
        let mut app = new_app();
        // Nothing cached → unknown.
        assert_eq!(app.agent_count("x"), None);

        let rows = vec![
            ProcRow { pid: "1".into(), depth: 0, args: "/sbin/init".into() },
            ProcRow { pid: "2".into(), depth: 1, args: "node /usr/local/bin/claude".into() },
            ProcRow { pid: "3".into(), depth: 1, args: "claude --resume".into() },
            // A run row repeating an agent's argv isn't another agent.
            ProcRow {
                pid: crate::tui::procs::RUN_PID.into(),
                depth: 0,
                args: "1790000000-a1b2 running claude -p x".into(),
            },
        ];
        app.procs.insert("x".into(), ProcState::Rows { rows, signalable: true });
        assert_eq!(app.agent_count("x"), Some(2));

        // A message state (not running / error) has no countable forest.
        app.procs.insert("y".into(), ProcState::Message("(not running)".into()));
        assert_eq!(app.agent_count("y"), None);
    }

    #[test]
    fn proc_fetch_targets_includes_selected_running_instance() {
        use crate::tui::data::{ContainerStatus, InstanceRow, SandboxRow};
        let mut app = new_app();
        let row = InstanceRow {
            name: "up".into(),
            sandbox: "s".into(),
            container: "devsandbox-up".into(),
            status: ContainerStatus::Running("Up".into()),
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
        };
        app.set_snapshot(Snapshot {
            instances: vec![row],
            sandboxes: vec![SandboxRow {
                name: "s".into(),
                source: "image x".into(),
                folder: None,
                services: Vec::new(),
                extends: Vec::new(),
                config_hash: "h".into(),
                build_hash: String::new(),
                issues: Vec::new(),
                dispatcher: false,
                worktree_branch: None,
                root: None,
                machine: None,
            }],
            services: Vec::new(),
            sandbox_count: 1,
            runtime_name: "docker".into(),
            runtime_version: None,
            collected_at: std::time::Instant::now(),
            stats: true,
            error: None,
        });

        // Cursor on the sandbox node: no instance selected, nothing expanded.
        assert!(app.proc_fetch_targets().is_empty());

        // Move onto the running instance: it becomes a fetch target for the
        // Detail agent count, without being expanded.
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.proc_fetch_targets(), vec!["up".to_string()]);
        assert!(app.expanded_procs.is_empty());
    }
}
