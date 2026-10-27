//! The identity modals (docs/architecture/dashboard/tabs-and-actions.md,
//! *Identity modals*): before a `run`/`rebuild`/`start` is sent for a
//! sandbox whose row carries un-acked identity notes, a three-choice modal holds it (`[u]pdate / [c]ontinue as
//! is / [s]kip this time`, as the CLI's prompt, `serve::verbs_client::
//! identity`); and the offer, on the first snapshot and on `U`, listing
//! every un-acked finding of the roots (unused templates included) with
//! `update / continue as is / later`.
//!
//! I/O-free: what the user picks is an [`IdentityReq`] the event loop hands
//! to the daemon worker, and its answer comes back through
//! [`App::identity_offer`] / [`App::identity_answered`]. The held verb is
//! released through `cleared_action`, which [`App::take_pending_action`]
//! returns without asking again.

use crate::serve::verbs_client::identity::{self, Choice, Group};
use crate::snapshot::SandboxRow;
use crate::tui::data::{Snapshot, group_of};
use crate::tui::prompt::PromptAction;
use crate::tui::remote::{Route, find_instance, machine_root, sandbox_key};

use super::view::{ConfirmModal, Confirmed};
use super::{App, Modal, VerbEnd};

/// What the event loop asks the daemon worker for.
#[derive(Clone, Debug, PartialEq)]
pub enum IdentityReq {
    /// The offer's findings: every un-acked one in these roots (a default
    /// route: the dashboard's own). `asked`: `U`, which says so when
    /// there's nothing; the first snapshot's is quiet.
    Offer { routes: Vec<Route>, asked: bool },
    /// An answer (`Update` or `Keep`), and the verb it holds, sent once
    /// done (but after a `conflict`).
    Answer { choice: Choice, on: IdentityOn, then: Option<PromptAction> },
}

/// Which findings an answer is about.
#[derive(Clone, Debug, PartialEq)]
pub enum IdentityOn {
    /// The offer's, as fetched (their file hashes guard the write).
    Groups(Vec<Group>),
    /// A sandbox row's notes, by id, in the root at `route`.
    Notes { route: Route, ids: Vec<String> },
}

/// The sandbox row verb `action` creates or rebuilds an instance of:
/// `run`'s sandbox, `rebuild`/`start`'s instance's group. `None` for the
/// other verbs (and an orphan).
fn target<'a>(s: &'a Snapshot, action: &PromptAction) -> Option<&'a SandboxRow> {
    match action {
        PromptAction::Run { sandbox, .. } => s.sandboxes.iter().find(|r| sandbox_key(r) == *sandbox),
        PromptAction::Rebuild { instance, .. } | PromptAction::Start { instance } => {
            let inst = find_instance(s, instance)?;
            s.sandboxes.get(group_of(&s.sandboxes, inst)?)
        }
        _ => None,
    }
}

/// Where a sandbox row's config is: its machine and root there; this
/// host's rows route nowhere special (the dashboard's root).
fn row_route(s: &Snapshot, row: &SandboxRow) -> Route {
    match &row.machine {
        None => Route::default(),
        Some(m) => Route { machine: Some(m.clone()), root: row.root.clone().or_else(|| machine_root(s, m)) },
    }
}

/// The roots the offer reads: the dashboard's own, plus each machine root
/// whose rows carry un-acked notes (a machine's unused templates are left
/// to its own dashboard).
pub(super) fn offer_routes(s: &Snapshot) -> Vec<Route> {
    let mut routes = vec![Route::default()];
    for row in &s.sandboxes {
        if row.machine.is_some() && row.identity.iter().any(|n| !n.acked) {
            let route = row_route(s, row);
            if !routes.contains(&route) {
                routes.push(route);
            }
        }
    }
    routes
}

impl App {
    /// The identity check on a verb about to go out: `Some` when it may,
    /// else the modal holding it is open.
    pub(super) fn identity_gate(&mut self, action: PromptAction) -> Option<PromptAction> {
        let Some(s) = &self.snapshot else { return Some(action) };
        let Some(row) = target(s, &action) else { return Some(action) };
        let notes: Vec<_> = row.identity.iter().filter(|n| !n.acked).collect();
        if notes.is_empty() {
            return Some(action);
        }
        let ids = notes.iter().map(|n| n.id.clone()).collect();
        let body = notes.iter().map(|n| format!("  {}", n.text)).collect();
        let question = format!("sandbox {} assumes root in the container, which breaks under identity = \"host\":", sandbox_key(row));
        let route = row_route(s, row);
        self.modal = Modal::Confirm(ConfirmModal::new(question, body, Confirmed::Identity { route, ids, action }));
        None
    }

    /// `U` (Instances / Services): read the offer's findings.
    pub(super) fn request_identity_offer(&mut self) {
        let routes = self.snapshot.as_ref().map(offer_routes).unwrap_or_else(|| vec![Route::default()]);
        self.status = Some("reading the config for /root…".into());
        self.pending_identity = Some(IdentityReq::Offer { routes, asked: true });
    }

    /// The first snapshot's offer, once per dashboard.
    pub(super) fn offer_identity_once(&mut self) {
        if std::mem::replace(&mut self.identity_offered, true) {
            return;
        }
        if let Some(s) = &self.snapshot {
            self.pending_identity = Some(IdentityReq::Offer { routes: offer_routes(s), asked: false });
        }
    }

    /// The offer's findings came: list them, unless there are none (`U`
    /// says so) or, for the quiet first one, another modal is up.
    pub fn identity_offer(&mut self, groups: Vec<Group>, asked: bool) {
        if groups.is_empty() {
            if asked {
                self.status = Some("no config uses /root".into());
            }
            return;
        }
        if !asked && !matches!(self.modal, Modal::None) {
            return;
        }
        if asked {
            self.status = None;
        }
        let question = "update these to work under identity = \"host\"?".to_string();
        let body = identity::listing(&groups);
        self.modal = Modal::Confirm(ConfirmModal::new(question, body, Confirmed::IdentityOffer(groups)));
    }

    /// A modal's answer: `u`/`c` go to the worker with the held verb, a
    /// skip (or the offer's later) sends it as is.
    pub(super) fn identity_chosen(&mut self, choice: Choice, on: IdentityOn, then: Option<PromptAction>) {
        match choice {
            Choice::Skip => self.cleared_action = then,
            Choice::Update | Choice::Keep => {
                self.status = Some(match choice {
                    Choice::Update => "updating the config…".into(),
                    _ => "recording that…".into(),
                });
                self.pending_identity = Some(IdentityReq::Answer { choice, on, then });
            }
        }
    }

    /// The worker's report on an answer: what it did (`Ok`, maybe empty),
    /// then the held verb goes out with that line ahead of its own; `Err`
    /// is a file changed meanwhile: the verb isn't sent, and its guards
    /// (`s`'s in-flight sets) clear as for a failed verb.
    pub fn identity_answered(&mut self, outcome: Result<String, String>, then: Option<PromptAction>) {
        match outcome {
            Ok(line) => {
                if !line.is_empty() {
                    self.status = Some(line.clone());
                }
                if then.is_some() {
                    self.identity_note = (!line.is_empty()).then_some(line);
                    self.cleared_action = then;
                }
            }
            Err(why) => match then {
                Some(action) => self.verb_answered(&action, VerbEnd::Done { status: why, failed: true }),
                None => self.status = Some(why),
            },
        }
    }

    /// Take the request for the daemon worker, if any.
    pub fn take_pending_identity(&mut self) -> Option<IdentityReq> {
        self.pending_identity.take()
    }

    /// What an answer said, for the status line of the verb it released.
    pub fn take_identity_note(&mut self) -> Option<String> {
        self.identity_note.take()
    }
}

#[cfg(test)]
mod tests {
    use crate::snapshot::IdentityNote;
    use crate::tui::app::test_support::*;
    use crate::tui::app::*;
    use crate::tui::remote::Route;
    use crate::serve::verbs_client::identity::{Choice, Group};

    use super::{IdentityOn, IdentityReq};

    fn note(id: &str, acked: bool) -> IdentityNote {
        IdentityNote { id: id.into(), text: format!("sandbox.s: mounts entry {id}"), acked }
    }

    /// One sandbox `s`, instances `inst0`/`inst1` exited, the sandbox's
    /// notes `notes`; the first snapshot's offer already taken.
    fn app_with(notes: Vec<IdentityNote>) -> App {
        let mut app = new_app();
        let mut s = snapshot_with_status(2, crate::tui::data::ContainerStatus::Exited("Exited (0)".into()));
        s.sandboxes[0].identity = notes;
        app.set_snapshot(s);
        app.take_pending_identity();
        app
    }

    fn start() -> PromptAction {
        PromptAction::Start { instance: "inst0".into() }
    }

    fn identity_modal(app: &App) -> Option<(&str, Vec<String>)> {
        match &app.modal {
            Modal::Confirm(m) if m.is_identity() => Some((m.question.as_str(), m.body.clone())),
            _ => None,
        }
    }

    #[test]
    fn the_modal_holds_a_build_verb_only_with_unacked_notes() {
        // None, or acked only: straight through.
        for notes in [vec![], vec![note("a", true)]] {
            let mut app = app_with(notes);
            app.pending_action = Some(start());
            assert_eq!(app.take_pending_action(), Some(start()));
            assert!(identity_modal(&app).is_none());
        }
        let mut app = app_with(vec![note("a", true), note("b", false)]);
        // Not a build verb: no modal.
        app.pending_action = Some(PromptAction::Stop { instance: "inst0".into() });
        assert_eq!(app.take_pending_action(), Some(PromptAction::Stop { instance: "inst0".into() }));
        for action in [
            start(),
            PromptAction::Rebuild { instance: "inst1".into(), force: false },
            PromptAction::Run { sandbox: "s".into(), name: None, branch: None, base: None },
        ] {
            app.pending_action = Some(action.clone());
            assert_eq!(app.take_pending_action(), None, "{action:?}");
            let (question, body) = identity_modal(&app).expect("modal");
            assert!(question.starts_with("sandbox s assumes root"), "{question}");
            assert_eq!(body, vec!["  sandbox.s: mounts entry b".to_string()], "the un-acked notes only");
            app.modal = Modal::None;
        }
    }

    /// `s` sends the held verb as is; `u`/`c` queue the answer holding it;
    /// esc is a skip too. The verb, once released, isn't asked about again.
    #[test]
    fn each_key_of_the_modal() {
        for (code, choice) in [(KeyCode::Char('u'), Some(Choice::Update)), (KeyCode::Char('c'), Some(Choice::Keep))] {
            let mut app = app_with(vec![note("b", false)]);
            app.pending_action = Some(start());
            assert_eq!(app.take_pending_action(), None);
            app.on_key(key(code));
            assert!(matches!(app.modal, Modal::None));
            assert_eq!(app.take_pending_action(), None, "held until answered");
            assert_eq!(
                app.take_pending_identity(),
                Some(IdentityReq::Answer {
                    choice: choice.unwrap(),
                    on: IdentityOn::Notes { route: Route::default(), ids: vec!["b".into()] },
                    then: Some(start()),
                })
            );
            // Done: the verb goes, the answer's line ahead of its own.
            app.identity_answered(Ok("updated devsandboxes.toml (1 edit)".into()), Some(start()));
            assert_eq!(app.take_pending_action(), Some(start()), "not asked again");
            assert_eq!(app.take_identity_note().as_deref(), Some("updated devsandboxes.toml (1 edit)"));
        }
        for code in [KeyCode::Char('s'), KeyCode::Esc] {
            let mut app = app_with(vec![note("b", false)]);
            app.pending_action = Some(start());
            assert_eq!(app.take_pending_action(), None);
            app.on_key(key(code));
            assert!(matches!(app.modal, Modal::None));
            assert_eq!(app.take_pending_identity(), None);
            assert_eq!(app.take_pending_action(), Some(start()), "{code:?}");
        }
        // Other keys leave it up.
        let mut app = app_with(vec![note("b", false)]);
        app.pending_action = Some(start());
        app.take_pending_action();
        app.on_key(key(KeyCode::Char('y')));
        assert!(identity_modal(&app).is_some());
    }

    /// A conflict drops the held verb and clears its guard; `s`'s start
    /// goes through the same modal.
    #[test]
    fn a_conflict_sends_nothing_and_clears_the_guard() {
        let mut app = app_with(vec![note("b", false)]);
        app.on_key(key(KeyCode::Down)); // onto inst0
        app.on_key(key(KeyCode::Char('s')));
        assert!(app.starting.contains("inst0"));
        assert_eq!(app.take_pending_action(), None);
        app.on_key(key(KeyCode::Char('u')));
        let Some(IdentityReq::Answer { then, .. }) = app.take_pending_identity() else { panic!("no answer") };
        app.identity_answered(Err("devsandboxes.toml changed while you were asked".into()), then);
        assert_eq!(app.take_pending_action(), None);
        assert!(!app.starting.contains("inst0"));
        assert_eq!(app.status.as_deref(), Some("devsandboxes.toml changed while you were asked"));
    }

    /// The first snapshot asks for the offer once; `U` any time, from the
    /// Instances and Services tabs.
    #[test]
    fn the_offer_is_read_on_the_first_snapshot_and_on_u() {
        let mut app = new_app();
        app.set_snapshot(snapshot_with(1));
        assert_eq!(app.take_pending_identity(), Some(IdentityReq::Offer { routes: vec![Route::default()], asked: false }));
        app.set_snapshot(snapshot_with(1));
        assert_eq!(app.take_pending_identity(), None, "once");

        app.on_key(key(KeyCode::Char('U')));
        assert_eq!(app.take_pending_identity(), Some(IdentityReq::Offer { routes: vec![Route::default()], asked: true }));
        app.set_tab(Tab::Services);
        app.on_key(key(KeyCode::Char('U')));
        assert!(app.take_pending_identity().is_some());
        app.set_tab(Tab::Ports);
        app.on_key(key(KeyCode::Char('U')));
        assert_eq!(app.take_pending_identity(), None);
    }

    /// A machine's root with un-acked notes is read too.
    #[test]
    fn the_offer_reads_machines_roots_with_notes() {
        let mut app = machine_app();
        app.take_pending_identity();
        let mut s = app.snapshot.clone().unwrap();
        s.sandboxes[1].identity = vec![note("b", false)];
        app.set_snapshot(s);
        app.on_key(key(KeyCode::Char('U')));
        let box_root = Route { machine: Some("box".into()), root: Some("/r/one".into()) };
        assert_eq!(
            app.take_pending_identity(),
            Some(IdentityReq::Offer { routes: vec![Route::default(), box_root], asked: true })
        );
    }

    #[test]
    fn the_offer_lists_and_answers() {
        let group = Group::sample("/cfg", &[("a", "sandbox.web: mounts entry \"/root/x\" -> \"${containerEnv:HOME}/x\"")]);
        // Nothing: `U` says so, the first one is quiet.
        let mut app = app_with(vec![]);
        app.identity_offer(Vec::new(), false);
        assert_eq!(app.status, None);
        app.identity_offer(Vec::new(), true);
        assert_eq!(app.status.as_deref(), Some("no config uses /root"));

        app.identity_offer(vec![group.clone()], false);
        let (_, body) = identity_modal(&app).expect("offer");
        assert_eq!(body[0], "/cfg/devsandboxes.toml assumes root in the container, which breaks under identity = \"host\":");
        assert!(body[1].contains("${containerEnv:HOME}/x"));
        app.on_key(key(KeyCode::Char('c')));
        assert_eq!(
            app.take_pending_identity(),
            Some(IdentityReq::Answer { choice: Choice::Keep, on: IdentityOn::Groups(vec![group.clone()]), then: None })
        );
        // Later: nothing.
        app.identity_offer(vec![group.clone()], true);
        app.on_key(key(KeyCode::Char('l')));
        assert!(matches!(app.modal, Modal::None));
        assert_eq!(app.take_pending_identity(), None);
        // The quiet one doesn't cover another modal.
        app.open_help();
        app.identity_offer(vec![group], false);
        assert!(matches!(app.modal, Modal::Help(_)));
    }
}
