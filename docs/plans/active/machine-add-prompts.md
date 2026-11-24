# machine add prompts

`machine add`'s yes/no questions already use the list prompt
(docs/architecture/cli-prompts.md). What's left: its allowance question
is a typed comma-separated list, and its `note:` / `warning:` lines are
unstyled. Off a TTY nothing changes.

## Step 1: a multi-select in `src/ask`

`ask::multi(question, choices, checked) -> Result<Option<Vec<usize>>>`
(`None`: cancelled), on a pure `Multi` state machine modelled on
`src/ask/select.rs` `Select`: one line per choice with `◉` / `◯`, the
cursor as `Select`'s; ↑/↓, `k`/`j` move, Space toggles, Enter confirms
the checked set (empty allowed), Esc / Ctrl-C cancel. Rendered through
the same `interactive` loop (`src/ask/mod.rs`, `Widget`), leaving one
summary line `? question › a, b` (or `none`). Plain fallback: the
choices numbered, one line read: comma/space-separated numbers or
labels, empty keeps `checked`, anything else asks again (unlike
`plain_select`: a typo shouldn't end an add).
Constraints: I/O-free state, unit-tested with `Style::plain()`; no
change to `select` / `review`.
Docs: `docs/architecture/cli-prompts.md` (widget, keys, fallback).

## Step 2: the allowance question

`commands::machine::add`'s `AllowStep::Ask` arm (`lends_line`,
`allow_question`, `parse_allow_answer`, `prompt`) becomes
`ask::multi` over `AllowInfo::flags()` (CLI spelling, `host-network`),
each with a short hint of what it lets placed work do (from
docs/architecture/daemon/fleet.md's allowances), the ones the probe's
`allow` already lends checked. The answer is the set the machine lends:
a checked flag `Some(true)`, an unchecked one that was lent
`Some(false)`, untouched ones `None` (an `AllowPatch`). Binds and env
vars stay flags (`--allow-bind`, `--allow-local-env`), named in the
help line. Cancel stops the add: `not added: nothing changed`
(nothing has been sent yet: `machines.add` comes after).
`parse_allow_answer` and `prompt` go if nothing else uses them.
Landmarks: `src/commands/machine.rs` `add`, `allow_step`,
`allow_question`, `parse_allow_answer`, `set_allowance`,
`allowance_names`; `src/serve/fleet/member.rs` `AllowInfo::flags`,
`AllowPatch`.
Docs: `docs/architecture/daemon/machines.md` (the `add` allowance
paragraph), `docs/architecture/cli-prompts.md` *Callers*.

## Step 3 (proposed, not asked): styled stderr notes

`login::styled` (prefix styling: `warning:` yellow, `note:` cyan)
moves to `src/ask/style.rs` as `Style::lines`, and `machine.rs`'s
`eprintln!("note: …")` / `"warning: …"` sites print through it.
Login keeps its own extra prefixes (token URL, `approved claim
version`). Other commands' notes: not in this step.
Docs: `docs/architecture/cli-prompts.md`.
