# 0032: Dashboard logic is an I/O-free state machine; the `:` grammar is one table

Status: accepted

## Context

A TUI mixes input handling, rendering and side effects. When they're
interleaved, behaviour can only be tested by driving a real terminal, and
duplicated knowledge drifts: the prompt's parser and its tab completer once
disagreed (`rebuild --force` parsed but never completed).

## Decision

- `src/tui/app/` is a deliberately I/O-free state machine: key and mouse
  dispatch mutate `App` and leave effects as data (queued actions, a
  pending clipboard string, daemon commands) that the event loop in
  `tui/mod.rs` performs. Pure helpers (`select.rs`, `procs.rs`,
  `remote.rs`) follow the same rule. Two accepted exceptions run inline:
  spawning a terminal tab's PTY and reading/appending prompt history.
- The `:` command line's grammar is one declarative table
  (`tui/spec.rs`, `SPECS`); both parsing and tab completion derive from it,
  so a new command or flag is one table entry.

## Consequences

- The dashboard's behaviour is unit-tested without a terminal, daemon or
  runtime (`app/test_support.rs`).
- Code that needs I/O from a key handler must be expressed as an effect
  the loop runs, not called inline.

Source: docs/tui.md; docs/tui-prompt-spec.md; docs/tui-selection.md, "Design" (App stays I/O-free).
