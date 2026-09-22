# TUI prompt: spec-driven parsing + completion

## Problem

Adding an option to a prompt command today touches three hand-rolled places
that can (and do) drift:

- `src/tui/prompt.rs:15` — `COMMANDS` const (first-token completion list).
- `src/tui/prompt.rs:358` — `parse_line`: per-command hand-written token
  loops, flag knowledge, and usage strings.
- `src/tui/app.rs:1126` — `candidates_for` / `run_candidates` /
  `run_sandbox_token`: per-command completion knowledge, duplicated from the
  parser.

Live drift example: `parse_line` accepts `rebuild --force <instance>`
(`src/tui/prompt.rs:444`) but `candidates_for` only ever offers instance
names at `idx == 1` (`src/tui/app.rs:1141`), so `--force` never
tab-completes. `run` flags complete only because `run_candidates` re-encodes
them by hand.

The clap CLI (`src/main.rs`) is already declarative — one attribute per new
option — so the rework targets the TUI only. Deriving the TUI spec from
clap's `Command` introspection was considered and rejected: the prompt's
grammar is a small subset (no `--all`, no short flags, tab-dependent
rebuild), and coupling to clap internals costs more than a ~60-line local
table.

## Design

New module `src/tui/spec.rs`: one declarative table describing every prompt
command; both the parser and the completer read it. I/O-free, fully
unit-testable, mirroring the existing `app.rs`-is-pure convention.

```rust
/// What completes (and what is expected) in an argument position.
pub enum ArgValue {
    Instance,          // instance names from the snapshot
    InstanceOrService, // instance names; service names on the Services tab
    Sandbox,           // sandbox config names
    Branch,            // the branch `run` would use (worktree-branch/default)
    Free,              // free-form: parses fine, no candidates
}

pub struct FlagSpec {
    pub name: &'static str,          // "--force"
    pub value: Option<ArgValue>,     // None = boolean flag
}

pub struct CommandSpec {
    pub name: &'static str,
    pub aliases: &'static [&'static str], // rebuild → ["recreate"]
    pub usage: &'static str,              // "rebuild [--force] <instance>"
    pub flags: &'static [FlagSpec],
    pub positionals: &'static [ArgValue], // required, in order
    pub trailing: bool,                   // exec: rest of line is argv
}

pub const SPECS: &[CommandSpec] = /* run, exec, code, rm, rename, stop,
                                     start, rebuild */;
```

- **Generic parser** `parse_args(spec, tokens) -> Result<ParsedArgs, String>`:
  walks tokens once; flags position-independent; errors (`unknown flag`,
  `--x needs a value`, `unexpected argument`, usage on missing positionals)
  all derive from the spec. `ParsedArgs` holds set flags, flag values,
  positionals, trailing argv.

- **`parse_line`** (`prompt.rs`) shrinks to: look up spec by name/alias,
  `parse_args`, then one small match arm per command mapping `ParsedArgs` →
  the existing `PromptAction` variants. `COMMANDS` is replaced by a derive
  from `SPECS` (primary names only, same display order — aliases still
  parse but don't complete, as today).

- **Completion** (`app.rs`): `candidates_for` becomes generic. Given
  `(idx, tokens)` and the spec: previous token is a value-taking flag → that
  flag's `ArgValue` candidates; stem starts with `-` → flags not already on
  the line; otherwise → next unconsumed positional's `ArgValue` candidates,
  plus flags once all positionals are filled. `ArgValue` resolves through a
  small context struct (tab, sandboxes, instances, services, config) built
  where the data already lives (`app.rs:1063`). `run_sandbox_token`
  generalizes to "first positional not consumed by a flag value", driven by
  the spec's flag arity.

- The Services-tab rewrite of `Rebuild` → `ServiceRebuild` at
  `app.rs:1052` stays; `InstanceOrService` is its completion-side mirror.

Net effect: a new flag or command = one `SPECS` entry + one arm/field in the
`PromptAction` mapping. Completion, unknown-flag errors, usage strings, and
the command list follow automatically. `rebuild --force` completing is the
acceptance test.

## Step 1: spec module + generic parser

Add `src/tui/spec.rs` (registered in `src/tui/mod.rs`) with `ArgValue`,
`FlagSpec`, `CommandSpec`, `SPECS` for all eight commands, `find(cmd)`
(name or alias lookup), and `parse_args`. Port the grammar exactly from
`parse_line` (`src/tui/prompt.rs:358-468`), including: `run` at most one
positional, `exec` requires trailing argv, `rename` exactly two
positionals, `rebuild` flag position-independence. `#[cfg(test)]` tests for
the parser cover the same cases as the existing `parse_*` tests in
`prompt.rs` plus flag-value and trailing edge cases. Module compiles and is
tested but not yet wired in.

Constraints: no behavior change elsewhere; error strings should match the
current ones where practical (`unknown flag \`--x\``, usage messages) so
step 2's test churn stays minimal.

## Step 2: rewire `parse_line` and `COMMANDS` onto the spec

In `src/tui/prompt.rs`: replace the body of `parse_line` with spec lookup +
`parse_args` + a per-command `ParsedArgs → PromptAction` mapping; derive
the first-token completion list from `SPECS` (keep the `COMMANDS`-shaped
public surface `app.rs:19` imports, or export a `commands()` fn and update
the import). Delete the now-dead hand-rolled loops. All existing tests in
`prompt.rs` must keep passing (adjust only error-message assertions if
wording legitimately changed in step 1 — flag it in the report if so).

## Step 3: spec-driven completion in `app.rs`

Replace `candidates_for` internals (`src/tui/app.rs:1126-1196`) with the
generic walk described above; `run_candidates`/`run_sandbox_token` fold
into it. Keep the existing candidate _sources_ (snapshot instance names,
config sandbox names, service names, `worktree-branch` lookup at
`app.rs:1162-1167`) and the one-config-load-per-tab-keypress behavior at
`app.rs:1063-1086`. Existing completion tests (`app.rs:~2820-2910`) keep
passing unchanged where behavior is identical; add new ones: `rebuild
--force` completes, `rebuild b<TAB>` on the Services tab still offers
services, flags not re-offered once used, flag value positions complete.

## Parked (out of scope, one-liners after this lands)

- TUI `--all` for `stop`/`start`/`rebuild` to mirror the CLI.
- Prompt commands for `logs`/`inspect`.

## Checks

`cargo test` after every step (clippy/rustfmt not installed here).
