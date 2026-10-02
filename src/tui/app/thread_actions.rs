//! Actions on an Inbox thread (docs/inbox-threads.md, *Actions*): the `1`-`9`
//! buttons and the fixed `o`/`t`/`l`/`p` keys (Inbox list or thread). A host
//! verb maps onto the plumbing the dashboard already has for a selected instance
//! (`PromptAction::Code`, the terminal panel, the logs modal, `pending_port`,
//! `pending_open`, `PromptAction::Rm`), aimed at the thread's target instead
//! of a table row. A button's dispatcher half (no `host`, `notify`, `done`)
//! is an [`Op::Act`] for the store, which enqueues the owner's event.
//!
//! The target is the one thing a thread body can't choose freely: its
//! `child` (one of the owner's dispatcher children), else the owner itself.
//! Nothing a container writes can aim a host action at another instance.

use crate::commands::vscode::Goto;
use crate::inbox::thread::HostVerb;
use crate::inbox::{Op, Thread};
use crate::tui::prompt::PromptAction;

use super::{App, PortRequest};

impl App {
    /// The instance `t`'s host actions act on, or a status line saying why
    /// there is none.
    ///
    /// With a `child`, only that child: resolved through the owner's own
    /// entry in `thread_children`, so a key another dispatcher also uses
    /// can't reach that dispatcher's child, and a child that's gone is
    /// refused rather than falling back to the owner (the action was meant
    /// for the child's work, not the dispatcher's). Without one, the owner by
    /// `instance_id` in the snapshot, so a renamed owner still resolves. An
    /// archived thread's owner was removed: its buttons are history.
    pub(super) fn thread_target(&self, t: &Thread) -> Result<String, String> {
        if t.archived {
            return Err(format!("archived: `{}` was removed", t.owner_name));
        }
        if let Some(key) = &t.child {
            return self
                .thread_children
                .get(&t.owner)
                .and_then(|keys| keys.get(key))
                .cloned()
                .ok_or_else(|| format!("child `{key}` not found (removed, renamed or ambiguous)"));
        }
        self.snapshot
            .as_ref()
            .and_then(|s| s.instances.iter().find(|r| !r.instance_id.is_empty() && r.instance_id == t.owner))
            .map(|r| r.name.clone())
            .ok_or_else(|| format!("`{}` not found", t.owner_name))
    }

    /// `1`-`9`: run action `n` (0-based) of `t`. A host verb runs now; the
    /// dispatcher half of an action (`notify`, `done`, or no `host` at all)
    /// is queued as an event for the owner, and the status line says so. A
    /// host verb that can't run (no target) sends no event either: the click
    /// didn't do what the button says.
    pub(super) fn run_thread_action(&mut self, t: &Thread, n: usize) {
        let Some(action) = t.actions.get(n) else {
            self.status = Some(format!("no action {}", n + 1));
            return;
        };
        if t.archived {
            self.status = Some(format!("archived: `{}` was removed", t.owner_name));
            return;
        }
        let queued = action.enqueues_event().then(|| {
            let what = if action.done { "done" } else { "event" };
            format!("{what} queued for {}", t.owner_name)
        });
        let act = Op::Act { thread: t.id, action: action.id.clone() };
        let Some(host) = &action.host else {
            let child = self.send_act(t, act, action.done);
            let parts: Vec<String> =
                [Some(format!("[{}] {}", n + 1, action.label)), child, queued].into_iter().flatten().collect();
            self.status = Some(parts.join(" · "));
            return;
        };
        let later = queued.as_deref();
        // `open` needs no instance, but an archived thread is read-only all
        // the same.
        let target = match host {
            HostVerb::Open(_) if !t.archived => String::new(),
            _ => match self.thread_target(t) {
                Ok(name) => name,
                Err(why) => {
                    self.status = Some(why);
                    return;
                }
            },
        };
        match host {
            HostVerb::Vscode(v) => {
                // Checked at put time, but a column without a line or a 0 still
                // gets here: open the window anyway and say why the spot wasn't.
                let (goto, dropped) = match v.path.as_deref().map(|p| Goto::new(p, v.line, v.col)) {
                    Some(Ok(goto)) => (Some(goto), None),
                    Some(Err(why)) => (None, Some(format!("--goto dropped ({why})"))),
                    None => (None, None),
                };
                let child = later.is_some().then(|| self.send_act(t, act, action.done)).flatten();
                let note: Vec<&str> =
                    dropped.as_deref().into_iter().chain(child.as_deref()).chain(later).collect();
                self.code_note = (!note.is_empty()).then(|| note.join(" · "));
                self.pending_action = Some(PromptAction::Code { instance: target, goto });
                return;
            }
            HostVerb::Terminal(_) => self.open_instance_terminal(&target),
            HostVerb::Logs(_) => self.logs_on(&target),
            HostVerb::Forward(f) => self.request_port(PortRequest {
                instance: target,
                service: None,
                address: None,
                spec: f.port.to_string(),
            }),
            HostVerb::Open(o) => self.request_open_link(o.url.clone()),
            // The `:rm` path: the CLI's own confirm, on the suspended screen.
            HostVerb::Rm(_) => {
                self.pending_action = Some(PromptAction::Rm { instance: target, force: false })
            }
        }
        if let Some(later) = later {
            let child = self.send_act(t, act, action.done);
            let parts: Vec<String> =
                [self.status.take(), child, Some(later.to_string())].into_iter().flatten().collect();
            self.status = Some(parts.join(" · "));
        }
    }

    /// Queue `act`'s event; a `done: true` action also marks the thread's
    /// child done (docs/inbox-threads.md, *Actions*). The child's status note,
    /// if any.
    fn send_act(&mut self, t: &Thread, act: Op, done: bool) -> Option<String> {
        self.request_inbox(act);
        if done { self.mark_thread_child(t, true) } else { None }
    }

    /// Set or clear the done flag of `t`'s `child` (*Decisions*: the thread's
    /// done/reopen carries to its child), resolved like [`Self::thread_target`]
    /// but never falling back to the owner: a dispatcher isn't done because
    /// one of its threads is. The status note: what was queued, or why the
    /// child was left alone (the thread op still happens); `None` without a
    /// `child`.
    pub(super) fn mark_thread_child(&mut self, t: &Thread, done: bool) -> Option<String> {
        let key = t.child.as_ref()?;
        let what = if done { "done" } else { "not done" };
        match self.thread_children.get(&t.owner).and_then(|keys| keys.get(key)).cloned() {
            Some(name) => {
                let note = format!("child {name} {what}");
                self.request_child_done(name, done);
                Some(note)
            }
            None => Some(format!("child `{key}` not found: not marked {what}")),
        }
    }

    /// `o`: VS Code on the thread's target.
    pub(super) fn thread_code(&mut self, t: &Thread) {
        match self.thread_target(t) {
            Ok(name) => self.pending_action = Some(PromptAction::Code { instance: name, goto: None }),
            Err(why) => self.status = Some(why),
        }
    }

    /// `t`: a terminal on the thread's target. It takes focus like `t`
    /// does elsewhere; the Inbox keeps its focus underneath, so leaving the
    /// terminal comes back to the thread.
    pub(super) fn thread_terminal(&mut self, t: &Thread) {
        match self.thread_target(t) {
            Ok(name) => self.open_instance_terminal(&name),
            Err(why) => self.status = Some(why),
        }
    }

    /// `l`: the logs modal for the thread's target; closing it comes
    /// back to the thread.
    pub(super) fn thread_logs(&mut self, t: &Thread) {
        match self.thread_target(t) {
            Ok(name) => self.logs_on(&name),
            Err(why) => self.status = Some(why),
        }
    }

    /// `p`: the `port` prompt prefilled for the thread's target.
    pub(super) fn thread_port_prompt(&mut self, t: &Thread) {
        match self.thread_target(t) {
            Ok(name) => self.open_port_prompt_for(&name),
            Err(why) => self.status = Some(why),
        }
    }

    /// Logs for instance `name`, through its snapshot row's container.
    fn logs_on(&mut self, name: &str) {
        let container = self
            .snapshot
            .as_ref()
            .and_then(|s| s.instances.iter().find(|r| r.name == name))
            .map(|r| r.container.clone());
        match container {
            Some(container) => self.open_logs_for(&container),
            None => self.status = Some(format!("logs: `{name}` not in the snapshot")),
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode;

    use super::super::test_support::*;
    use super::super::{Focus, InboxFocus, Modal, Tab};
    use super::*;
    use crate::inbox::thread::{Forward, NoArgs, Open, Vscode};
    use crate::inbox::{Action, Inbox, State, ThreadPut};

    /// `snapshot_with_status(2, running())` with owner ids: `inst0` is the
    /// dispatcher `d-id`, `inst1` its child.
    fn app_with_instances() -> App {
        let mut app = new_app();
        let mut snap = snapshot_with_status(2, running());
        snap.instances[0].instance_id = "d-id".into();
        snap.instances[1].instance_id = "c-id".into();
        app.set_snapshot(snap);
        app
    }

    fn act(label: &str, host: Option<HostVerb>) -> Action {
        Action { id: label.to_lowercase(), label: label.into(), host, ..Action::default() }
    }

    /// A thread from `d-id` (named `inst0`) with `child` and `actions`,
    /// installed, selected and focused.
    fn open_thread(app: &mut App, child: Option<&str>, actions: Vec<Action>) {
        let mut inbox = Inbox::default();
        let put = ThreadPut {
            key: "k".into(),
            title: "asks".into(),
            state: State::NeedsYou,
            child: child.map(str::to_string),
            actions,
            ..ThreadPut::default()
        };
        inbox.put("d-id", "inst0", 10, put);
        app.set_inbox(inbox);
        app.on_key(key(KeyCode::Char('4')));
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.inbox.focus, InboxFocus::Thread);
        app.take_pending_inbox();
    }

    fn children(app: &mut App, owner: &str, key: &str, name: &str) {
        app.thread_children.entry(owner.into()).or_default().insert(key.into(), name.into());
    }

    fn target(app: &App) -> Result<String, String> {
        app.thread_target(app.selected_inbox_thread().unwrap())
    }

    #[test]
    fn target_is_the_child_else_the_owner() {
        let mut app = app_with_instances();
        open_thread(&mut app, None, Vec::new());
        assert_eq!(target(&app), Ok("inst0".into()), "no child: the owner, by id");

        let mut app = app_with_instances();
        children(&mut app, "d-id", "pr-1", "inst1");
        open_thread(&mut app, Some("pr-1"), Vec::new());
        assert_eq!(target(&app), Ok("inst1".into()));
    }

    #[test]
    fn missing_or_foreign_child_is_refused_without_owner_fallback() {
        let mut app = app_with_instances();
        open_thread(&mut app, Some("pr-1"), Vec::new());
        let why = target(&app).unwrap_err();
        assert!(why.contains("child `pr-1` not found"), "{why}");

        // Another dispatcher's child under the same key is not this thread's.
        children(&mut app, "other-id", "pr-1", "inst1");
        assert!(target(&app).is_err());

        // A refused target runs nothing, and says why.
        app.on_key(key(KeyCode::Char('o')));
        assert_eq!(app.pending_action, None);
        assert!(app.status.as_deref().unwrap().contains("not found"));
    }

    #[test]
    fn archived_and_unknown_owner_are_refused() {
        let mut app = app_with_instances();
        let mut inbox = Inbox::default();
        let put = ThreadPut {
            key: "k".into(),
            title: "asks".into(),
            state: State::NeedsYou,
            actions: vec![act("Open", Some(HostVerb::Open(Open { url: "https://x/1".into() })))],
            ..ThreadPut::default()
        };
        inbox.put("d-id", "inst0", 10, put);
        inbox.archive_owner("d-id");
        app.set_inbox(inbox);
        app.on_key(key(KeyCode::Char('4')));
        app.inbox.view = super::super::View::All;
        app.on_key(key(KeyCode::Enter));
        assert!(target(&app).unwrap_err().starts_with("archived"));
        app.on_key(key(KeyCode::Char('1')));
        assert_eq!(app.take_pending_open(), None, "even `open` is refused on an archived thread");
        assert!(app.status.as_deref().unwrap().starts_with("archived"));

        // Owner id in no snapshot row (an empty id never matches either).
        let mut app = new_app();
        app.set_snapshot(snapshot_with_status(1, running()));
        open_thread(&mut app, None, Vec::new());
        assert_eq!(target(&app), Err("`inst0` not found".into()));
    }

    #[test]
    fn vscode_and_rm_queue_prompt_actions_on_the_target() {
        let mut app = app_with_instances();
        children(&mut app, "d-id", "pr-1", "inst1");
        let vscode = HostVerb::Vscode(Vscode { path: Some("src/a.rs".into()), line: Some(3), col: Some(5) });
        // Passes put-time validation, but a column needs a line.
        let col_only = HostVerb::Vscode(Vscode { path: Some("a.rs".into()), line: None, col: Some(2) });
        open_thread(&mut app, Some("pr-1"), vec![
            act("Code", Some(vscode)),
            act("Remove", Some(HostVerb::Rm(NoArgs {}))),
            act("Plain", Some(HostVerb::Vscode(Vscode::default()))),
            act("Col", Some(col_only)),
        ]);
        app.on_key(key(KeyCode::Char('1')));
        let goto = Goto { path: "src/a.rs".into(), line: Some(3), col: Some(5) };
        assert_eq!(
            app.pending_action.take(),
            Some(PromptAction::Code { instance: "inst1".into(), goto: Some(goto) })
        );
        assert_eq!(app.code_note, None);

        app.on_key(key(KeyCode::Char('4')));
        assert_eq!(app.pending_action.take(), Some(PromptAction::Code { instance: "inst1".into(), goto: None }));
        assert_eq!(app.code_note.take().as_deref(), Some("--goto dropped (goto column needs a line)"));

        app.on_key(key(KeyCode::Char('2')));
        assert_eq!(app.pending_action.take(), Some(PromptAction::Rm { instance: "inst1".into(), force: false }));

        // No path: nothing to say about it.
        app.on_key(key(KeyCode::Char('3')));
        assert_eq!(app.pending_action.take(), Some(PromptAction::Code { instance: "inst1".into(), goto: None }));
        assert_eq!(app.code_note, None);

        // Fixed `o`: the same target, no goto.
        app.on_key(key(KeyCode::Char('o')));
        assert_eq!(app.pending_action.take(), Some(PromptAction::Code { instance: "inst1".into(), goto: None }));
        assert_eq!(app.inbox.focus, InboxFocus::Thread);
    }

    #[test]
    fn forward_and_open_queue_their_requests() {
        let mut app = app_with_instances();
        open_thread(&mut app, None, vec![
            act("Forward", Some(HostVerb::Forward(Forward { port: 3000 }))),
            act("PR", Some(HostVerb::Open(Open { url: "https://x/pr/1".into() }))),
        ]);
        app.on_key(key(KeyCode::Char('2')));
        assert_eq!(app.take_pending_open().as_deref(), Some("https://x/pr/1"));

        app.on_key(key(KeyCode::Char('1')));
        assert_eq!(
            app.take_pending_port(),
            Some(PortRequest { instance: "inst0".into(), service: None, address: None, spec: "3000".into() })
        );
        // Like the `port` prompt: on to the Ports tab, the pane kept for later.
        assert_eq!(app.tab, Tab::Ports);
        app.on_key(key(KeyCode::Char('4')));
        assert_eq!(app.inbox.focus, InboxFocus::Thread);
    }

    #[test]
    fn terminal_targets_the_child_and_keeps_the_pane() {
        let mut app = app_with_instances();
        children(&mut app, "d-id", "pr-1", "inst1");
        open_thread(&mut app, Some("pr-1"), vec![act("Shell", Some(HostVerb::Terminal(NoArgs {})))]);
        // A live terminal on the child is focused rather than spawned, so the
        // target is visible without a PTY.
        app.terms.open(term_sess("inst0", "devsandbox-inst0"));
        app.terms.open(term_sess("inst1", "devsandbox-inst1"));
        app.terms.set_active(0);

        app.on_key(key(KeyCode::Char('1')));
        assert_eq!(app.focus, Focus::Terminal);
        assert_eq!(app.terms.active(), 1);

        // Back from the terminal: the pane is still there.
        app.on_key(crossterm::event::KeyEvent::new(KeyCode::Char(']'), crossterm::event::KeyModifiers::CONTROL));
        assert_eq!(app.focus, Focus::Dashboard);
        assert_eq!(app.inbox.focus, InboxFocus::Thread);

        // Fixed `t`: same target. A stopped child is refused by name.
        app.terms.set_active(0);
        app.on_key(key(KeyCode::Char('t')));
        assert_eq!(app.terms.active(), 1);
        app.focus = Focus::Dashboard;
        let mut snap = app.snapshot.clone().unwrap();
        snap.instances[1].status = crate::tui::data::ContainerStatus::Missing;
        app.set_snapshot(snap);
        app.on_key(key(KeyCode::Char('t')));
        assert_eq!(app.status.as_deref(), Some("terminal: `inst1` is not running"));
    }

    #[test]
    fn logs_open_on_the_target() {
        let mut app = app_with_instances();
        open_thread(&mut app, None, vec![act("Logs", Some(HostVerb::Logs(NoArgs {})))]);
        app.on_key(key(KeyCode::Char('1')));
        match &app.modal {
            Modal::Logs(v) => assert!(v.title.contains("devsandbox-inst0"), "{}", v.title),
            _ => panic!("logs modal not open"),
        }
        // Closing it returns to the pane.
        app.on_key(key(KeyCode::Esc));
        assert!(matches!(app.modal, Modal::None));
        assert_eq!(app.inbox.focus, InboxFocus::Thread);

        app.on_key(key(KeyCode::Char('l')));
        assert!(matches!(app.modal, Modal::Logs(_)));
    }

    #[test]
    fn p_prefills_the_port_prompt_for_the_target() {
        let mut app = app_with_instances();
        children(&mut app, "d-id", "pr-1", "inst1");
        open_thread(&mut app, Some("pr-1"), Vec::new());
        app.on_key(key(KeyCode::Char('p')));
        assert_eq!(prompt(&app).input(), "port inst1 ");
    }

    #[test]
    fn dispatcher_parts_and_out_of_range_keys() {
        let mut app = app_with_instances();
        let open = HostVerb::Open(Open { url: "https://x/1".into() });
        open_thread(&mut app, None, vec![
            act("Post", None),
            Action { notify: true, ..act("Look", Some(open.clone())) },
            Action { done: true, ..act("Code", Some(HostVerb::Vscode(Vscode::default()))) },
        ]);

        let id = app.selected_inbox_thread().unwrap().id;
        let acted = |action: &str| vec![Op::Act { thread: id, action: action.into() }];

        // No host: only the event, for the store to stamp.
        app.on_key(key(KeyCode::Char('1')));
        assert_eq!(app.take_pending_inbox(), acted("post"));
        assert_eq!(app.status.as_deref(), Some("[1] Post · event queued for inst0"));
        assert_eq!((app.take_pending_open(), app.pending_action.take()), (None, None));
        assert!(app.selected_inbox_thread().unwrap().events.is_empty(), "not applied to the local copy");

        // Host + notify: the host part runs, and the event is queued.
        app.on_key(key(KeyCode::Char('2')));
        assert_eq!(app.take_pending_open().as_deref(), Some("https://x/1"));
        assert_eq!(app.take_pending_inbox(), acted("look"));
        assert!(app.status.as_deref().unwrap().ends_with("event queued for inst0"), "{:?}", app.status);
        // Host + done: VS Code, plus the done event (noted on the launch).
        app.on_key(key(KeyCode::Char('3')));
        assert!(app.pending_action.take().is_some());
        assert_eq!(app.take_pending_inbox(), acted("code"));
        assert_eq!(app.code_note.take().as_deref(), Some("done queued for inst0"));

        // Past the last action: nothing at all.
        app.on_key(key(KeyCode::Char('4')));
        assert_eq!(app.status.as_deref(), Some("no action 4"));
        assert_eq!(app.tab, Tab::Inbox, "`4` stays shadowed");
        assert_eq!((app.take_pending_open(), app.pending_action.take(), app.take_pending_port()), (None, None, None));
        assert!(app.terms.is_empty() && matches!(app.modal, Modal::None));
        assert_eq!(app.take_pending_inbox(), []);
    }

    fn child_done(name: &str, done: bool) -> Vec<super::super::PendingDone> {
        vec![super::super::PendingDone { instance: name.into(), done, report: false }]
    }

    #[test]
    fn done_and_reopen_carry_to_the_child() {
        let mut app = app_with_instances();
        children(&mut app, "d-id", "pr-1", "inst1");
        open_thread(&mut app, Some("pr-1"), Vec::new());
        app.on_key(key(KeyCode::Char('d')));
        assert_eq!(app.take_pending_done(), child_done("inst1", true));
        assert_eq!(app.status.as_deref(), Some("marked done · child inst1 done · event queued for inst0"));

        // `u` on a done thread clears the child's flag.
        let mut app = app_with_instances();
        children(&mut app, "d-id", "pr-1", "inst1");
        let mut inbox = Inbox::default();
        let put = ThreadPut { key: "k".into(), title: "asks".into(), state: State::Done, child: Some("pr-1".into()), ..ThreadPut::default() };
        inbox.put("d-id", "inst0", 10, put);
        app.set_inbox(inbox);
        app.on_key(key(KeyCode::Char('4')));
        app.inbox.view = super::super::View::All;
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.inbox.focus, InboxFocus::Thread);
        app.on_key(key(KeyCode::Char('u')));
        assert_eq!(app.take_pending_done(), child_done("inst1", false));
        assert!(app.status.as_deref().unwrap().starts_with("reopened · child inst1 not done"), "{:?}", app.status);
    }

    #[test]
    fn done_without_a_resolvable_child_is_thread_only() {
        // No child: nothing to mark, nothing said about it.
        let mut app = app_with_instances();
        open_thread(&mut app, None, Vec::new());
        app.on_key(key(KeyCode::Char('d')));
        assert_eq!(app.take_pending_done(), []);
        assert_eq!(app.status.as_deref(), Some("marked done · event queued for inst0"));
        // A child that's gone: the thread is still done, with a note.
        let mut app = app_with_instances();
        open_thread(&mut app, Some("pr-1"), Vec::new());
        app.on_key(key(KeyCode::Char('d')));
        assert_eq!(app.take_pending_done(), []);
        assert!(!app.take_pending_inbox().is_empty());
        assert!(app.status.as_deref().unwrap().contains("child `pr-1` not found: not marked done"), "{:?}", app.status);
    }

    #[test]
    fn a_done_action_marks_the_child_done() {
        let mut app = app_with_instances();
        children(&mut app, "d-id", "pr-1", "inst1");
        open_thread(&mut app, Some("pr-1"), vec![
            Action { done: true, ..act("Merged", None) },
            Action { done: true, ..act("Code", Some(HostVerb::Vscode(Vscode::default()))) },
            Action { done: true, ..act("Logs", Some(HostVerb::Logs(NoArgs {}))) },
            act("Post", None),
        ]);
        app.on_key(key(KeyCode::Char('1')));
        assert_eq!(app.take_pending_done(), child_done("inst1", true));
        assert_eq!(app.status.as_deref(), Some("[1] Merged · child inst1 done · done queued for inst0"));

        app.on_key(key(KeyCode::Char('2')));
        assert_eq!(app.take_pending_done(), child_done("inst1", true));
        assert_eq!(app.code_note.take().as_deref(), Some("child inst1 done · done queued for inst0"));

        app.on_key(key(KeyCode::Char('3')));
        assert_eq!(app.take_pending_done(), child_done("inst1", true));
        app.on_key(key(KeyCode::Esc)); // close the logs modal

        // A plain event action leaves the child alone.
        app.on_key(key(KeyCode::Char('4')));
        assert_eq!(app.take_pending_done(), []);
    }

    /// A host-only button never reaches the owner; one whose host verb
    /// can't run sends nothing either.
    #[test]
    fn host_only_and_refused_targets_queue_no_event() {
        let mut app = app_with_instances();
        open_thread(&mut app, Some("gone"), vec![
            act("Shell", Some(HostVerb::Terminal(NoArgs {}))),
            Action { notify: true, ..act("Logs", Some(HostVerb::Logs(NoArgs {}))) },
        ]);
        app.on_key(key(KeyCode::Char('1')));
        app.on_key(key(KeyCode::Char('2')));
        assert!(app.status.as_deref().unwrap().contains("child `gone` not found"), "{:?}", app.status);
        assert_eq!(app.take_pending_inbox(), []);
    }
}
