//! Per-instance process rows: expansion, selection, signalling, and which
//! instances the background proc fetch should poll.

use std::collections::BTreeMap;

use crate::tui::data::{ContainerStatus, Node};
use crate::tui::procs::ProcState;

use super::{App, Tab};

/// A POSIX signal the process-row shortcuts can send. Kept small and typed so
/// the key handler, status text, and the event loop share one source of truth.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    Term,
    Kill,
}

impl Signal {
    /// The numeric signal passed to `kill -<n>` (portable across busybox and
    /// coreutils, unlike some name spellings).
    pub fn num(self) -> i32 {
        match self {
            Signal::Term => 15,
            Signal::Kill => 9,
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

/// A pending `kill` the event loop should run on a background thread: signal
/// `pid` inside `container`. Set by the SIGTERM/SIGKILL process-row shortcuts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingSignal {
    pub container: String,
    pub pid: String,
    pub signal: Signal,
}

impl App {
    /// The selected process row as `(container, pid)`, when the cursor is on a
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
        let ProcState::Rows { rows, signalable } = self.procs.get(&inst.name)? else {
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
        Some((inst.container.clone(), proc.pid.clone()))
    }

    /// Queue `signal` for the selected process, for the event loop to send on a
    /// background thread. No-op when the cursor is not on a signalable process.
    pub(super) fn signal_selected_proc(&mut self, signal: Signal) {
        let Some((container, pid)) = self.selected_proc() else {
            return;
        };
        self.status = Some(format!("sending {} to pid {pid}…", signal.name()));
        self.pending_signal = Some(PendingSignal {
            container,
            pid,
            signal,
        });
    }

    /// Take the pending process signal for the event loop to spawn, if any.
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
            Some(ProcState::Rows { rows, .. }) => {
                Some(rows.iter().filter(|r| crate::tui::procs::is_agent(&r.args)).count())
            }
            _ => None,
        }
    }

    /// The selected instance as a `(name, container)` fetch target, but only when
    /// its container is running. Drives the on-demand proc fetch that backs the
    /// Detail agent count for instances that aren't expanded. `None` on non-
    /// instance nodes or a non-running selection.
    fn selected_running_instance(&self) -> Option<(String, String)> {
        let snapshot = self.snapshot.as_ref()?;
        let idx = self.selected_instance_index()?;
        let row = snapshot.instances.get(idx)?;
        matches!(row.status, ContainerStatus::Running(_))
            .then(|| (row.name.clone(), row.container.clone()))
    }

    /// Consume the "fetch now" signal set on expand.
    pub fn take_needs_proc_fetch(&mut self) -> bool {
        std::mem::take(&mut self.needs_proc_fetch)
    }

    /// The process-list fetch targets: `(instance name, container)` for every
    /// expanded instance whose container is running. Expanded instances that are
    /// not running (or absent from the snapshot) get a `(not running)` message
    /// row stored directly here — no fetch — and are omitted from the returned
    /// list. Instances no longer in the snapshot are dropped from the cache. The
    /// selected running instance is appended (deduped) so the Detail agent count
    /// has a fresh forest even when its process layer isn't expanded.
    pub fn proc_fetch_targets(&mut self) -> Vec<(String, String)> {
        let Some(snapshot) = &self.snapshot else {
            return Vec::new();
        };
        let mut targets = Vec::new();
        let mut not_running: Vec<String> = Vec::new();
        for name in &self.expanded_procs {
            match snapshot.instances.iter().find(|r| &r.name == name) {
                Some(row) if matches!(row.status, ContainerStatus::Running(_)) => {
                    targets.push((row.name.clone(), row.container.clone()));
                }
                _ => not_running.push(name.clone()),
            }
        }
        for name in not_running {
            self.procs
                .insert(name, ProcState::Message("(not running)".to_string()));
        }
        // The selected running instance is fetched too (for the Detail agent
        // count), even when its process layer isn't expanded. Deduped against the
        // expanded targets so it's never fetched twice in one batch.
        if let Some((name, container)) = self.selected_running_instance() {
            if !targets.iter().any(|(n, _)| n == &name) {
                targets.push((name, container));
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
        assert!(app.take_pending_stop().is_none());
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
                container: "devsandbox-inst0".into(),
                pid: "10".into(),
                signal: Signal::Term,
            }),
        );
        app.on_key(key(KeyCode::Char('K')));
        assert_eq!(
            app.take_pending_signal(),
            Some(PendingSignal {
                container: "devsandbox-inst0".into(),
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
            }],
            services: Vec::new(),
            sandbox_count: 1,
            runtime_name: "docker",
            runtime_version: None,
            collected_at: std::time::Instant::now(),
            error: None,
        };
        app.set_snapshot(snap);
        app.expanded_procs.insert("up".into());
        app.expanded_procs.insert("down".into());

        let targets = app.proc_fetch_targets();
        assert_eq!(targets, vec![("up".to_string(), "devsandbox-up".to_string())]);
        // Non-running instance got a (not running) message row, no fetch.
        assert_eq!(
            app.procs.get("down"),
            Some(&crate::tui::procs::ProcState::Message("(not running)".into())),
        );
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
            }],
            services: Vec::new(),
            sandbox_count: 1,
            runtime_name: "docker",
            runtime_version: None,
            collected_at: std::time::Instant::now(),
            error: None,
        });

        // Cursor on the sandbox node: no instance selected, nothing expanded.
        assert!(app.proc_fetch_targets().is_empty());

        // Move onto the running instance: it becomes a fetch target for the
        // Detail agent count, without being expanded.
        app.on_key(key(KeyCode::Down));
        assert_eq!(
            app.proc_fetch_targets(),
            vec![("up".to_string(), "devsandbox-up".to_string())],
        );
        assert!(app.expanded_procs.is_empty());
    }
}
