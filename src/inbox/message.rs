//! The `devsbd thread send` body (docs/inbox-redesign.md, *Messages*): the
//! schema the host checks. The helper only checks JSON syntax and pulls out
//! `thread` and `id` (`devsbd/src/json.rs`), so everything here runs on the
//! host, like the put schema in `thread.rs`.
//!
//! The wire types are separate from the store's [`Block`]: they deny unknown
//! fields and unknown block types, so a newer owner (forms, a new block) on an
//! older host fails loudly with an `error` record instead of rendering half a
//! message, while the store type stays free to grow.

use serde::Deserialize;

use super::feed::{Block, Field, HEADER_MESSAGE};
use super::thread::{check_key, check_text};
use crate::devsbd::control::{valid_message_id, MAX_MESSAGE_ID};
use crate::devsbd::notify::MAX_SEND_BODY;

/// Blocks per message.
pub const MAX_BLOCKS: usize = 8;
/// A markdown block's text.
const MAX_MARKDOWN: usize = 16 * 1024;
/// Rows per fields block.
const MAX_FIELDS: usize = 20;
const MAX_FIELD_LABEL: usize = 60;
const MAX_FIELD_VALUE: usize = 200;

/// One checked `thread send`: message `id` in thread `thread`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageSend {
    pub thread: String,
    pub id: String,
    pub blocks: Vec<Block>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    thread: String,
    id: String,
    blocks: Vec<WireBlock>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
enum WireBlock {
    Markdown { text: String },
    Fields { items: Vec<WireField> },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireField {
    label: String,
    value: String,
}

/// Parse and check one send body. `Err` is a single line naming what to fix:
/// it is shown to the sender as an `error` record.
pub fn parse(body: &str) -> Result<MessageSend, String> {
    if body.len() > MAX_SEND_BODY {
        return Err(format!("message longer than {MAX_SEND_BODY} bytes"));
    }
    let wire: Wire = serde_json::from_str(body).map_err(|e| e.to_string())?;
    check_key("thread", &wire.thread)?;
    if !valid_message_id(&wire.id) {
        return Err(format!("bad `id` `{}`: lowercase letters, digits and `-`, 1-{MAX_MESSAGE_ID} chars", wire.id));
    }
    // v2 put compat, removed in step 13b: a v2 put's `message` lives under
    // this id, and the put would withdraw or overwrite a sent one.
    if wire.id == HEADER_MESSAGE {
        return Err(format!("`id` `{HEADER_MESSAGE}` is reserved"));
    }
    if wire.blocks.is_empty() {
        return Err("no blocks".into());
    }
    if wire.blocks.len() > MAX_BLOCKS {
        return Err(format!("{} blocks, at most {MAX_BLOCKS} allowed", wire.blocks.len()));
    }
    let blocks = wire
        .blocks
        .into_iter()
        .enumerate()
        .map(|(n, b)| block(n + 1, b))
        .collect::<Result<_, _>>()?;
    Ok(MessageSend { thread: wire.thread, id: wire.id, blocks })
}

/// Check block number `n` (1-based, for the error) and convert it.
fn block(n: usize, b: WireBlock) -> Result<Block, String> {
    match b {
        WireBlock::Markdown { text } => {
            check_text(&format!("blocks[{n}].text"), &text, MAX_MARKDOWN)?;
            if text.trim().is_empty() {
                return Err(format!("block {n}: empty markdown"));
            }
            Ok(Block::Markdown { text })
        }
        WireBlock::Fields { items } => {
            if items.is_empty() {
                return Err(format!("block {n}: fields block has no items"));
            }
            if items.len() > MAX_FIELDS {
                return Err(format!("block {n}: {} items, at most {MAX_FIELDS} allowed", items.len()));
            }
            let items = items
                .into_iter()
                .map(|f| {
                    check_text(&format!("blocks[{n}] label"), &f.label, MAX_FIELD_LABEL)?;
                    check_text(&format!("blocks[{n}] `{}` value", f.label), &f.value, MAX_FIELD_VALUE)?;
                    // One row each: the pane aligns them as `label  value`.
                    if f.label.contains(['\n', '\r']) || f.value.contains(['\n', '\r']) {
                        return Err(format!("block {n}: field `{}` spans lines", f.label.lines().next().unwrap_or("")));
                    }
                    if f.label.trim().is_empty() {
                        return Err(format!("block {n}: a field has an empty label"));
                    }
                    Ok(Field { label: f.label, value: f.value })
                })
                .collect::<Result<_, String>>()?;
            Ok(Block::Fields { items })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r##"{
        "thread": "pr-6900",
        "id": "run-1791277117",
        "blocks": [
            { "type": "markdown", "text": "Addressed 3 of 4 comments, 1 needs your call." },
            { "type": "fields", "items": [{ "label": "Head", "value": "36b13d41" }, { "label": "CI", "value": "green" }] }
        ]
    }"##;

    fn with_blocks(blocks: &str) -> String {
        format!(r#"{{"thread":"t","id":"m","blocks":[{blocks}]}}"#)
    }

    #[test]
    fn parses_the_documented_body() {
        let send = parse(EXAMPLE).unwrap();
        assert_eq!((send.thread.as_str(), send.id.as_str()), ("pr-6900", "run-1791277117"));
        assert_eq!(
            send.blocks,
            [
                Block::Markdown { text: "Addressed 3 of 4 comments, 1 needs your call.".into() },
                Block::Fields {
                    items: vec![
                        Field { label: "Head".into(), value: "36b13d41".into() },
                        Field { label: "CI".into(), value: "green".into() },
                    ]
                },
            ]
        );
        // An empty value is a fine row.
        assert!(parse(&with_blocks(r#"{"type":"fields","items":[{"label":"Base","value":""}]}"#)).is_ok());
    }

    #[test]
    fn rejects_unknown_fields_and_block_types() {
        // A newer owner on this host: forms aren't known here yet.
        let form = parse(&with_blocks(r#"{"type":"form","id":"f","questions":[]}"#)).unwrap_err();
        assert!(form.contains("unknown variant `form`"), "{form}");
        for bad in [
            r#"{"thread":"t","id":"m","blocks":[{"type":"markdown","text":"x"}],"extra":1}"#.to_string(),
            with_blocks(r#"{"type":"markdown","text":"x","style":"bold"}"#),
            with_blocks(r#"{"type":"fields","items":[{"label":"a","value":"b","link":"x"}]}"#),
            with_blocks(r#"{"text":"no type"}"#),
            r#"{"thread":"t","id":"m"}"#.to_string(),
            r#"{"thread":"t","blocks":[]}"#.to_string(),
        ] {
            assert!(parse(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn rejects_every_invalid_field() {
        let err = |body: &str| parse(body).unwrap_err();
        assert!(err(r#"{"thread":"PR","id":"m","blocks":[]}"#).starts_with("bad `thread` `PR`"));
        assert!(err(r#"{"thread":"t","id":"M","blocks":[]}"#).starts_with("bad `id` `M`"));
        assert!(err(&format!(r#"{{"thread":"t","id":"{}","blocks":[]}}"#, "x".repeat(61))).starts_with("bad `id`"));
        assert_eq!(err(r#"{"thread":"t","id":"header-message","blocks":[]}"#), "`id` `header-message` is reserved");
        assert_eq!(err(&with_blocks("")), "no blocks");
        let nine = vec![r#"{"type":"markdown","text":"x"}"#; 9].join(",");
        assert_eq!(err(&with_blocks(&nine)), "9 blocks, at most 8 allowed");
        let eight = vec![r#"{"type":"markdown","text":"x"}"#; 8].join(",");
        assert!(parse(&with_blocks(&eight)).is_ok());
        // Markdown.
        assert_eq!(err(&with_blocks(r#"{"type":"markdown","text":"  "}"#)), "block 1: empty markdown");
        let long = format!(r#"{{"type":"markdown","text":"{}"}}"#, "x".repeat(MAX_MARKDOWN + 1));
        assert!(err(&with_blocks(&long)).contains("longer than 16384 bytes"));
        let fits = format!(r#"{{"type":"markdown","text":"{}"}}"#, "x".repeat(MAX_MARKDOWN));
        assert!(parse(&with_blocks(&fits)).is_ok());
        assert!(err(&with_blocks(r#"{"type":"markdown","text":"a\u001bb"}"#)).contains("control character"));
        // Fields.
        let field = |label: &str, value: &str| format!(r#"{{"label":"{label}","value":"{value}"}}"#);
        let fields = |items: &[String]| with_blocks(&format!(r#"{{"type":"fields","items":[{}]}}"#, items.join(",")));
        assert_eq!(err(&fields(&[])), "block 1: fields block has no items");
        let many: Vec<_> = (0..21).map(|i| field(&format!("l{i}"), "v")).collect();
        assert_eq!(err(&fields(&many)), "block 1: 21 items, at most 20 allowed");
        assert!(parse(&fields(&many[..20])).is_ok());
        assert!(err(&fields(&[field(&"l".repeat(61), "v")])).contains("longer than 60 bytes"));
        assert!(err(&fields(&[field("l", &"v".repeat(201))])).contains("longer than 200 bytes"));
        assert!(parse(&fields(&[field(&"l".repeat(60), &"v".repeat(200))])).is_ok());
        assert_eq!(err(&fields(&[field(" ", "v")])), "block 1: a field has an empty label");
        assert!(err(&fields(&[field("a", "b\\nc")])).contains("spans lines"));
        // The whole body's budget.
        let pad = "x".repeat(MAX_SEND_BODY);
        assert!(err(&format!(r#"{{"thread":"t","id":"m","blocks":[],"p":"{pad}"}}"#)).contains("longer than 49152"));
    }
}
