//! Which threads a list shows: the Inbox views (docs/inbox-threads.md,
//! *Inbox UI*). Pure, and here rather than in the TUI because every client
//! filters with it: the dashboard's view strip and the daemon API's
//! `inbox.threads.list` (src/serve/api.rs) must never disagree on what
//! "Needs you" holds. The dashboard adds its own display bits (titles,
//! stepping) in `src/tui/app/inbox.rs`.

use super::{State, Thread};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum View {
    /// Waiting on the user ([`Thread::needs_you`]): the default, and what the
    /// badges count.
    #[default]
    NeedsYou,
    Active,
    Done,
    /// Everything, archived threads and read notify records included.
    All,
}

impl View {
    pub const ALL: [View; 4] = [View::NeedsYou, View::Active, View::Done, View::All];

    /// Whether `t` belongs in this view. Archived threads (their owner is
    /// gone) are history and only show in All; so do read notify records,
    /// which have no state to file them under.
    pub fn shows(self, t: &Thread) -> bool {
        match self {
            View::NeedsYou => t.needs_you(),
            View::Active => !t.archived && t.state == Some(State::Active),
            View::Done => !t.archived && t.state == Some(State::Done),
            View::All => true,
        }
    }

    /// The wire name (kebab-case, like [`State::as_str`]).
    pub fn as_str(self) -> &'static str {
        match self {
            View::NeedsYou => "needs-you",
            View::Active => "active",
            View::Done => "done",
            View::All => "all",
        }
    }

    pub fn parse(s: &str) -> Option<View> {
        View::ALL.into_iter().find(|v| v.as_str() == s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_names_round_trip() {
        for v in View::ALL {
            assert_eq!(View::parse(v.as_str()), Some(v));
        }
        assert_eq!(View::parse("needs_you"), None);
    }
}
