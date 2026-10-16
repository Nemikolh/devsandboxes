# 0022: The helper has no dependencies; shared codecs are line-based and shared via `#[path]`

Status: accepted

## Context

`devsbd` is copied into every container, so its size is the product. Host
and helper must agree on several formats (frames, control requests,
notify records, the boot file). serde and a shared crate would be the
obvious way, at a large size cost.

## Decision

- The `devsbd` crate has an empty `[dependencies]` table: std only, built
  with a size profile (`opt-level = "z"`, LTO, `panic = "abort"`, strip).
- Formats shared with the host are plain Rust files included on both sides
  with `#[path]` (`src/devsbd/proto.rs`, `bootfile.rs`, `notify.rs`,
  `control.rs`, `escape.rs`) instead of a shared crate. The non-frame
  codecs are line-based with one escaping scheme (`escape.rs`).
- JSON the helper relays (Inbox thread puts/sends) is only syntax-checked in
  the helper (`devsbd/src/json.rs`, std-only) and carried opaque; the host
  checks the schema with serde and turns a reject into an `error` record
  the author sees.

## Consequences

- Adding a field to a shared format means hand-written encode/decode on a
  line codec, with round-trip tests, not a derive.
- Schema errors surface asynchronously (in the Inbox), not as a helper exit
  code; only syntax errors fail at the call (exit 2).

Source: docs/sandbox-helper.md, "Binary: devsbd/ crate"; docs/inbox-threads.md, "Decisions" (Helper: syntax check only).
