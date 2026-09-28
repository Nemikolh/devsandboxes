//! Inbox tab: container notifications (`devsbd notify`, docs/automations.md)
//! kept in memory, newest first. The event loop pushes what the bridge worker
//! drains; everything here is plain state so dedupe, cap, unread and selection
//! rules stay unit-testable. Opening a link is only *requested* here
//! (`pending_open`); the loop spawns the opener.

use crate::devsbd::notify::Record;

use super::{App, Tab};

/// Entries kept; the oldest drop off beyond this. In memory only: the
/// container outbox is the durable queue, the inbox is a session view.
pub const INBOX_CAP: usize = 200;

/// Entries kept per instance, within [`INBOX_CAP`]: one noisy container drops
/// its own oldest rows instead of evicting every other instance's.
pub const INBOX_INSTANCE_CAP: usize = 50;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboxEntry {
    /// Stable per-session id, so the selection can follow an entry while rows
    /// are inserted above it or removed.
    pub id: u64,
    /// Instance (state key) the notification came from.
    pub instance: String,
    pub record: Record,
    pub unread: bool,
}

#[derive(Default)]
pub struct Inbox {
    /// Newest first.
    pub entries: Vec<InboxEntry>,
    next_id: u64,
}

impl Inbox {
    /// Insert at the top. A keyed record replaces the older entry with the
    /// same `(instance, key)`: a dispatcher re-reporting the same condition
    /// every poll keeps one row, bumped to the top and unread again. Keys are
    /// per instance, since two sandboxes can't know about each other's keys.
    fn push(&mut self, instance: String, record: Record, unread: bool) {
        if let Some(key) = &record.key {
            self.entries
                .retain(|e| !(e.instance == instance && e.record.key.as_ref() == Some(key)));
        }
        let id = self.next_id;
        self.next_id += 1;
        let mine = self.entries.iter().filter(|e| e.instance == instance).count();
        if mine >= INBOX_INSTANCE_CAP {
            if let Some(oldest) = self.entries.iter().rposition(|e| e.instance == instance) {
                self.entries.remove(oldest);
            }
        }
        self.entries.insert(0, InboxEntry { id, instance, record, unread });
        self.entries.truncate(INBOX_CAP);
    }

    pub fn unread(&self) -> usize {
        self.entries.iter().filter(|e| e.unread).count()
    }

    /// Unread entries from `instance`, for the Instances-row badge.
    pub fn unread_for(&self, instance: &str) -> usize {
        self.entries
            .iter()
            .filter(|e| e.unread && e.instance == instance)
            .count()
    }

    fn mark_all_read(&mut self) {
        for e in &mut self.entries {
            e.unread = false;
        }
    }
}

/// Whether `link` is an `http(s)://` URL. The link comes from inside the
/// container and ends up as the opener's argv, so only web links pass: no
/// bare paths, no leading `-` (an option to `xdg-open`/`open`), and no
/// `file:` or custom schemes that would hand the container a host handler.
fn is_url(link: &str) -> bool {
    ["http://", "https://"]
        .iter()
        .any(|p| link.len() > p.len() && link[..p.len()].eq_ignore_ascii_case(p))
}

impl App {
    /// Take one drained notification. Arriving while the Inbox tab is shown
    /// counts as seen; elsewhere it's unread (tab title + instance badge).
    /// A cursor on the top row stays on top (the newest); one moved down
    /// stays on the entry it was on.
    #[cfg_attr(not(unix), allow(dead_code))] // fed by the unix-only bridge worker
    pub fn push_notification(&mut self, instance: String, record: Record) {
        let selected = match self.selected[Tab::Inbox.index()] {
            0 => None,
            _ => self.selected_inbox_id(),
        };
        let unread = self.tab != Tab::Inbox;
        self.inbox.push(instance, record, unread);
        self.reselect_inbox(selected);
    }

    /// Switch tabs; entering the Inbox marks everything read.
    pub(super) fn set_tab(&mut self, tab: Tab) {
        self.tab = tab;
        if tab == Tab::Inbox {
            self.inbox.mark_all_read();
        }
    }

    /// Tab-bar title, carrying the unread count on the Inbox.
    pub fn tab_title(&self, tab: Tab) -> String {
        match (tab, self.inbox.unread()) {
            (Tab::Inbox, n) if n > 0 => format!("{} ({n})", tab.title()),
            _ => tab.title().to_string(),
        }
    }

    /// The entry under the Inbox cursor, if any.
    pub fn selected_inbox_entry(&self) -> Option<&InboxEntry> {
        self.inbox.entries.get(self.selected[Tab::Inbox.index()])
    }

    fn selected_inbox_id(&self) -> Option<u64> {
        self.selected_inbox_entry().map(|e| e.id)
    }

    /// Put the cursor back on entry `id` if it survived, else re-clamp.
    fn reselect_inbox(&mut self, id: Option<u64>) {
        if let Some(pos) = id.and_then(|id| self.inbox.entries.iter().position(|e| e.id == id)) {
            self.selected[Tab::Inbox.index()] = pos;
        }
        self.clamp_selection();
    }

    /// `d` (Inbox tab): dismiss the selected entry.
    pub(super) fn dismiss_selected_notification(&mut self) {
        let slot = Tab::Inbox.index();
        if self.selected[slot] < self.inbox.entries.len() {
            self.inbox.entries.remove(self.selected[slot]);
            self.clamp_selection();
        }
    }

    /// `D` (Inbox tab): clear the inbox.
    pub(super) fn clear_notifications(&mut self) {
        self.inbox.entries.clear();
        self.clamp_selection();
    }

    /// `enter` (Inbox tab): ask the event loop to open the selected entry's
    /// link. No link → no-op; a non-URL link → status line, nothing opened.
    pub(super) fn open_selected_link(&mut self) {
        let Some(link) = self.selected_inbox_entry().and_then(|e| e.record.link.clone()) else {
            return;
        };
        if is_url(&link) {
            self.status = Some(format!("opening {link}"));
            self.pending_open = Some(link);
        } else {
            self.status = Some(format!("not a URL, not opening: {link}"));
        }
    }

    /// Take the pending link for the event loop to open, if any.
    pub fn take_pending_open(&mut self) -> Option<String> {
        self.pending_open.take()
    }
}

/// `HH:MM` of unix time `at`, shifted by `utc_offset` seconds.
pub fn clock(at: u64, utc_offset: i64) -> String {
    let secs = (at as i64 + utc_offset).rem_euclid(86_400);
    format!("{:02}:{:02}", secs / 3600, secs % 3600 / 60)
}

/// Parse `date +%z` output (`+0200`, `-0530`) into seconds east of UTC.
pub fn parse_utc_offset(s: &str) -> Option<i64> {
    let s = s.trim();
    let (sign, digits) = match s.as_bytes().first()? {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => return None,
    };
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let hours: i64 = digits[..2].parse().ok()?;
    let minutes: i64 = digits[2..].parse().ok()?;
    Some(sign * (hours * 3600 + minutes * 60))
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use crate::devsbd::notify::Level;
    use crossterm::event::KeyCode;

    fn rec(msg: &str, key: Option<&str>, link: Option<&str>) -> Record {
        Record {
            level: Level::Info,
            key: key.map(str::to_string),
            link: link.map(str::to_string),
            msg: msg.into(),
            at: 0,
        }
    }

    fn msgs(app: &App) -> Vec<(&str, &str)> {
        app.inbox
            .entries
            .iter()
            .map(|e| (e.instance.as_str(), e.record.msg.as_str()))
            .collect()
    }

    #[test]
    fn push_is_newest_first() {
        let mut app = new_app();
        app.push_notification("a".into(), rec("one", None, None));
        app.push_notification("b".into(), rec("two", None, None));
        assert_eq!(msgs(&app), [("b", "two"), ("a", "one")]);
    }

    #[test]
    fn dedupe_by_key_per_instance() {
        let mut app = new_app();
        app.push_notification("a".into(), rec("pr 1 v1", Some("pr-1"), None));
        app.push_notification("a".into(), rec("other", None, None));
        app.push_notification("b".into(), rec("b pr 1", Some("pr-1"), None));
        app.set_tab(Tab::Inbox); // read everything
        app.set_tab(Tab::Instances);
        app.push_notification("a".into(), rec("pr 1 v2", Some("pr-1"), None));
        // Same (instance, key) replaced and moved to the top; same key from
        // another instance is its own row; unkeyed entries never dedupe.
        assert_eq!(msgs(&app), [("a", "pr 1 v2"), ("b", "b pr 1"), ("a", "other")]);
        assert!(app.inbox.entries[0].unread);
        assert_eq!(app.inbox.unread(), 1);
    }

    #[test]
    fn capped_dropping_oldest() {
        let mut app = new_app();
        // Spread over instances so only the global cap applies.
        for i in 0..INBOX_CAP + 5 {
            let instance = format!("i{}", i % 10);
            app.push_notification(instance, rec(&i.to_string(), None, None));
        }
        assert_eq!(app.inbox.entries.len(), INBOX_CAP);
        assert_eq!(app.inbox.entries[0].record.msg, (INBOX_CAP + 4).to_string());
        assert_eq!(app.inbox.entries.last().unwrap().record.msg, "5");
    }

    #[test]
    fn per_instance_cap_drops_only_that_instances_oldest() {
        let mut app = new_app();
        app.push_notification("quiet".into(), rec("q0", None, None));
        for i in 0..INBOX_INSTANCE_CAP {
            app.push_notification("noisy".into(), rec(&i.to_string(), None, None));
        }
        app.push_notification("quiet".into(), rec("q1", None, None));
        assert_eq!(app.inbox.entries.len(), INBOX_INSTANCE_CAP + 2);
        // The 51st from `noisy` drops `noisy`'s oldest ("0"), not `quiet`'s.
        app.push_notification("noisy".into(), rec("new", None, None));
        let noisy: Vec<_> = msgs(&app).into_iter().filter(|(i, _)| *i == "noisy").map(|(_, m)| m).collect();
        assert_eq!(noisy.len(), INBOX_INSTANCE_CAP);
        assert_eq!(noisy[0], "new");
        assert_eq!(*noisy.last().unwrap(), "1");
        let quiet: Vec<_> = msgs(&app).into_iter().filter(|(i, _)| *i == "quiet").map(|(_, m)| m).collect();
        assert_eq!(quiet, ["q1", "q0"]);
    }

    #[test]
    fn unread_count_and_read_on_entering_tab() {
        let mut app = new_app();
        app.push_notification("a".into(), rec("one", None, None));
        app.push_notification("a".into(), rec("two", None, None));
        assert_eq!(app.inbox.unread(), 2);
        assert_eq!(app.tab_title(Tab::Inbox), "Inbox (2)");
        assert_eq!(app.tab_title(Tab::Ports), "Ports");

        app.on_key(key(KeyCode::Char('4')));
        assert_eq!(app.tab, Tab::Inbox);
        assert_eq!(app.inbox.unread(), 0);
        assert_eq!(app.tab_title(Tab::Inbox), "Inbox");

        // Arriving while the Inbox is shown counts as seen.
        app.push_notification("a".into(), rec("three", None, None));
        assert_eq!(app.inbox.unread(), 0);

        // Tab-cycling into the Inbox marks read too.
        app.on_key(key(KeyCode::Char('1')));
        app.push_notification("a".into(), rec("four", None, None));
        app.on_key(key(KeyCode::BackTab)); // Instances → Inbox (wraps)
        assert_eq!(app.tab, Tab::Inbox);
        assert_eq!(app.inbox.unread(), 0);
    }

    #[test]
    fn badge_count_per_instance() {
        let mut app = new_app();
        app.push_notification("a".into(), rec("1", None, None));
        app.push_notification("a".into(), rec("2", None, None));
        app.push_notification("b".into(), rec("3", None, None));
        assert_eq!(app.inbox.unread_for("a"), 2);
        assert_eq!(app.inbox.unread_for("b"), 1);
        assert_eq!(app.inbox.unread_for("c"), 0);
        app.set_tab(Tab::Inbox);
        assert_eq!(app.inbox.unread_for("a"), 0);
    }

    #[test]
    fn dismiss_and_clear() {
        let mut app = new_app();
        for m in ["1", "2", "3"] {
            app.push_notification("a".into(), rec(m, None, None));
        }
        app.on_key(key(KeyCode::Char('4')));
        app.on_key(key(KeyCode::Down)); // "2"
        app.on_key(key(KeyCode::Char('d')));
        assert_eq!(msgs(&app), [("a", "3"), ("a", "1")]);
        assert_eq!(app.selected(), 1);
        app.on_key(key(KeyCode::Char('d'))); // last row: selection re-clamps
        assert_eq!(msgs(&app), [("a", "3")]);
        assert_eq!(app.selected(), 0);

        app.push_notification("a".into(), rec("4", None, None));
        app.on_key(key(KeyCode::Char('D')));
        assert!(app.inbox.entries.is_empty());
        assert_eq!(app.selected(), 0);
        // Empty inbox: `d` is a no-op, not a panic.
        app.on_key(key(KeyCode::Char('d')));
    }

    #[test]
    fn d_off_the_inbox_does_not_dismiss() {
        let mut app = new_app();
        app.push_notification("a".into(), rec("1", None, None));
        app.on_key(key(KeyCode::Char('d')));
        app.on_key(key(KeyCode::Char('D')));
        assert_eq!(app.inbox.entries.len(), 1);
    }

    #[test]
    fn selection_follows_entry_on_push() {
        let mut app = new_app();
        app.push_notification("a".into(), rec("1", None, None));
        app.push_notification("a".into(), rec("2", Some("k"), None));
        app.on_key(key(KeyCode::Char('4')));
        app.on_key(key(KeyCode::Down)); // "1"
        app.push_notification("a".into(), rec("3", None, None));
        assert_eq!(app.selected_inbox_entry().unwrap().record.msg, "1");
        // A dedupe removing a row above the cursor keeps it on "1" too.
        app.push_notification("a".into(), rec("2b", Some("k"), None));
        assert_eq!(app.selected_inbox_entry().unwrap().record.msg, "1");
        // A cursor on the top row stays on top.
        while app.selected() > 0 {
            app.on_key(key(KeyCode::Up));
        }
        app.push_notification("a".into(), rec("4", None, None));
        assert_eq!(app.selected(), 0);
        assert_eq!(app.selected_inbox_entry().unwrap().record.msg, "4");
    }

    #[test]
    fn enter_opens_link_only_when_present() {
        let mut app = new_app();
        app.push_notification("a".into(), rec("no link", None, None));
        app.push_notification("a".into(), rec("pr", None, Some("https://x/pr/1")));
        app.on_key(key(KeyCode::Char('4')));
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.take_pending_open().as_deref(), Some("https://x/pr/1"));
        assert!(matches!(app.modal, super::super::Modal::None));

        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.take_pending_open(), None);
    }

    #[test]
    fn enter_refuses_non_url_links() {
        let mut app = new_app();
        app.push_notification("a".into(), rec("x", None, Some("--help")));
        app.on_key(key(KeyCode::Char('4')));
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.take_pending_open(), None);
        assert!(app.status.as_deref().unwrap().starts_with("not a URL"));
    }

    #[test]
    fn url_check() {
        assert!(is_url("https://github.com/o/r/pull/1"));
        assert!(is_url("HTTP://example.com"));
        assert!(!is_url("vscode://file/x"));
        assert!(!is_url("file:///etc/passwd"));
        assert!(!is_url("https://"));
        assert!(!is_url("-x"));
        assert!(!is_url("/etc/passwd"));
        assert!(!is_url("1http://x"));
        assert!(!is_url("http:"));
        assert!(!is_url("-a:b"));
    }

    #[test]
    fn clock_and_offset() {
        assert_eq!(clock(0, 0), "00:00");
        assert_eq!(clock(13 * 3600 + 7 * 60 + 59, 0), "13:07");
        assert_eq!(clock(23 * 3600, 2 * 3600), "01:00");
        assert_eq!(clock(3600, -2 * 3600), "23:00");
        assert_eq!(parse_utc_offset("+0200\n"), Some(7200));
        assert_eq!(parse_utc_offset("-0530"), Some(-(5 * 3600 + 30 * 60)));
        assert_eq!(parse_utc_offset("CEST"), None);
        assert_eq!(parse_utc_offset("+02"), None);
        assert_eq!(parse_utc_offset(""), None);
    }
}
