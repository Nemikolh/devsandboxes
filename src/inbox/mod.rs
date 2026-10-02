//! The Inbox model: container notifications (`devsbd notify`,
//! docs/automations.md) as threads, newest first. This is the persisted half,
//! out of `src/tui/` because the bridge writes records here while any number
//! of dashboards read them (see [`store`]); the TUI keeps only view state
//! (selection, folds).
//!
//! Records sharing an `(owner, key)` form a thread: the newest is the head,
//! older ones are its history. `owner` is the sending instance's
//! `instance_id`, not its name, so a rename or rebuild keeps the thread
//! (`owner_name` is only what to show).
//!
//! Everything here is plain state: threading, cap, unread and the dismiss
//! operations stay unit-testable without a store.

pub mod store;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::devsbd::notify::{Level, Record};

/// Records kept per owner, thread history included; the owner's oldest record
/// drops off beyond this. Per owner only, so a noisy container never evicts
/// another's.
pub const INBOX_INSTANCE_CAP: usize = 200;

/// `inbox.toml` schema version written by this build. v1 (no `version` key)
/// identified threads by instance *name*; see [`Inbox::from_toml`].
pub const VERSION: u32 = 2;

/// One received record. `id` grows with arrival order across the whole inbox
/// (persisted), so "oldest" is well defined across threads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    pub id: u64,
    pub record: Record,
}

/// A keyed record's history, or a single unkeyed record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Thread {
    /// Stable id, persisted: selection and fold state follow it across the
    /// reloads every dashboard does when the store changes.
    pub id: u64,
    /// `instance_id` of the sender (stable across stop/restart/rebuild).
    pub owner: String,
    /// The owner's state key when the newest record arrived. Display only;
    /// never an identity (instances get renamed).
    pub owner_name: String,
    /// Threading key, shared by every note (unkeyed threads hold one note).
    pub key: Option<String>,
    /// Newest first, never empty: `notes[0]` is the head.
    pub notes: Vec<Note>,
    /// Only the head counts as unread; history is a trace, not news.
    pub unread: bool,
}

impl Thread {
    pub fn head(&self) -> &Record {
        &self.notes[0].record
    }

    /// Older records under the head.
    pub fn history(&self) -> usize {
        self.notes.len() - 1
    }
}

/// A mutation a dashboard asks the store to apply. Row indices can't cross the
/// process boundary (another dashboard may have changed the list), so every
/// variant names what it touches by a stable id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    MarkAllRead,
    /// Dismiss a whole thread (its history with it).
    RemoveThread(u64),
    /// Dismiss one record, by its note `seq`.
    RemoveNote(u64),
    /// Dismiss everything from one owner.
    RemoveOwner(String),
    /// Dismiss everything.
    Clear,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Inbox {
    /// Ordered by head, newest first.
    pub threads: Vec<Thread>,
    next_id: u64,
}

impl Inbox {
    fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Insert at the top. A keyed record joins the thread with the same
    /// `(owner, key)` as its new head, moving it to the top with `unread`.
    /// Keys are per owner, since two sandboxes can't know each other's.
    pub fn push(&mut self, owner: String, owner_name: String, record: Record, unread: bool) {
        let note = Note { id: self.next_id(), record };
        let key = note.record.key.clone();
        let existing = key.as_ref().and_then(|key| {
            self.threads
                .iter()
                .position(|t| t.owner == owner && t.key.as_ref() == Some(key))
        });
        let thread = match existing {
            Some(pos) => {
                let mut t = self.threads.remove(pos);
                t.notes.insert(0, note);
                t.unread = unread;
                // A rename shows up on the next record; the thread is kept.
                t.owner_name = owner_name;
                t
            }
            None => {
                let id = self.next_id();
                Thread { id, owner: owner.clone(), owner_name, key, notes: vec![note], unread }
            }
        };
        self.threads.insert(0, thread);
        self.enforce_cap(&owner);
    }

    /// Apply one dashboard-requested [`Op`].
    pub fn apply(&mut self, op: &Op) {
        match op {
            Op::MarkAllRead => self.mark_all_read(),
            Op::RemoveThread(id) => self.threads.retain(|t| t.id != *id),
            Op::RemoveNote(seq) => self.remove_note(*seq),
            Op::RemoveOwner(owner) => self.threads.retain(|t| &t.owner != owner),
            Op::Clear => self.threads.clear(),
        }
    }

    /// Drop the note with arrival id `seq`, and its thread when it was the
    /// last one.
    fn remove_note(&mut self, seq: u64) {
        for i in 0..self.threads.len() {
            if let Some(pos) = self.threads[i].notes.iter().position(|n| n.id == seq) {
                self.threads[i].notes.remove(pos);
                if self.threads[i].notes.is_empty() {
                    self.threads.remove(i);
                }
                return;
            }
        }
    }

    /// Drop `owner`'s oldest records until it is within the cap. The oldest is
    /// always some thread's last note (a head is newer than its history).
    fn enforce_cap(&mut self, owner: &str) {
        loop {
            let mine = self.threads.iter().filter(|t| t.owner == owner);
            if mine.clone().map(|t| t.notes.len()).sum::<usize>() <= INBOX_INSTANCE_CAP {
                return;
            }
            let Some(pos) = self
                .threads
                .iter()
                .enumerate()
                .filter(|(_, t)| t.owner == owner)
                .min_by_key(|(_, t)| t.notes.last().map_or(u64::MAX, |n| n.id))
                .map(|(i, _)| i)
            else {
                return;
            };
            self.threads[pos].notes.pop();
            if self.threads[pos].notes.is_empty() {
                self.threads.remove(pos);
            }
        }
    }

    pub fn unread(&self) -> usize {
        self.threads.iter().filter(|t| t.unread).count()
    }

    /// Unread threads from `owner` (an `instance_id`), for the group header.
    pub fn unread_for(&self, owner: &str) -> usize {
        self.threads.iter().filter(|t| t.unread && t.owner == owner).count()
    }

    /// Unread threads from the instance currently *named* `name`, for the
    /// Instances-row badge: a snapshot row knows the name, not the id.
    pub fn unread_for_name(&self, name: &str) -> usize {
        self.threads.iter().filter(|t| t.unread && t.owner_name == name).count()
    }

    /// Records (history included) from `owner`.
    pub fn count_for(&self, owner: &str) -> usize {
        self.threads
            .iter()
            .filter(|t| t.owner == owner)
            .map(|t| t.notes.len())
            .sum()
    }

    /// Name to show for `owner`, from its newest thread; the id itself when
    /// the owner has no threads left.
    pub fn owner_name<'a>(&'a self, owner: &'a str) -> &'a str {
        self.threads
            .iter()
            .find(|t| t.owner == owner)
            .map_or(owner, |t| t.owner_name.as_str())
    }

    fn mark_all_read(&mut self) {
        for t in &mut self.threads {
            t.unread = false;
        }
    }

    /// The `inbox.toml` contents (always v2).
    pub fn to_toml(&self) -> Result<String, String> {
        let saved = SavedInbox {
            version: VERSION,
            threads: self
                .threads
                .iter()
                .map(|t| SavedThread {
                    id: t.id,
                    owner: t.owner.clone(),
                    owner_name: t.owner_name.clone(),
                    key: t.key.clone(),
                    instance: None,
                    unread: t.unread,
                    notes: t
                        .notes
                        .iter()
                        .map(|n| SavedNote {
                            seq: n.id,
                            level: n.record.level.as_str().to_string(),
                            at: n.record.at,
                            key: None,
                            link: n.record.link.clone(),
                            msg: n.record.msg.clone(),
                        })
                        .collect(),
                })
                .collect(),
        };
        toml::to_string_pretty(&saved).map_err(|e| e.to_string())
    }

    /// Parse `inbox.toml` contents. `names` maps instance names (state keys)
    /// to `instance_id`s, and is only used by a v1 file, which identified
    /// threads by name: a name that no longer resolves keeps its thread under
    /// the placeholder owner `name:<instance>` (step 3 archives those).
    pub fn from_toml(text: &str, names: &BTreeMap<String, String>) -> Result<Inbox, String> {
        let saved: SavedInbox = toml::from_str(text).map_err(|e| e.to_string())?;
        let mut inbox = Inbox::default();
        for t in saved.threads {
            // v1 repeated the key on every note; it belongs to the thread.
            let v1_key = t.notes.first().and_then(|n| n.key.clone());
            let notes = t
                .notes
                .into_iter()
                .map(|n| {
                    let level = Level::parse(&n.level).ok_or_else(|| format!("bad level `{}`", n.level))?;
                    Ok(Note {
                        id: n.seq,
                        record: Record { level, key: None, link: n.link, msg: n.msg, at: n.at },
                    })
                })
                .collect::<Result<Vec<_>, String>>()?;
            if notes.is_empty() {
                continue;
            }
            // v1: `instance` is a name; v2 carries the id and the key.
            let (owner, owner_name, key) = match t.instance {
                Some(name) => {
                    let owner = names.get(&name).cloned().unwrap_or_else(|| format!("name:{name}"));
                    (owner, name, v1_key)
                }
                None => (t.owner, t.owner_name, t.key),
            };
            let id = if saved.version >= VERSION { t.id } else { inbox.next_id() };
            let notes = notes
                .into_iter()
                .map(|n| Note { record: Record { key: key.clone(), ..n.record }, ..n })
                .collect();
            inbox.threads.push(Thread { id, owner, owner_name, key, notes, unread: t.unread });
        }
        // Ids keep growing past everything loaded, so arrival order (which the
        // cap reads) and thread identity stay unique.
        inbox.next_id = inbox
            .threads
            .iter()
            .flat_map(|t| std::iter::once(t.id).chain(t.notes.iter().map(|n| n.id)))
            .max()
            .map_or(0, |m| m + 1);
        Ok(inbox)
    }
}

/// `inbox.toml` schema: threads newest first, each with its notes newest
/// first. One struct serves both versions, since a v1 file is just a v2 one
/// with `instance` instead of `owner`/`owner_name` and the key on the notes.
#[derive(Serialize, Deserialize)]
struct SavedInbox {
    /// Absent in v1 files, which this build migrates on load.
    #[serde(default)]
    version: u32,
    #[serde(default, rename = "thread", skip_serializing_if = "Vec::is_empty")]
    threads: Vec<SavedThread>,
}

#[derive(Serialize, Deserialize)]
struct SavedThread {
    #[serde(default)]
    id: u64,
    #[serde(default)]
    owner: String,
    #[serde(default)]
    owner_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key: Option<String>,
    /// v1 only: the owner's state key. Never written back.
    #[serde(default, skip_serializing)]
    instance: Option<String>,
    #[serde(default)]
    unread: bool,
    #[serde(default, rename = "note")]
    notes: Vec<SavedNote>,
}

#[derive(Serialize, Deserialize)]
struct SavedNote {
    /// Arrival order across the inbox (the cap drops the lowest first).
    seq: u64,
    level: String,
    at: u64,
    /// v1 only on read: the key now lives on the thread.
    #[serde(default, skip_serializing)]
    key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    link: Option<String>,
    msg: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(msg: &str, key: Option<&str>, link: Option<&str>) -> Record {
        Record {
            level: Level::Info,
            key: key.map(str::to_string),
            link: link.map(str::to_string),
            msg: msg.into(),
            at: 0,
        }
    }

    /// Push from owner `id`, whose name is the id without its `-id` suffix.
    fn push(inbox: &mut Inbox, owner: &str, record: Record, unread: bool) {
        inbox.push(format!("{owner}-id"), owner.into(), record, unread);
    }

    /// Thread heads, newest first.
    fn msgs(inbox: &Inbox) -> Vec<(&str, &str)> {
        inbox
            .threads
            .iter()
            .map(|t| (t.owner_name.as_str(), t.head().msg.as_str()))
            .collect()
    }

    fn names(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(n, id)| (n.to_string(), id.to_string())).collect()
    }

    #[test]
    fn push_is_newest_first() {
        let mut inbox = Inbox::default();
        push(&mut inbox, "a", rec("one", None, None), true);
        push(&mut inbox, "b", rec("two", None, None), true);
        assert_eq!(msgs(&inbox), [("b", "two"), ("a", "one")]);
    }

    #[test]
    fn same_key_threads_per_owner() {
        let mut inbox = Inbox::default();
        push(&mut inbox, "a", rec("pr 1 v1", Some("pr-1"), None), false);
        push(&mut inbox, "a", rec("other", None, None), false);
        push(&mut inbox, "b", rec("b pr 1", Some("pr-1"), None), false);
        push(&mut inbox, "a", rec("pr 1 v2", Some("pr-1"), None), true);
        // Same (owner, key) becomes the head of one thread, moved to the top;
        // same key from another owner is its own thread; unkeyed records never
        // thread.
        assert_eq!(msgs(&inbox), [("a", "pr 1 v2"), ("b", "b pr 1"), ("a", "other")]);
        let t = &inbox.threads[0];
        assert_eq!(t.notes.iter().map(|n| n.record.msg.as_str()).collect::<Vec<_>>(), ["pr 1 v2", "pr 1 v1"]);
        assert!(t.unread);
        assert_eq!(inbox.unread(), 1);
    }

    #[test]
    fn a_renamed_owner_keeps_its_threads() {
        let mut inbox = Inbox::default();
        inbox.push("id-1".into(), "web".into(), rec("v1", Some("k"), None), true);
        inbox.push("id-1".into(), "web-2".into(), rec("v2", Some("k"), None), true);
        assert_eq!(inbox.threads.len(), 1);
        assert_eq!(inbox.threads[0].owner_name, "web-2");
        assert_eq!(inbox.unread_for("id-1"), 1);
        assert_eq!(inbox.unread_for_name("web-2"), 1);
        assert_eq!(inbox.unread_for_name("web"), 0);
    }

    #[test]
    fn cap_counts_history_and_drops_that_owners_oldest() {
        let mut inbox = Inbox::default();
        push(&mut inbox, "quiet", rec("q0", None, None), true);
        push(&mut inbox, "noisy", rec("k0", Some("k"), None), true);
        for i in 1..INBOX_INSTANCE_CAP {
            push(&mut inbox, "noisy", rec(&i.to_string(), None, None), true);
        }
        assert_eq!(inbox.count_for("noisy-id"), INBOX_INSTANCE_CAP);
        // A new record in the keyed thread: noisy's oldest record is the
        // thread's own first one ("k0"), dropped from its history.
        push(&mut inbox, "noisy", rec("k1", Some("k"), None), true);
        assert_eq!(inbox.count_for("noisy-id"), INBOX_INSTANCE_CAP);
        assert_eq!(
            inbox.threads[0].notes.iter().map(|n| n.record.msg.as_str()).collect::<Vec<_>>(),
            ["k1"]
        );
        // Next: the oldest unkeyed one ("1") goes, its thread with it.
        push(&mut inbox, "noisy", rec("new", None, None), true);
        assert_eq!(inbox.count_for("noisy-id"), INBOX_INSTANCE_CAP);
        assert!(!msgs(&inbox).contains(&("noisy", "1")));
        assert!(msgs(&inbox).contains(&("noisy", "2")));
        assert_eq!(inbox.count_for("quiet-id"), 1);
    }

    #[test]
    fn ops_remove_by_identity() {
        let mut inbox = Inbox::default();
        push(&mut inbox, "a", rec("v1", Some("k"), None), true);
        push(&mut inbox, "a", rec("v2", Some("k"), None), true);
        push(&mut inbox, "a", rec("solo", None, None), true);
        push(&mut inbox, "b", rec("b1", None, None), true);

        let thread = inbox.threads.iter().find(|t| t.key.is_some()).unwrap();
        let history = thread.notes[1].id;
        let (thread_id, solo_id) = (thread.id, inbox.threads[1].id);
        inbox.apply(&Op::RemoveNote(history));
        assert_eq!(msgs(&inbox), [("b", "b1"), ("a", "solo"), ("a", "v2")]);
        inbox.apply(&Op::RemoveThread(thread_id));
        assert_eq!(msgs(&inbox), [("b", "b1"), ("a", "solo")]);
        // Removing a thread's last note drops the thread.
        inbox.apply(&Op::RemoveNote(inbox.threads[1].notes[0].id));
        assert_eq!(msgs(&inbox), [("b", "b1")]);
        // Unknown ids are no-ops, not panics.
        inbox.apply(&Op::RemoveNote(solo_id));
        inbox.apply(&Op::RemoveThread(thread_id));
        assert_eq!(msgs(&inbox), [("b", "b1")]);

        push(&mut inbox, "a", rec("again", None, None), true);
        assert_eq!(inbox.unread(), 2);
        inbox.apply(&Op::MarkAllRead);
        assert_eq!(inbox.unread(), 0);
        inbox.apply(&Op::RemoveOwner("a-id".into()));
        assert_eq!(msgs(&inbox), [("b", "b1")]);
        inbox.apply(&Op::Clear);
        assert!(inbox.threads.is_empty());
    }

    #[test]
    fn toml_roundtrip() {
        let mut inbox = Inbox::default();
        push(&mut inbox, "a", rec("v1", Some("k"), Some("https://x/1")), true);
        push(&mut inbox, "b", rec("multi\nline \"q\"", None, None), true);
        push(&mut inbox, "a", Record { level: Level::Warn, at: 7, ..rec("v2", Some("k"), None) }, true);
        push(&mut inbox, "b", rec("read", None, None), false);

        let text = inbox.to_toml().unwrap();
        assert!(text.contains("version = 2"), "{text}");
        let loaded = Inbox::from_toml(&text, &BTreeMap::new()).unwrap();
        // Thread ids survive, so selection and folds follow a reload.
        assert_eq!(loaded, inbox);

        // Ids keep growing past the saved ones, so the cap's order holds.
        let mut loaded = loaded;
        let max_saved = inbox.threads.iter().flat_map(|t| &t.notes).map(|n| n.id).max().unwrap();
        push(&mut loaded, "a", rec("later", None, None), true);
        assert!(loaded.threads[0].notes[0].id > max_saved);

        assert!(Inbox::from_toml("", &BTreeMap::new()).unwrap().threads.is_empty());
        assert!(Inbox::from_toml(
            "version = 2\n[[thread]]\nowner = \"a\"\n[[thread.note]]\nseq = 0\nlevel = \"loud\"\nat = 0\nmsg = \"x\"\n",
            &BTreeMap::new()
        )
        .is_err());
    }

    /// A v1 file (no `version`, threads keyed by instance name, the key on the
    /// notes) loads with owners resolved through the state's names.
    #[test]
    fn migrates_v1() {
        let v1 = "\
[[thread]]
instance = \"web\"
unread = true
[[thread.note]]
seq = 5
level = \"warn\"
at = 7
key = \"pr-1\"
link = \"https://x/1\"
msg = \"v2\"
[[thread.note]]
seq = 2
level = \"info\"
at = 1
key = \"pr-1\"
msg = \"v1\"

[[thread]]
instance = \"gone\"
[[thread.note]]
seq = 1
level = \"info\"
at = 0
msg = \"orphan\"
";
        let mut inbox = Inbox::from_toml(v1, &names(&[("web", "web-abc1")])).unwrap();
        assert_eq!(inbox.threads.len(), 2);
        let web = &inbox.threads[0];
        assert_eq!((web.owner.as_str(), web.owner_name.as_str()), ("web-abc1", "web"));
        assert_eq!(web.key.as_deref(), Some("pr-1"));
        assert_eq!(web.notes.iter().map(|n| n.id).collect::<Vec<_>>(), [5, 2]);
        assert_eq!(web.head().key.as_deref(), Some("pr-1"), "the key stays on every record");
        assert_eq!(web.head().level, Level::Warn);
        assert!(web.unread);
        // An unresolved name keeps its thread under a placeholder owner.
        let gone = &inbox.threads[1];
        assert_eq!((gone.owner.as_str(), gone.owner_name.as_str()), ("name:gone", "gone"));
        assert!(!gone.unread);
        // Fresh thread ids, and seqs keep growing past the saved notes.
        assert_ne!(web.id, gone.id);
        push(&mut inbox, "web", rec("new", None, None), true);
        assert!(inbox.threads[0].notes[0].id > 5);
        // Re-reading what v1 became is a plain v2 load.
        let text = inbox.to_toml().unwrap();
        assert_eq!(Inbox::from_toml(&text, &BTreeMap::new()).unwrap(), inbox);
    }
}
