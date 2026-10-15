//! One-key instance actions (stop/start, remove, open in VS Code, stop
//! forward) and the pending-request queue `tui::mod` drains onto background
//! threads.

use crate::snapshot::{InstanceRow, PhaseRow};
use crate::tui::data::ContainerStatus;
use crate::tui::prompt::PromptAction;

use super::{App, PortRequest};

/// Why `row` is off-limits to the one-key actions: a live `run`/`rebuild`
/// is materializing it, and anything else would race it (the CLI guards
/// refuse too, but the dashboard shouldn't offer it). `None` = go.
fn busy(row: &InstanceRow) -> Option<String> {
    match &row.phase {
        Some(PhaseRow::InFlight { step }) => {
            Some(format!("`{}` is still {}", row.name, crate::commands::doing(step)))
        }
        _ => None,
    }
}

/// How a verb request the event loop sent for a [`PromptAction`] ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VerbEnd {
    /// Its status line, and whether it failed.
    Done { status: String, failed: bool },
    /// rm's `confirm`: its branch question, nothing removed yet.
    Confirm(String),
}

/// A done-flag write for the event loop to send (`instances.done`).
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
    /// `d` (Instances tab): remove the instance under the cursor right away,
    /// no prompt, as the same `rm` request `:rm` sends (non-forced, so `rm`'s
    /// own dirty-worktree refusal and branch-delete question, a Confirm modal
    /// via [`Self::verb_answered`], still guard it).
    /// A no-op off an instance (process rows are filtered by the caller). An
    /// in-flight one can't be removed: `d` asks to cancel its build instead
    /// (a cancelled `run` rolls back, so that's its removal); a failed one is
    /// removable.
    pub(super) fn remove_instance(&mut self) {
        let Some(i) = self.selected_instance_index() else {
            return;
        };
        let Some(row) = self.snapshot.as_ref().and_then(|s| s.instances.get(i)) else {
            return;
        };
        if busy(row).is_some() {
            let name = row.name.clone();
            self.confirm_cancel_build(name);
            return;
        }
        self.pending_action = Some(PromptAction::Rm { instance: row.name.clone(), force: false, delete_branch: None });
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
        self.pending_action = Some(PromptAction::Code { instance: row.name.clone(), goto: None });
    }

    /// `s` (Instances tab): stop the running instance under the cursor, or
    /// start it when its container is exited, as a `stop` / detached `start`
    /// verb request the event loop sends (keeping [`App`] I/O-free). Works for
    /// orphan-group instance children and process rows (routing to the parent
    /// instance); a no-op on sandbox / empty / orphan-group nodes and while a
    /// stop/start for the same instance is in flight.
    ///
    /// Two cases can't be served by a `start` and are *rebuilt* instead, by queuing a [`PromptAction::Rebuild`] like the `:` prompt's
    /// (the loop runs it as a detached `devsandbox rebuild`): an exited
    /// instance whose container drifted from the current config, and a
    /// missing container — `rebuild` is the worktree-preserving way to
    /// recreate it (`run` refuses the taken name, `rm` destroys the
    /// worktree). It needs no `starting` guard: once the rebuild records its
    /// phase the row is in flight, which `busy` refuses, and `rebuild`
    /// itself refuses an in-flight instance.
    ///
    /// The phase comes first: an in-flight instance gets nothing (a status
    /// line says what it's doing), and a failed one is rebuilt, the retry
    /// (`rebuild` re-materializes it whatever its container's state).
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
        if let Some(why) = busy(row) {
            self.status = Some(why);
            return;
        }
        if let Some(PhaseRow::Failed { .. }) = row.phase {
            self.pending_action = Some(PromptAction::Rebuild { instance: name, force: false });
            return;
        }
        match row.status {
            ContainerStatus::Running(_) => {
                self.status = Some(format!("stopping {name}…"));
                self.stopping.insert(name.clone());
                self.pending_action = Some(PromptAction::Stop { instance: name });
            }
            ContainerStatus::Exited(_) if row.drift => {
                // Config drifted: recreate the container (worktree preserved)
                // with a detached rebuild instead of a start.
                self.pending_action =
                    Some(PromptAction::Rebuild { instance: name, force: false });
            }
            ContainerStatus::Exited(_) => {
                self.status = Some(format!("starting {name}…"));
                self.starting.insert(name.clone());
                self.pending_action = Some(PromptAction::Start { instance: name });
            }
            ContainerStatus::Unknown => {
                self.status = Some(format!("{name}: status still loading"));
            }
            ContainerStatus::Missing => {
                // No container to start: recreate it via the CLI rebuild (its
                // "config label gone → rebuild anyway" rule covers exactly
                // this).
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
    /// the cursor: `wait` while it's in flight, `rebuild` once failed, `start`
    /// when it is exited or missing (drifted/missing ones rebuild, which is
    /// still a start from the user's seat), `stop` otherwise.
    pub fn stop_start_hint(&self) -> &'static str {
        let row = self.selected_instance_index().and_then(|i| self.snapshot.as_ref()?.instances.get(i));
        match row {
            Some(InstanceRow { phase: Some(PhaseRow::InFlight { .. }), .. }) => "wait",
            Some(InstanceRow { phase: Some(PhaseRow::Failed { .. }), .. }) => "rebuild",
            Some(row) if matches!(row.status, ContainerStatus::Exited(_) | ContainerStatus::Missing) => "start",
            _ => "stop",
        }
    }

    /// Help-bar verb for the `d` key, matching [`Self::remove_instance`]:
    /// `cancel` on an in-flight instance, else `rm`.
    pub fn remove_hint(&self) -> &'static str {
        let row = self.selected_instance_index().and_then(|i| self.snapshot.as_ref()?.instances.get(i));
        match row {
            Some(row) if busy(row).is_some() => "cancel",
            _ => "rm",
        }
    }

    /// The event loop's report that the verb request for `action` ended:
    /// show its status, clear `s`'s in-flight guard (a detached start
    /// answers once spawned, F14: a thread action waiting on it runs when a
    /// snapshot shows the instance up, [`Self::set_snapshot`]), and turn rm's
    /// branch question into a Confirm modal whose answer resends it.
    pub fn verb_answered(&mut self, action: &PromptAction, end: VerbEnd) {
        match (action, end) {
            (PromptAction::Rm { instance, force, .. }, VerbEnd::Confirm(question)) => {
                self.status = None;
                self.confirm_rm_branch(instance.clone(), *force, question);
            }
            // Only rm asks.
            (_, VerbEnd::Confirm(question)) => self.status = Some(question),
            (action, VerbEnd::Done { status, failed }) => {
                self.status = Some(status);
                match action {
                    PromptAction::Stop { instance } => {
                        self.stopping.remove(instance);
                    }
                    PromptAction::Start { instance } => self.start_answered(instance, failed),
                    _ => {}
                }
            }
        }
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
    fn d_on_an_instance_queues_rm_without_a_prompt() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with(1));
        app.on_key(key(KeyCode::Down)); // onto inst0
        app.on_key(key(KeyCode::Char('d')));
        assert_eq!(
            app.take_pending_action(),
            Some(PromptAction::Rm { instance: "inst0".into(), force: false, delete_branch: None })
        );
        assert!(app.prompt.is_none());
        assert_eq!(app.take_pending_done(), []);
    }

    #[test]
    fn d_and_u_off_an_instance_queue_nothing() {
        // A process row.
        let mut app = app_on_proc_row(&["10"]);
        app.on_key(key(KeyCode::Char('d')));
        app.on_key(key(KeyCode::Char('u')));
        assert_eq!(app.take_pending_action(), None);
        // The sandbox node.
        let mut app = new_app();
        app.set_snapshot(snapshot_with(1));
        app.on_key(key(KeyCode::Char('d')));
        assert_eq!(app.take_pending_action(), None);
        // Another tab.
        app.on_key(key(KeyCode::Down));
        app.tab = Tab::Services;
        app.on_key(key(KeyCode::Char('d')));
        assert_eq!(app.take_pending_action(), None);
    }

    #[test]
    fn u_on_an_instance_does_nothing() {
        let mut app = new_app();
        let mut snap = snapshot_with(1);
        snap.instances[0].done = true;
        app.set_snapshot(snap);
        app.on_key(key(KeyCode::Down)); // onto inst0
        app.on_key(key(KeyCode::Char('u')));
        assert_eq!(app.take_pending_done(), []);
        assert_eq!(app.take_pending_action(), None);
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
            Some(PromptAction::Code { instance: "inst0".into(), goto: None }),
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
            Some(PromptAction::Code { instance: "orphan0".into(), goto: None }),
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
        assert_eq!(app.take_pending_action(), Some(PromptAction::Stop { instance: "inst0".into() }));
        assert_eq!(app.take_pending_action(), None);
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
        assert_eq!(app.take_pending_action(), Some(PromptAction::Start { instance: "inst0".into() }));
        assert_eq!(app.take_pending_action(), None);
    }

    /// Before the runtime is listed, `s` can't know whether to stop or start.
    #[test]
    fn s_on_unknown_instance_does_nothing() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with_status(1, ContainerStatus::Unknown));
        app.on_key(key(KeyCode::Down)); // onto inst0
        app.on_key(key(KeyCode::Char('s')));
        assert_eq!(app.status.as_deref(), Some("inst0: status still loading"));
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
        // Drift rebuilds: no start guard.
        assert_eq!(app.take_pending_action(), None);
        assert!(app.starting.is_empty());
        assert_eq!(app.take_pending_action(), None);
    }

    #[test]
    fn s_on_exited_undrifted_instance_still_starts() {
        let mut app = new_app();
        let status = ContainerStatus::Exited("Exited (0)".into());
        let snap = snapshot_with_status(1, status); // drift: false
        app.set_snapshot(snap);
        app.on_key(key(KeyCode::Down)); // onto inst0
        app.on_key(key(KeyCode::Char('s')));
        assert!(app.starting.contains("inst0"));
        assert_eq!(app.take_pending_action(), Some(PromptAction::Start { instance: "inst0".into() }));
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
        assert_eq!(app.take_pending_action(), Some(PromptAction::Stop { instance: "inst0".into() }));
        assert_eq!(app.take_pending_action(), None);
    }

    #[test]
    fn s_on_missing_container_queues_rebuild() {
        // A missing container can't be started; `s` recreates it via a
        // detached rebuild (worktree preserved), like the drifted-exited case.
        let mut app = new_app();
        app.set_snapshot(snapshot_with(1)); // status Missing
        app.on_key(key(KeyCode::Down)); // onto inst0
        app.on_key(key(KeyCode::Char('s')));
        assert_eq!(
            app.take_pending_action(),
            Some(PromptAction::Rebuild { instance: "inst0".into(), force: false })
        );
        assert_eq!(app.take_pending_action(), None);
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
        assert_eq!(app.take_pending_action(), Some(PromptAction::Stop { instance: "orphan0".into() }));
    }

    #[test]
    fn s_on_sandbox_is_noop() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with(1)); // cursor on Sandbox(0)
        app.on_key(key(KeyCode::Char('s')));
        assert_eq!(app.take_pending_action(), None);
        assert!(app.stopping.is_empty());
    }

    #[test]
    fn s_dedupes_while_stop_in_flight() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with_status(1, running()));
        app.on_key(key(KeyCode::Down)); // onto inst0
        app.on_key(key(KeyCode::Char('s')));
        assert_eq!(app.take_pending_action(), Some(PromptAction::Stop { instance: "inst0".into() }));
        // Still in flight (loop hasn't cleared `stopping`): a second `s` is a no-op.
        app.on_key(key(KeyCode::Down)); // keep cursor on inst0
        app.on_key(key(KeyCode::Char('s')));
        assert_eq!(app.take_pending_action(), None);
    }

    #[test]
    fn s_dedupes_while_start_in_flight() {
        let mut app = new_app();
        let status = ContainerStatus::Exited("Exited (0)".into());
        app.set_snapshot(snapshot_with_status(1, status));
        app.on_key(key(KeyCode::Down)); // onto inst0
        app.on_key(key(KeyCode::Char('s')));
        assert_eq!(app.take_pending_action(), Some(PromptAction::Start { instance: "inst0".into() }));
        // Still in flight (loop hasn't cleared `starting`): a second `s` is a no-op.
        app.on_key(key(KeyCode::Down)); // keep cursor on inst0
        app.on_key(key(KeyCode::Char('s')));
        assert_eq!(app.take_pending_action(), None);
    }

    /// The answer to `s`'s request clears its guard (failed or not), so `s`
    /// works again; the status line is the answer's.
    #[test]
    fn s_guards_clear_when_the_answer_arrives() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with_status(1, running()));
        app.on_key(key(KeyCode::Down)); // onto inst0
        app.on_key(key(KeyCode::Char('s')));
        let stop = app.take_pending_action().unwrap();
        app.verb_answered(&stop, VerbEnd::Done { status: "stopped inst0".into(), failed: false });
        assert!(app.stopping.is_empty());
        assert_eq!(app.status.as_deref(), Some("stopped inst0"));

        app.set_snapshot(snapshot_with_status(1, ContainerStatus::Exited("Exited (0)".into())));
        app.on_key(key(KeyCode::Char('s')));
        let start = app.take_pending_action().unwrap();
        assert_eq!(start, PromptAction::Start { instance: "inst0".into() });
        app.verb_answered(&start, VerbEnd::Done { status: "start inst0 failed: boom".into(), failed: true });
        assert!(app.starting.is_empty());
        app.on_key(key(KeyCode::Char('s')));
        assert_eq!(app.take_pending_action(), Some(start));
    }

    /// rm's branch question opens the Confirm modal as rm words it: `y`/
    /// `enter` resend with `deleteBranch: true`, `n` with `false` (same
    /// `force`), `esc`/`q` send nothing.
    #[test]
    fn rms_question_is_a_three_way_confirm() {
        let q = "delete branch `b`? it has commits not on its remote";
        let ask = |app: &mut App, force| {
            let rm = PromptAction::Rm { instance: "inst0".into(), force, delete_branch: None };
            app.verb_answered(&rm, VerbEnd::Confirm(q.into()));
            match &app.modal {
                Modal::Confirm(c) => {
                    assert_eq!(c.question, q);
                    assert_eq!(c.keys_hint(), "y/enter yes · n no · esc cancel");
                }
                _ => panic!("no confirm"),
            }
        };
        let resent = |force, delete| Some(PromptAction::Rm { instance: "inst0".into(), force, delete_branch: Some(delete) });
        let mut app = new_app();
        for (code, force, want) in [
            (KeyCode::Char('y'), false, resent(false, true)),
            (KeyCode::Enter, true, resent(true, true)),
            (KeyCode::Char('n'), true, resent(true, false)),
            (KeyCode::Esc, false, None),
            (KeyCode::Char('q'), false, None),
        ] {
            ask(&mut app, force);
            // Other keys neither answer nor reach the dashboard.
            app.on_key(key(KeyCode::Char('s')));
            assert!(matches!(app.modal, Modal::Confirm(_)));
            app.on_key(key(code));
            assert!(matches!(app.modal, Modal::None), "{code:?}");
            assert_eq!(app.take_pending_action(), want, "{code:?}");
            assert_eq!(app.take_pending_cancel(), None);
        }
        assert_eq!(app.status.as_deref(), Some("rm inst0: cancelled"));
    }

    #[test]
    fn s_ignored_on_services_tab() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with(1));
        app.tab = Tab::Services;
        app.on_key(key(KeyCode::Char('s')));
        assert_eq!(app.take_pending_action(), None);
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

    /// An in-flight row offers nothing that would race the build: `s` only
    /// says what it's doing, whatever its container does, and `d` asks to
    /// cancel it rather than remove it.
    #[test]
    fn s_and_d_on_an_in_flight_instance_queue_nothing() {
        for status in [ContainerStatus::Missing, running(), ContainerStatus::Exited("Exited (1)".into())] {
            let mut app = new_app();
            app.set_snapshot(snapshot_with_phase(1, status, in_flight("postCreateCommand")));
            app.on_key(key(KeyCode::Down)); // onto inst0
            assert_eq!(app.stop_start_hint(), "wait");
            app.on_key(key(KeyCode::Char('s')));
            assert_eq!(app.status.as_deref(), Some("`inst0` is still running postCreateCommand"));
            app.on_key(key(KeyCode::Char('d')));
            assert!(matches!(app.modal, Modal::Confirm(_)));
            app.on_key(key(KeyCode::Esc));
            assert_eq!(app.take_pending_action(), None);
            assert_eq!(app.take_pending_cancel(), None);
        }
    }

    /// `d` on an in-flight row: confirm, then the loop SIGINTs its build;
    /// any answer but yes queues nothing.
    #[test]
    fn d_on_an_in_flight_instance_confirms_a_cancel() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with_phase(1, ContainerStatus::Missing, in_flight("building")));
        app.on_key(key(KeyCode::Down)); // onto inst0
        app.on_key(key(KeyCode::Char('d')));
        match &app.modal {
            Modal::Confirm(c) => assert_eq!(c.question, "cancel inst0's build?"),
            _ => panic!("no confirm"),
        }
        // Other keys neither answer nor reach the dashboard.
        app.on_key(key(KeyCode::Char('s')));
        assert!(matches!(app.modal, Modal::Confirm(_)));
        app.on_key(key(KeyCode::Char('n')));
        assert!(matches!(app.modal, Modal::None));
        assert_eq!(app.take_pending_cancel(), None);
        app.on_key(key(KeyCode::Char('d')));
        app.on_key(key(KeyCode::Char('y')));
        assert!(matches!(app.modal, Modal::None));
        assert_eq!(app.take_pending_cancel(), Some("inst0".into()));
        assert_eq!(app.status.as_deref(), Some("cancelling inst0…"));
        assert_eq!(app.take_pending_action(), None);
        // `enter` is yes too.
        app.on_key(key(KeyCode::Char('d')));
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.take_pending_cancel(), Some("inst0".into()));
    }

    /// A failed row retries with `s` (a rebuild, even over a running or
    /// exited container) and removes with `d`.
    #[test]
    fn s_rebuilds_and_d_removes_a_failed_instance() {
        for status in [ContainerStatus::Missing, running(), ContainerStatus::Exited("Exited (1)".into())] {
            let mut app = new_app();
            app.set_snapshot(snapshot_with_phase(1, status, failed("postCreateCommand")));
            app.on_key(key(KeyCode::Down)); // onto inst0
            assert_eq!(app.stop_start_hint(), "rebuild");
            app.on_key(key(KeyCode::Char('s')));
            assert_eq!(
                app.take_pending_action(),
                Some(PromptAction::Rebuild { instance: "inst0".into(), force: false })
            );
            assert_eq!(app.take_pending_action(), None);
            app.on_key(key(KeyCode::Char('d')));
            assert_eq!(app.take_pending_action(), Some(PromptAction::Rm { instance: "inst0".into(), force: false, delete_branch: None }));
        }
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
