//! One-key instance actions (stop/start, done/undone, open in VS Code, stop
//! forward) and the pending-request queue `tui::mod` drains onto background
//! threads.

use crate::tui::data::ContainerStatus;
use crate::tui::prompt::PromptAction;

use super::{App, PortRequest};

/// A done-flag write for the event loop (`commands::done::set_saved`, off the
/// UI thread).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingDone {
    /// State key (a snapshot row name).
    pub instance: String,
    /// Set (`true`) or clear the flag.
    pub done: bool,
    /// Whether the outcome replaces the status line. A thread's child write
    /// leaves the thread's own status (which already names the child) unless
    /// it fails.
    pub report: bool,
}

impl App {
    /// `d` / `u` (Instances tab): mark the instance under the cursor done, or
    /// clear it. A no-op off an instance (process rows are filtered by the
    /// caller); an instance already in the wanted state only gets a status.
    pub(super) fn set_selected_done(&mut self, done: bool) {
        let Some(i) = self.selected_instance_index() else {
            return;
        };
        let Some(row) = self.snapshot.as_ref().and_then(|s| s.instances.get(i)) else {
            return;
        };
        let name = row.name.clone();
        if row.done == done {
            self.status = Some(if done { format!("`{name}` is already done") } else { format!("`{name}` is not done") });
            return;
        }
        self.status = Some(format!("marking {name} {}…", if done { "done" } else { "not done" }));
        self.pending_done.push(PendingDone { instance: name, done, report: true });
    }

    /// Queue a done-flag write for `instance` on behalf of an Inbox thread
    /// (its outcome only shows on failure).
    pub(super) fn request_child_done(&mut self, instance: String, done: bool) {
        self.pending_done.push(PendingDone { instance, done, report: false });
    }

    /// Take the pending done-flag writes for the event loop to spawn.
    pub fn take_pending_done(&mut self) -> Vec<PendingDone> {
        std::mem::take(&mut self.pending_done)
    }

    /// `d` (Ports tab): stop the selected forward, queuing its id for the event
    /// loop (step 10) to remove on its worker thread. No-op with no row selected.
    pub(super) fn stop_selected_forward(&mut self) {
        let Some(row) = self.ports.get(self.selected()) else {
            return;
        };
        self.status = Some(format!("stopping {}", row.local));
        self.pending_unport = Some(row.id);
    }

    /// Take the pending forward request for the event loop to start, if any.
    pub fn take_pending_port(&mut self) -> Option<PortRequest> {
        self.pending_port.take()
    }

    /// Take the pending forward-stop id for the event loop to remove, if any.
    pub fn take_pending_unport(&mut self) -> Option<u64> {
        self.pending_unport.take()
    }

    /// `o` (Instances tab): VS Code attach for the instance under the cursor,
    /// routed through the same [`PromptAction::Code`] path the `code` command
    /// uses. Works for orphan-group instance children and process rows (routing
    /// to the parent instance); a no-op on sandbox / empty / orphan-group nodes.
    pub(super) fn attach_code(&mut self) {
        let Some(i) = self.selected_instance_index() else {
            return;
        };
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let Some(row) = snapshot.instances.get(i) else {
            return;
        };
        self.pending_action = Some(PromptAction::Code { instance: row.name.clone() });
    }

    /// `s` (Instances tab): stop the running instance under the cursor, or
    /// start it when its container is exited, on a background thread (the event
    /// loop owns the docker work, keeping [`App`] I/O-free). Works for
    /// orphan-group instance children and process rows (routing to the parent
    /// instance); a no-op on sandbox / empty / orphan-group nodes and while a
    /// stop/start for the same instance is in flight.
    ///
    /// Two cases can't be served by a bare `docker start` and are *rebuilt*
    /// instead, by queuing a [`PromptAction::Rebuild`] through the same suspend
    /// path the `:` prompt uses (the loop restores the terminal, runs `rebuild`
    /// with inherited stdio, then re-enters): an exited instance whose
    /// container drifted from the current config, and a missing container —
    /// `rebuild` is the worktree-preserving way to recreate it (`run` refuses
    /// the taken name, `rm` destroys the worktree). Rebuild is modal, so it
    /// needs no `starting` in-flight guard — the guard is only for the
    /// background stop/start ops.
    pub(super) fn stop_or_start_instance(&mut self) {
        let Some(i) = self.selected_instance_index() else {
            return;
        };
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let Some(row) = snapshot.instances.get(i) else {
            return;
        };
        let name = row.name.clone();
        if self.stopping.contains(&name) || self.starting.contains(&name) {
            return;
        }
        match row.status {
            ContainerStatus::Running(_) => {
                self.status = Some(format!("stopping {name}…"));
                self.stopping.insert(name.clone());
                self.pending_stop = Some(name);
            }
            ContainerStatus::Exited(_) if row.drift => {
                // Config drifted: recreate the container (worktree preserved) via
                // the modal suspend path instead of a bare start. No `starting`
                // guard — that's for background ops; the suspend is modal.
                self.pending_action =
                    Some(PromptAction::Rebuild { instance: name, force: false });
            }
            ContainerStatus::Exited(_) => {
                self.status = Some(format!("starting {name}…"));
                self.starting.insert(name.clone());
                self.pending_start = Some(name);
            }
            ContainerStatus::Unknown => {
                self.status = Some(format!("{name}: status still loading"));
            }
            ContainerStatus::Missing => {
                // No container to start: recreate it via the CLI rebuild on the
                // suspend path (its "config label gone → rebuild anyway" rule
                // covers exactly this).
                self.pending_action =
                    Some(PromptAction::Rebuild { instance: name, force: false });
            }
        }
    }

    /// Help-bar verb for the `r` key: `rename` when the cursor is on an
    /// instance (or one of its process rows), `run` otherwise — mirrors what
    /// [`Self::open_rename_or_run_prompt`] would do.
    pub fn run_rename_hint(&self) -> &'static str {
        if self.selected_instance_index().is_some() {
            "rename"
        } else {
            "run"
        }
    }

    /// Help-bar verb for the `s` key, matching what
    /// [`Self::stop_or_start_instance`] would actually do to the instance under
    /// the cursor: `start` when it is exited or missing (drifted/missing ones
    /// rebuild, which is still a start from the user's seat), `stop` otherwise.
    pub fn stop_start_hint(&self) -> &'static str {
        let startable = self
            .selected_instance_index()
            .and_then(|i| self.snapshot.as_ref()?.instances.get(i))
            .is_some_and(|row| {
                matches!(
                    row.status,
                    ContainerStatus::Exited(_) | ContainerStatus::Missing
                )
            });
        if startable { "start" } else { "stop" }
    }

    /// Take the pending background stop for the event loop to spawn, if any.
    pub fn take_pending_stop(&mut self) -> Option<String> {
        self.pending_stop.take()
    }

    /// Take the pending background start for the event loop to spawn, if any.
    pub fn take_pending_start(&mut self) -> Option<String> {
        self.pending_start.take()
    }
}

#[cfg(test)]
mod tests {
    use crate::tui::app::test_support::*;
    use crate::tui::app::*;

    #[test]
    fn d_on_ports_sets_pending_unport() {
        let mut app = new_app();
        app.tab = Tab::Ports;
        app.set_ports(port_rows(3));
        app.on_key(key(KeyCode::Down)); // onto id 1
        app.on_key(key(KeyCode::Char('d')));
        assert_eq!(app.take_pending_unport(), Some(1));
        assert_eq!(app.status.as_deref(), Some("stopping 127.0.0.1:3001"));
    }

    #[test]
    fn d_and_u_on_an_instance_queue_the_done_flag() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with(1));
        app.on_key(key(KeyCode::Down)); // onto inst0
        app.on_key(key(KeyCode::Char('d')));
        assert_eq!(
            app.take_pending_done(),
            [PendingDone { instance: "inst0".into(), done: true, report: true }]
        );
        assert_eq!(app.status.as_deref(), Some("marking inst0 done…"));
        // Not done yet: `u` only says so.
        app.on_key(key(KeyCode::Char('u')));
        assert_eq!(app.take_pending_done(), []);
        assert_eq!(app.status.as_deref(), Some("`inst0` is not done"));

        let mut snap = snapshot_with(1);
        snap.instances[0].done = true;
        app.set_snapshot(snap);
        app.on_key(key(KeyCode::Char('u')));
        assert_eq!(
            app.take_pending_done(),
            [PendingDone { instance: "inst0".into(), done: false, report: true }]
        );
        app.on_key(key(KeyCode::Char('d')));
        assert_eq!(app.take_pending_done(), []);
        assert_eq!(app.status.as_deref(), Some("`inst0` is already done"));
    }

    #[test]
    fn d_and_u_off_an_instance_queue_nothing() {
        // A process row.
        let mut app = app_on_proc_row(&["10"]);
        app.on_key(key(KeyCode::Char('d')));
        app.on_key(key(KeyCode::Char('u')));
        assert_eq!(app.take_pending_done(), []);
        // The sandbox node.
        let mut app = new_app();
        app.set_snapshot(snapshot_with(1));
        app.on_key(key(KeyCode::Char('d')));
        assert_eq!(app.take_pending_done(), []);
        // Another tab.
        app.on_key(key(KeyCode::Down));
        app.tab = Tab::Services;
        app.on_key(key(KeyCode::Char('d')));
        assert_eq!(app.take_pending_done(), []);
    }

    #[test]
    fn d_elsewhere_does_nothing_new() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with(1)); // Instances tab
        app.on_key(key(KeyCode::Down)); // onto inst0
        app.on_key(key(KeyCode::Char('d')));
        assert_eq!(app.take_pending_unport(), None);
    }

    #[test]
    fn o_on_instance_sets_pending_code() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with(1)); // [Sandbox(0), inst0]
        app.on_key(key(KeyCode::Down)); // onto inst0
        app.on_key(key(KeyCode::Char('o')));
        assert_eq!(
            app.take_pending_action(),
            Some(PromptAction::Code { instance: "inst0".into() }),
        );
    }

    #[test]
    fn o_on_orphan_instance_sets_pending_code() {
        let mut app = new_app();
        app.set_snapshot(orphan_snapshot()); // [Orphans, orphan0]
        app.on_key(key(KeyCode::Down)); // onto orphan0
        assert_eq!(app.selected_node(), Some(Node::Instance(0)));
        app.on_key(key(KeyCode::Char('o')));
        assert_eq!(
            app.take_pending_action(),
            Some(PromptAction::Code { instance: "orphan0".into() }),
        );
    }

    #[test]
    fn o_on_sandbox_is_noop() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with(1)); // cursor on Sandbox(0)
        app.on_key(key(KeyCode::Char('o')));
        assert_eq!(app.take_pending_action(), None);
    }

    #[test]
    fn s_on_running_instance_sets_pending_stop() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with_status(1, running())); // [Sandbox(0), inst0]
        app.on_key(key(KeyCode::Down)); // onto inst0
        app.on_key(key(KeyCode::Char('s')));
        assert!(app.stopping.contains("inst0"));
        assert_eq!(app.status.as_deref(), Some("stopping inst0…"));
        assert_eq!(app.take_pending_stop(), Some("inst0".into()));
        assert_eq!(app.take_pending_start(), None);
    }

    #[test]
    fn s_on_exited_instance_sets_pending_start() {
        let mut app = new_app();
        let status = ContainerStatus::Exited("Exited (0)".into());
        app.set_snapshot(snapshot_with_status(1, status));
        app.on_key(key(KeyCode::Down)); // onto inst0
        app.on_key(key(KeyCode::Char('s')));
        assert!(app.starting.contains("inst0"));
        assert_eq!(app.status.as_deref(), Some("starting inst0…"));
        assert_eq!(app.take_pending_start(), Some("inst0".into()));
        assert_eq!(app.take_pending_stop(), None);
    }

    /// Before the runtime is listed, `s` can't know whether to stop or start.
    #[test]
    fn s_on_unknown_instance_does_nothing() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with_status(1, ContainerStatus::Unknown));
        app.on_key(key(KeyCode::Down)); // onto inst0
        app.on_key(key(KeyCode::Char('s')));
        assert_eq!(app.status.as_deref(), Some("inst0: status still loading"));
        assert_eq!(app.take_pending_stop(), None);
        assert_eq!(app.take_pending_start(), None);
        assert_eq!(app.take_pending_action(), None);
    }

    #[test]
    fn s_on_exited_drifted_instance_queues_rebuild() {
        let mut app = new_app();
        let status = ContainerStatus::Exited("Exited (0)".into());
        let mut snap = snapshot_with_status(1, status);
        snap.instances[0].drift = true;
        app.set_snapshot(snap);
        app.on_key(key(KeyCode::Down)); // onto inst0
        app.on_key(key(KeyCode::Char('s')));
        assert_eq!(
            app.take_pending_action(),
            Some(PromptAction::Rebuild { instance: "inst0".into(), force: false })
        );
        // Drift takes the suspend path, not the background start guard.
        assert_eq!(app.take_pending_start(), None);
        assert!(app.starting.is_empty());
        assert_eq!(app.take_pending_stop(), None);
    }

    #[test]
    fn s_on_exited_undrifted_instance_still_bare_starts() {
        let mut app = new_app();
        let status = ContainerStatus::Exited("Exited (0)".into());
        let snap = snapshot_with_status(1, status); // drift: false
        app.set_snapshot(snap);
        app.on_key(key(KeyCode::Down)); // onto inst0
        app.on_key(key(KeyCode::Char('s')));
        assert!(app.starting.contains("inst0"));
        assert_eq!(app.take_pending_start(), Some("inst0".into()));
        assert_eq!(app.take_pending_action(), None);
    }

    #[test]
    fn s_on_running_drifted_instance_still_stops() {
        let mut app = new_app();
        let mut snap = snapshot_with_status(1, running());
        snap.instances[0].drift = true;
        app.set_snapshot(snap);
        app.on_key(key(KeyCode::Down)); // onto inst0
        app.on_key(key(KeyCode::Char('s')));
        assert!(app.stopping.contains("inst0"));
        assert_eq!(app.take_pending_stop(), Some("inst0".into()));
        assert_eq!(app.take_pending_action(), None);
        assert_eq!(app.take_pending_start(), None);
    }

    #[test]
    fn s_on_missing_container_queues_rebuild() {
        // A missing container can't be bare-started; `s` recreates it via the
        // modal CLI rebuild (worktree preserved), like the drifted-exited case.
        let mut app = new_app();
        app.set_snapshot(snapshot_with(1)); // status Missing
        app.on_key(key(KeyCode::Down)); // onto inst0
        app.on_key(key(KeyCode::Char('s')));
        assert_eq!(
            app.take_pending_action(),
            Some(PromptAction::Rebuild { instance: "inst0".into(), force: false })
        );
        assert_eq!(app.take_pending_stop(), None);
        assert_eq!(app.take_pending_start(), None);
        assert!(app.stopping.is_empty());
        assert!(app.starting.is_empty());
    }

    #[test]
    fn s_on_orphan_instance_sets_pending_stop() {
        let mut app = new_app();
        let mut snap = orphan_snapshot(); // [Orphans, orphan0]
        snap.instances[0].status = running();
        app.set_snapshot(snap);
        app.on_key(key(KeyCode::Down)); // onto orphan0
        assert_eq!(app.selected_node(), Some(Node::Instance(0)));
        app.on_key(key(KeyCode::Char('s')));
        assert_eq!(app.take_pending_stop(), Some("orphan0".into()));
    }

    #[test]
    fn s_on_sandbox_is_noop() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with(1)); // cursor on Sandbox(0)
        app.on_key(key(KeyCode::Char('s')));
        assert_eq!(app.take_pending_stop(), None);
        assert!(app.stopping.is_empty());
    }

    #[test]
    fn s_dedupes_while_stop_in_flight() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with_status(1, running()));
        app.on_key(key(KeyCode::Down)); // onto inst0
        app.on_key(key(KeyCode::Char('s')));
        assert_eq!(app.take_pending_stop(), Some("inst0".into()));
        // Still in flight (loop hasn't cleared `stopping`): a second `s` is a no-op.
        app.on_key(key(KeyCode::Down)); // keep cursor on inst0
        app.on_key(key(KeyCode::Char('s')));
        assert_eq!(app.take_pending_stop(), None);
    }

    #[test]
    fn s_dedupes_while_start_in_flight() {
        let mut app = new_app();
        let status = ContainerStatus::Exited("Exited (0)".into());
        app.set_snapshot(snapshot_with_status(1, status));
        app.on_key(key(KeyCode::Down)); // onto inst0
        app.on_key(key(KeyCode::Char('s')));
        assert_eq!(app.take_pending_start(), Some("inst0".into()));
        // Still in flight (loop hasn't cleared `starting`): a second `s` is a no-op.
        app.on_key(key(KeyCode::Down)); // keep cursor on inst0
        app.on_key(key(KeyCode::Char('s')));
        assert_eq!(app.take_pending_start(), None);
    }

    #[test]
    fn s_ignored_on_services_tab() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with(1));
        app.tab = Tab::Services;
        app.on_key(key(KeyCode::Char('s')));
        assert_eq!(app.take_pending_stop(), None);
    }

    #[test]
    fn stop_start_hint_tracks_selection_status() {
        let mut app = new_app();
        // No snapshot / nothing selected → the default verb.
        assert_eq!(app.stop_start_hint(), "stop");
        // Exited instance under the cursor → `start`.
        app.set_snapshot(snapshot_with_status(
            1,
            ContainerStatus::Exited("Exited (0)".into()),
        ));
        app.on_key(key(KeyCode::Down)); // onto inst0
        assert_eq!(app.stop_start_hint(), "start");
        // Running → `stop`; Missing (s rebuilds — a start from the user's
        // seat) → `start`.
        app.set_snapshot(snapshot_with_status(1, running()));
        assert_eq!(app.stop_start_hint(), "stop");
        app.set_snapshot(snapshot_with_status(1, ContainerStatus::Missing));
        assert_eq!(app.stop_start_hint(), "start");
    }

    #[test]
    fn run_rename_hint_tracks_selection() {
        let mut app = new_app();
        // No snapshot / sandbox row selected → `run`.
        assert_eq!(app.run_rename_hint(), "run");
        app.set_snapshot(snapshot_with(2)); // [Sandbox(0), inst0, inst1]
        assert_eq!(app.run_rename_hint(), "run");
        // Instance row under the cursor → `rename`.
        app.on_key(key(KeyCode::Down)); // onto inst0
        assert_eq!(app.run_rename_hint(), "rename");
    }
}
