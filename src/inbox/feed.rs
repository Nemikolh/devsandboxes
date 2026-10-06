//! A thread's feed (docs/inbox-redesign.md, *Feed*): what happened on an
//! owner thread, in first-insert order. The owner's **messages**, the user's
//! **replies** and **actions**, and host-recorded **markers** (done/reopen by
//! the user, state/status changes by the owner). The header (`thread put`)
//! is not in the feed; a put only leaves a marker when its state or status
//! changes.
//!
//! Submissions (a form's answers) join the feed with forms (step 15); there
//! is no variant for them yet.
//!
//! Every user item records the client that made it (`tui`, `cli`,
//! `api:<name>`) for the user's own audit. It stays in the store: events and
//! the API never carry it, so an owner can't tell clients apart.
//!
//! The rules that shape the feed ([`push`]: status markers collapse; [`cap`]:
//! what goes first past [`MAX_FEED`]) are pure functions over the item list.

use serde::{Deserialize, Serialize};

use super::State;

/// Feed items kept per thread; past it, [`cap`] drops the least valuable.
pub const MAX_FEED: usize = 300;

/// v2 put compat, removed in step 13b: the id of the message a v2 put's
/// `message` field becomes, replaced in place as the field changes.
pub const HEADER_MESSAGE: &str = "header-message";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedItem {
    /// Arrival order across the whole Inbox (like `Note::id`).
    pub seq: u64,
    /// Unix seconds of first insert (a merged status marker: of its last
    /// change).
    pub at: u64,
    pub kind: ItemKind,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ItemKind {
    /// From the owner. `id` is its idempotency key, unique in the thread.
    Message {
        id: String,
        blocks: Vec<Block>,
        /// Replaced in place since it was first inserted.
        #[serde(default)]
        edited: bool,
        /// The owner took it back; kept so the story stays readable.
        #[serde(default)]
        withdrawn: bool,
    },
    /// The user's free text from the composer.
    Reply { text: String, client: String },
    /// The user pressed a header button: its id and its label at the time.
    Action { action: String, label: String, client: String },
    Marker(Marker),
}

/// A message block (docs/inbox-redesign.md, *Messages*); `form` arrives with
/// forms (step 15). Serialized in the same shape `thread send` takes, so
/// `thread ls --feed` hands an owner back what it sent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Block {
    Markdown { text: String },
    /// A compact key/value list.
    Fields { items: Vec<Field> },
}

/// One row of a [`Block::Fields`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Field {
    pub label: String,
    pub value: String,
}

/// A one-line feed item. Done/Reopen are the user's (with the client),
/// State/Status the owner's put changing those fields.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "marker", rename_all = "lowercase")]
pub enum Marker {
    Done { client: String },
    Reopen { client: String },
    State { from: State, to: State },
    Status { from: Option<String>, to: Option<String> },
}

impl FeedItem {
    /// A message of one markdown block (the only block there is yet).
    pub fn markdown(seq: u64, at: u64, id: &str, text: &str) -> FeedItem {
        let blocks = vec![Block::Markdown { text: text.to_string() }];
        FeedItem { seq, at, kind: ItemKind::Message { id: id.to_string(), blocks, edited: false, withdrawn: false } }
    }

    fn is_marker(&self) -> bool {
        matches!(self.kind, ItemKind::Marker(_))
    }

    fn is_message(&self) -> bool {
        matches!(self.kind, ItemKind::Message { .. })
    }
}

/// A message's markdown blocks joined by a blank line (other blocks left
/// out): the header message's text, and a message's one-line summary.
pub fn markdown_of(blocks: &[Block]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            Block::Markdown { text } => Some(text.as_str()),
            Block::Fields { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// What a `thread send` did to the feed ([`send`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sent {
    /// Same id, same blocks, not withdrawn: nothing to do.
    Unchanged,
    /// Same id, replaced in place (`edited`), or a withdrawn one sent again.
    Edited,
    /// A new id, appended.
    Inserted,
}

/// Apply owner message `id` with `blocks` (docs/inbox-redesign.md,
/// *Messages*): the id is the owner's idempotency key, so the same content is
/// a no-op, other content replaces the message in place (its `seq`, `at` and
/// feed position stay: order is first-insert order) and tags it `edited`.
/// A withdrawn message sent again comes back, replaced and `edited` even with
/// the same blocks: it was gone from the reader's view, so it changed. `seq`
/// is only called for an insert, so a no-op mints no id.
pub fn send(feed: &mut Vec<FeedItem>, id: &str, blocks: Vec<Block>, seq: impl FnOnce() -> u64, at: u64) -> Sent {
    let existing = feed.iter_mut().find_map(|i| match &mut i.kind {
        ItemKind::Message { id: have, blocks, edited, withdrawn } if have == id => Some((blocks, edited, withdrawn)),
        _ => None,
    });
    match existing {
        Some((have, _, withdrawn)) if *have == blocks && !*withdrawn => Sent::Unchanged,
        Some((have, edited, withdrawn)) => {
            *have = blocks;
            *edited = true;
            *withdrawn = false;
            Sent::Edited
        }
        None => {
            let kind = ItemKind::Message { id: id.to_string(), blocks, edited: false, withdrawn: false };
            push(feed, FeedItem { seq: seq(), at, kind });
            Sent::Inserted
        }
    }
}

/// Withdraw owner message `id`: tagged `withdrawn` and kept, so the story
/// stays readable. Whether anything changed (an unknown or already withdrawn
/// id is a no-op).
pub fn withdraw(feed: &mut [FeedItem], id: &str) -> bool {
    feed.iter_mut().any(|i| match &mut i.kind {
        ItemKind::Message { id: have, withdrawn, .. } if have == id && !*withdrawn => {
            *withdrawn = true;
            true
        }
        _ => false,
    })
}

/// Append `item`, collapsing status changes: a status marker right after
/// another status marker (nothing in between) merges into it, keeping the
/// earlier `from` and taking the new `to` and `at`; a merge that lands back
/// where it started (`from == to`) leaves no marker at all. State markers
/// never merge: a state change is always news.
pub fn push(feed: &mut Vec<FeedItem>, item: FeedItem) {
    let back_to_start = match (&item.kind, feed.last().map(|l| &l.kind)) {
        (ItemKind::Marker(Marker::Status { to, .. }), Some(ItemKind::Marker(Marker::Status { from, .. }))) => from == to,
        _ => {
            feed.push(item);
            return;
        }
    };
    if back_to_start {
        feed.pop();
        return;
    }
    let (Some(last), ItemKind::Marker(Marker::Status { to, .. })) = (feed.last_mut(), item.kind) else { return };
    if let ItemKind::Marker(Marker::Status { to: prev_to, .. }) = &mut last.kind {
        *prev_to = to;
    }
    last.at = item.at;
}

/// Bring `feed` down to `max` items: the oldest markers go first, then the
/// oldest messages, then the oldest replies and actions (the user's own
/// words outlast the owner's narration, which it can re-send).
pub fn cap(feed: &mut Vec<FeedItem>, max: usize) {
    while feed.len() > max {
        let pos = feed
            .iter()
            .position(FeedItem::is_marker)
            // step 15: skip messages with an open form.
            .or_else(|| feed.iter().position(FeedItem::is_message))
            .unwrap_or(0);
        feed.remove(pos);
    }
}

/// The text of the [`HEADER_MESSAGE`] item, when present and not withdrawn.
/// v2 put compat, removed in step 13b.
pub fn header_message(feed: &[FeedItem]) -> Option<String> {
    feed.iter().find_map(|i| match &i.kind {
        ItemKind::Message { id, blocks, withdrawn: false, .. } if id == HEADER_MESSAGE => Some(markdown_of(blocks)),
        _ => None,
    })
}

/// What a v2 put's `message` does to the feed. v2 put compat, removed in
/// step 13b.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeaderChange {
    Unchanged,
    /// No header message yet: a new item (news).
    Insert(String),
    /// Replace its text in place (or bring a withdrawn one back): `edited`.
    Edit(String),
    /// The put dropped the field: withdraw the item.
    Withdraw,
}

/// Decide [`HeaderChange`] for a put whose `message` is `want` (empty counts
/// as absent). v2 put compat, removed in step 13b.
pub fn header_change(feed: &[FeedItem], want: Option<&str>) -> HeaderChange {
    let want = want.filter(|m| !m.trim().is_empty());
    let current = feed.iter().find_map(|i| match &i.kind {
        ItemKind::Message { id, blocks, withdrawn, .. } if id == HEADER_MESSAGE => Some((markdown_of(blocks), *withdrawn)),
        _ => None,
    });
    match (current, want) {
        (None, None) => HeaderChange::Unchanged,
        (None, Some(text)) => HeaderChange::Insert(text.to_string()),
        (Some((_, true)), None) => HeaderChange::Unchanged,
        (Some(_), None) => HeaderChange::Withdraw,
        (Some((text, false)), Some(want)) if text == want => HeaderChange::Unchanged,
        (Some(_), Some(want)) => HeaderChange::Edit(want.to_string()),
    }
}

/// Apply a [`HeaderChange`]; `seq`/`at` stamp an inserted item. v2 put
/// compat, removed in step 13b.
pub fn apply_header_change(feed: &mut Vec<FeedItem>, change: HeaderChange, seq: u64, at: u64) {
    let item = feed.iter_mut().find_map(|i| match &mut i.kind {
        ItemKind::Message { id, blocks, edited, withdrawn } if id == HEADER_MESSAGE => Some((blocks, edited, withdrawn)),
        _ => None,
    });
    match (change, item) {
        (HeaderChange::Insert(text), _) => push(feed, FeedItem::markdown(seq, at, HEADER_MESSAGE, &text)),
        (HeaderChange::Edit(text), Some((blocks, edited, withdrawn))) => {
            let new = vec![Block::Markdown { text }];
            if *blocks != new {
                *blocks = new;
                *edited = true;
            }
            *withdrawn = false;
        }
        (HeaderChange::Withdraw, Some((_, _, withdrawn))) => *withdrawn = true,
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(seq: u64, from: Option<&str>, to: Option<&str>) -> FeedItem {
        let (from, to) = (from.map(str::to_string), to.map(str::to_string));
        FeedItem { seq, at: seq * 10, kind: ItemKind::Marker(Marker::Status { from, to }) }
    }

    fn state(seq: u64, from: State, to: State) -> FeedItem {
        FeedItem { seq, at: seq * 10, kind: ItemKind::Marker(Marker::State { from, to }) }
    }

    fn reply(seq: u64) -> FeedItem {
        FeedItem { seq, at: seq * 10, kind: ItemKind::Reply { text: format!("r{seq}"), client: "tui".into() } }
    }

    fn statuses(feed: &[FeedItem]) -> Vec<(u64, u64, Option<&str>, Option<&str>)> {
        feed.iter()
            .filter_map(|i| match &i.kind {
                ItemKind::Marker(Marker::Status { from, to }) => Some((i.seq, i.at, from.as_deref(), to.as_deref())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn consecutive_status_markers_merge() {
        let mut feed = Vec::new();
        push(&mut feed, status(1, None, Some("a")));
        push(&mut feed, status(2, Some("a"), Some("b")));
        push(&mut feed, status(3, Some("b"), Some("c")));
        assert_eq!(feed.len(), 1);
        // The earlier `from` and seq, the new `to` and time.
        assert_eq!(statuses(&feed), [(1, 30, None, Some("c"))]);
    }

    #[test]
    fn a_merge_back_to_the_start_leaves_nothing() {
        let mut feed = vec![reply(1)];
        push(&mut feed, status(2, Some("a"), Some("b")));
        push(&mut feed, status(3, Some("b"), Some("a")));
        assert_eq!(feed, [reply(1)]);
        // Also from and to both absent.
        push(&mut feed, status(4, None, Some("x")));
        push(&mut feed, status(5, Some("x"), None));
        assert_eq!(feed, [reply(1)]);
    }

    #[test]
    fn anything_in_between_stops_the_merge() {
        for between in [reply(2), state(2, State::Active, State::NeedsYou), FeedItem::markdown(2, 20, "m", "hi")] {
            let mut feed = Vec::new();
            push(&mut feed, status(1, Some("a"), Some("b")));
            push(&mut feed, between.clone());
            push(&mut feed, status(3, Some("b"), Some("c")));
            assert_eq!(feed.len(), 3, "{between:?}");
            assert_eq!(statuses(&feed), [(1, 10, Some("a"), Some("b")), (3, 30, Some("b"), Some("c"))]);
        }
    }

    #[test]
    fn state_markers_never_merge() {
        let mut feed = Vec::new();
        push(&mut feed, state(1, State::Active, State::NeedsYou));
        push(&mut feed, state(2, State::NeedsYou, State::Active));
        assert_eq!(feed.len(), 2);
        // A status marker after a state marker starts fresh, too.
        push(&mut feed, status(3, None, Some("x")));
        assert_eq!(feed.len(), 3);
    }

    #[test]
    fn cap_drops_markers_then_messages_then_user_items() {
        let mut feed = vec![
            reply(1),
            FeedItem::markdown(2, 20, "m1", "one"),
            state(3, State::Active, State::NeedsYou),
            FeedItem::markdown(4, 40, "m2", "two"),
            status(5, None, Some("s")),
            reply(6),
        ];
        let seqs = |feed: &[FeedItem]| feed.iter().map(|i| i.seq).collect::<Vec<_>>();
        cap(&mut feed, 6);
        assert_eq!(seqs(&feed), [1, 2, 3, 4, 5, 6], "within the cap: untouched");
        cap(&mut feed, 4);
        assert_eq!(seqs(&feed), [1, 2, 4, 6], "markers first, oldest first");
        cap(&mut feed, 3);
        assert_eq!(seqs(&feed), [1, 4, 6], "then the oldest message");
        cap(&mut feed, 1);
        assert_eq!(seqs(&feed), [6], "then the oldest replies");
    }

    #[test]
    fn header_message_inserts_edits_and_withdraws() {
        let mut feed = Vec::new();
        assert_eq!(header_change(&feed, None), HeaderChange::Unchanged);
        assert_eq!(header_change(&feed, Some("  ")), HeaderChange::Unchanged, "empty is absent");
        let change = header_change(&feed, Some("one"));
        assert_eq!(change, HeaderChange::Insert("one".into()));
        apply_header_change(&mut feed, change, 7, 70);
        assert_eq!(feed, [FeedItem::markdown(7, 70, HEADER_MESSAGE, "one")]);
        assert_eq!(header_message(&feed).as_deref(), Some("one"));
        assert_eq!(header_change(&feed, Some("one")), HeaderChange::Unchanged);

        // Replaced in place: same seq and time, `edited`.
        let change = header_change(&feed, Some("two"));
        apply_header_change(&mut feed, change, 8, 80);
        let ItemKind::Message { blocks, edited, withdrawn, .. } = &feed[0].kind else { panic!() };
        assert_eq!((feed.len(), feed[0].seq, feed[0].at), (1, 7, 70));
        assert_eq!((markdown_of(blocks).as_str(), *edited, *withdrawn), ("two", true, false));

        // Dropped: withdrawn, kept; withdrawing again is nothing.
        let change = header_change(&feed, None);
        apply_header_change(&mut feed, change, 9, 90);
        assert!(matches!(feed[0].kind, ItemKind::Message { withdrawn: true, .. }));
        assert_eq!(header_message(&feed), None);
        assert_eq!(header_change(&feed, None), HeaderChange::Unchanged);
        // Back with the same text: un-withdrawn in place.
        assert_eq!(header_change(&feed, Some("two")), HeaderChange::Edit("two".into()));
        apply_header_change(&mut feed, HeaderChange::Edit("two".into()), 10, 100);
        assert_eq!(feed.len(), 1);
        assert_eq!(header_message(&feed).as_deref(), Some("two"));
    }

    fn md(text: &str) -> Vec<Block> {
        vec![Block::Markdown { text: text.into() }]
    }

    fn message(feed: &[FeedItem], id: &str) -> (u64, u64, Vec<Block>, bool, bool) {
        feed.iter()
            .find_map(|i| match &i.kind {
                ItemKind::Message { id: have, blocks, edited, withdrawn } if have == id => {
                    Some((i.seq, i.at, blocks.clone(), *edited, *withdrawn))
                }
                _ => None,
            })
            .unwrap()
    }

    #[test]
    fn send_inserts_edits_in_place_and_ignores_repeats() {
        let mut feed = vec![reply(1)];
        assert_eq!(send(&mut feed, "a", md("one"), || 2, 20), Sent::Inserted);
        assert_eq!(send(&mut feed, "b", md("two"), || 3, 30), Sent::Inserted);
        // Same content: nothing, and no id minted.
        assert_eq!(send(&mut feed, "a", md("one"), || panic!("minted"), 40), Sent::Unchanged);
        // New content: replaced in place, first-insert seq/time/position kept.
        assert_eq!(send(&mut feed, "a", md("uno"), || panic!("minted"), 50), Sent::Edited);
        assert_eq!(message(&feed, "a"), (2, 20, md("uno"), true, false));
        assert_eq!(feed.iter().map(|i| i.seq).collect::<Vec<_>>(), [1, 2, 3]);
        // The edited content again is a no-op too.
        assert_eq!(send(&mut feed, "a", md("uno"), || panic!("minted"), 60), Sent::Unchanged);
    }

    #[test]
    fn withdraw_keeps_the_message_and_a_resend_brings_it_back() {
        let mut feed = Vec::new();
        send(&mut feed, "a", md("one"), || 1, 10);
        assert!(!withdraw(&mut feed, "nope"), "an unknown id is a no-op");
        assert!(withdraw(&mut feed, "a"));
        assert!(!withdraw(&mut feed, "a"), "already withdrawn");
        assert_eq!(message(&feed, "a"), (1, 10, md("one"), false, true));
        // Sent again, same blocks: back, in place, and tagged edited.
        assert_eq!(send(&mut feed, "a", md("one"), || panic!("minted"), 20), Sent::Edited);
        assert_eq!(message(&feed, "a"), (1, 10, md("one"), true, false));
        assert_eq!(feed.len(), 1);
    }

    #[test]
    fn markdown_of_skips_fields() {
        let blocks = vec![
            Block::Markdown { text: "a".into() },
            Block::Fields { items: vec![Field { label: "Head".into(), value: "36b1".into() }] },
            Block::Markdown { text: "b".into() },
        ];
        assert_eq!(markdown_of(&blocks), "a\n\nb");
    }

    #[test]
    fn store_shape_round_trips() {
        let fields = vec![Block::Fields { items: vec![Field { label: "Head".into(), value: "36b1".into() }] }];
        let feed = vec![
            FeedItem { seq: 8, at: 80, kind: ItemKind::Message { id: "f".into(), blocks: fields, edited: true, withdrawn: false } },
            FeedItem::markdown(1, 10, "m", "hi"),
            reply(2),
            FeedItem { seq: 3, at: 30, kind: ItemKind::Action { action: "go".into(), label: "Go".into(), client: "api:x".into() } },
            FeedItem { seq: 4, at: 40, kind: ItemKind::Marker(Marker::Done { client: "cli".into() }) },
            FeedItem { seq: 5, at: 50, kind: ItemKind::Marker(Marker::Reopen { client: "tui".into() }) },
            state(6, State::Done, State::Active),
            status(7, None, Some("s")),
        ];
        let json = serde_json::to_string(&feed).unwrap();
        assert!(json.contains(r#""kind":{"type":"marker","marker":"done","client":"cli"}"#), "{json}");
        assert!(json.contains(r#""blocks":[{"type":"fields","items":[{"label":"Head","value":"36b1"}]}]"#), "{json}");
        assert_eq!(serde_json::from_str::<Vec<FeedItem>>(&json).unwrap(), feed);
    }
}
