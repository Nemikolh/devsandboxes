//! Permission requests (docs/architecture/dashboard/tabs-and-actions.md,
//! *Permission requests*): each pending one the user hasn't put off opens
//! a modal, `o`/`i`/`a`/`d` answering it (once, this instance, always,
//! deny) and `esc` putting it off; `:access` reopens the oldest. A request
//! listed while another modal is up, the prompt is open or keys go to a
//! terminal or a text input waits for them to close, so a modal never
//! takes keys typed for something else. One that opens by itself ignores
//! answers for [`ANSWER_GUARD`], so a key already on its way (`a` to
//! attach, `o` for VS Code) doesn't answer it.
//!
//! I/O-free: the list comes in through [`App::set_access`], an answer goes
//! out through [`App::take_pending_decide`].

use crate::broker::asks::Answer;
use crate::serve::api::AccessRequest;

use super::view::{ConfirmModal, Confirmed};
use super::{App, Modal};

/// How long a modal that opened by itself ignores `o`/`i`/`a`/`d`.
pub const ANSWER_GUARD: std::time::Duration = std::time::Duration::from_millis(400);

/// The modal's question and body for `r`.
fn modal_text(r: &AccessRequest) -> (String, Vec<String>) {
    let question = format!("`{}` (sandbox {}) asks for credential `{}` ({})", r.instance, r.sandbox, r.credential, r.kind);
    let scope = match &r.repo {
        Some(repo) => format!("{} on {repo}", r.scope),
        None => r.scope.clone(),
    };
    let held = match r.waiting {
        0 => "no: once lets its next matching call through within 5 minutes".to_string(),
        1 => "yes, a call waits on the answer".to_string(),
        n => format!("yes, {n} calls wait on the answer"),
    };
    let body = vec![
        format!("  scope    {scope}"),
        format!("  call     {} {}", r.method, r.route),
        format!("  held     {held}"),
        format!("  request  {}", r.id),
    ];
    (question, body)
}

impl App {
    /// The daemon's pending requests: a modal on one answered or expired
    /// meanwhile closes, and the next one not put off opens when nothing
    /// else is up.
    pub fn set_access(&mut self, list: Vec<AccessRequest>) {
        self.access_quiet.retain(|id| list.iter().any(|r| &r.id == id));
        self.access = list;
        if let Modal::Confirm(view) = &self.modal
            && let Some(id) = view.access_id()
            && !self.access.iter().any(|r| r.id == id)
        {
            self.status = Some(format!("permission request {id}: no longer pending"));
            self.modal = Modal::None;
        }
        self.open_access();
    }

    /// Opens the oldest request not put off, if keys would reach it.
    pub(super) fn open_access(&mut self) {
        if !matches!(self.modal, Modal::None)
            || self.prompt.is_some()
            || self.focus == super::Focus::Terminal
            || self.takes_raw_keys()
        {
            return;
        }
        if let Some(r) = self.access.iter().find(|r| !self.access_quiet.contains(&r.id)).cloned() {
            self.show_access(&r);
            self.access_guard = self.clock.map(|now| now + ANSWER_GUARD);
        }
    }

    /// The event loop's time, read by the answer guard.
    pub fn set_clock(&mut self, now: std::time::Instant) {
        self.clock = Some(now);
    }

    /// Whether an answer key now would be one typed before the modal
    /// that opened by itself could be seen.
    pub(super) fn access_guarded(&self) -> bool {
        matches!((self.access_guard, self.clock), (Some(until), Some(now)) if now < until)
    }

    /// `:access`: the oldest pending request, put off or not.
    pub(super) fn reopen_access(&mut self) {
        match self.access.first().cloned() {
            Some(r) => self.show_access(&r),
            None => self.status = Some("no pending permission requests".into()),
        }
    }

    fn show_access(&mut self, r: &AccessRequest) {
        self.access_guard = None;
        let (question, body) = modal_text(r);
        self.modal = Modal::Confirm(ConfirmModal::new(question, body, Confirmed::Access(r.id.clone())));
    }

    /// The modal's key: an answer goes to the daemon, `None` (`esc`) puts
    /// the request off. Either way it doesn't open again by itself.
    pub(super) fn access_chosen(&mut self, id: String, answer: Option<Answer>) {
        self.modal = Modal::None;
        self.access_quiet.insert(id.clone());
        match answer {
            Some(answer) => {
                self.status = Some(format!("answering {id}…"));
                self.pending_decide = Some((id, answer));
            }
            None => self.status = Some(format!("{id} put off: :access reopens it")),
        }
        self.open_access();
    }

    /// The header's note while requests are pending.
    pub fn access_note(&self) -> Option<String> {
        match self.access.len() {
            0 => None,
            1 => Some("1 permission request (:access)".into()),
            n => Some(format!("{n} permission requests (:access)")),
        }
    }

    /// Take the answer for the daemon worker, if any.
    pub fn take_pending_decide(&mut self) -> Option<(String, Answer)> {
        self.pending_decide.take()
    }
}

#[cfg(test)]
mod tests {
    use crate::broker::asks::Answer;
    use crate::serve::api::AccessRequest;
    use crate::tui::app::test_support::*;
    use crate::tui::app::*;

    fn request(id: &str) -> AccessRequest {
        AccessRequest {
            id: id.into(),
            instance: "web-1".into(),
            instance_id: "i1".into(),
            sandbox: "web".into(),
            credential: "github-work".into(),
            kind: "github".into(),
            scope: "pr.write".into(),
            repo: Some("o/r".into()),
            method: "POST".into(),
            route: "/repos/o/r/pulls".into(),
            opened_at: 1,
            held: true,
            waiting: 1,
        }
    }

    fn shown(app: &App) -> Option<&str> {
        match &app.modal {
            Modal::Confirm(m) => m.access_id(),
            _ => None,
        }
    }

    #[test]
    fn a_new_request_opens_its_modal() {
        let mut app = new_app();
        app.set_access(vec![request("R-1")]);
        assert_eq!(shown(&app), Some("R-1"));
        let Modal::Confirm(m) = &app.modal else { unreachable!() };
        assert_eq!(m.question, "`web-1` (sandbox web) asks for credential `github-work` (github)");
        assert_eq!(
            m.body,
            [
                "  scope    pr.write on o/r",
                "  call     POST /repos/o/r/pulls",
                "  held     yes, a call waits on the answer",
                "  request  R-1",
            ]
        );
        assert_eq!(m.choices(), "[o] once  [i] this instance  [a] always  [d] deny  [esc] later");
        assert_eq!(app.access_note().as_deref(), Some("1 permission request (:access)"));
        // A second one waits for the first.
        app.set_access(vec![request("R-1"), request("R-2")]);
        assert_eq!(shown(&app), Some("R-1"));
        assert_eq!(app.access_note().as_deref(), Some("2 permission requests (:access)"));
    }

    #[test]
    fn not_over_another_modal_the_prompt_or_a_terminal() {
        let mut app = new_app();
        app.open_help();
        app.set_access(vec![request("R-1")]);
        assert!(matches!(app.modal, Modal::Help(_)));
        // Queued: it opens once the help closes.
        app.on_key(key(KeyCode::Esc));
        assert_eq!(shown(&app), Some("R-1"));

        let mut app = new_app();
        app.on_key(key(KeyCode::Char(':')));
        app.set_access(vec![request("R-1")]);
        assert!(shown(&app).is_none());
        app.on_key(key(KeyCode::Esc));
        assert_eq!(shown(&app), Some("R-1"));

        let mut app = new_app();
        app.focus = Focus::Terminal;
        app.set_access(vec![request("R-1")]);
        assert!(shown(&app).is_none());
    }

    #[test]
    fn a_request_answered_elsewhere_closes_its_modal() {
        let mut app = new_app();
        app.set_access(vec![request("R-1"), request("R-2")]);
        app.set_access(vec![request("R-2")]);
        assert_eq!(shown(&app), Some("R-2"), "the next one opens");
        assert_eq!(app.status.as_deref(), Some("permission request R-1: no longer pending"));
        app.set_access(Vec::new());
        assert!(matches!(app.modal, Modal::None));
        assert_eq!(app.access_note(), None);
    }

    #[test]
    fn each_key_queues_its_answer() {
        for (c, answer) in [('o', Answer::Once), ('i', Answer::Instance), ('a', Answer::Always), ('d', Answer::Deny)] {
            let mut app = new_app();
            app.set_access(vec![request("R-1")]);
            app.on_key(key(KeyCode::Char(c)));
            assert!(matches!(app.modal, Modal::None), "{c}");
            assert_eq!(app.take_pending_decide(), Some(("R-1".to_string(), answer)), "{c}");
            // Still listed until the daemon says otherwise: not asked again.
            app.set_access(vec![request("R-1")]);
            assert!(shown(&app).is_none(), "{c}");
        }
        let mut app = new_app();
        app.set_access(vec![request("R-1")]);
        app.on_key(key(KeyCode::Char('y')));
        assert_eq!(shown(&app), Some("R-1"), "other keys do nothing");
        assert_eq!(app.take_pending_decide(), None);
    }

    #[test]
    fn a_modal_that_opened_by_itself_ignores_answers_at_first() {
        use super::ANSWER_GUARD;
        let t0 = std::time::Instant::now();
        let mut app = new_app();
        app.set_clock(t0);
        app.set_access(vec![request("R-1")]);
        app.set_clock(t0 + ANSWER_GUARD - std::time::Duration::from_millis(1));
        app.on_key(key(KeyCode::Char('a')));
        assert_eq!(shown(&app), Some("R-1"), "still open");
        assert_eq!(app.take_pending_decide(), None);
        app.set_clock(t0 + ANSWER_GUARD);
        app.on_key(key(KeyCode::Char('a')));
        assert_eq!(app.take_pending_decide(), Some(("R-1".to_string(), Answer::Always)));

        // esc works inside the window.
        let mut app = new_app();
        app.set_clock(t0);
        app.set_access(vec![request("R-1")]);
        app.on_key(key(KeyCode::Esc));
        assert!(matches!(app.modal, Modal::None));

        // `:access` answers at once.
        for c in ":access".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Enter));
        assert_eq!(shown(&app), Some("R-1"));
        app.on_key(key(KeyCode::Char('d')));
        assert_eq!(app.take_pending_decide(), Some(("R-1".to_string(), Answer::Deny)));
    }

    #[test]
    fn esc_puts_off_until_access() {
        let mut app = new_app();
        app.set_access(vec![request("R-1")]);
        app.on_key(key(KeyCode::Esc));
        assert!(matches!(app.modal, Modal::None));
        assert_eq!(app.take_pending_decide(), None);
        app.set_access(vec![request("R-1")]);
        app.on_key(key(KeyCode::Down));
        assert!(shown(&app).is_none(), "put off");
        // A new one still opens.
        app.set_access(vec![request("R-1"), request("R-2")]);
        assert_eq!(shown(&app), Some("R-2"));
        app.on_key(key(KeyCode::Esc));

        for c in ":access".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Enter));
        assert_eq!(shown(&app), Some("R-1"), "the oldest");
        app.on_key(key(KeyCode::Char('o')));
        assert_eq!(app.take_pending_decide(), Some(("R-1".to_string(), Answer::Once)));

        let mut app = new_app();
        for c in ":access".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.status.as_deref(), Some("no pending permission requests"));
    }
}
