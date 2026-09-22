# Plan: JSON output for UI consumers

Goal: an external UI (JS plugin) drives devsandbox entirely through the CLI.
Everything the TUI renders must be reachable as JSON. The single source of
truth is the TUI's `Snapshot` (`src/tui/data.rs:106`): both the TUI and the
JSON path consume the same struct, so parity holds by construction.

Explicit non-goals (park, don't build):
- No library/FFI bindings; the CLI is the API.
- No JSON for the interactive terminal — the UI owns its own PTY and runs
  `devsandbox exec` itself.
- No raw-number cpu/mem yet: `InstanceRow.cpu`/`mem` stay the runtime's
  pre-formatted strings (what the TUI shows). Raw metrics are a schema-v2
  idea, noted in the docs step.

Schema stability: every JSON payload is wrapped in
`{"schema": 1, "data": …}`. Bump the number on breaking changes.

## Step 1 — extract `src/snapshot.rs` from `src/tui/data.rs` (pure move)

Move the UI-independent snapshot machinery into a new top-level module
`src/snapshot.rs` (register in `src/main.rs:1-7`):

- types: `ContainerStatus` (data.rs:18), `InstanceRow` (:41), `SandboxRow`
  (:63), `ServiceRow` (:82), `Snapshot` (:106), `ServiceInput` (:242),
  `ResolvedServiceInput` (:250)
- fns: `extends_names` (:262), `service_source` (:274), `build_service_rows`
  (:288), `classify` (:358), `validate_sandbox` (:398), `collect` (:471),
  `drifted` (:704)
- their `#[cfg(test)]` tests move along.

Stays in `src/tui/data.rs` (tree/render concerns, depend on `super::procs`
or are display-only): `ORPHANS_NAME`, `Node`, `visible_nodes`,
`push_proc_nodes`, `sandbox_stats`, `humanize_secs`, `STALE_AFTER_SECS`,
`totals_line`, `plural`, and their tests. `tui/data.rs` re-exports
`pub use crate::snapshot::{collect, ContainerStatus, InstanceRow, SandboxRow,
ServiceRow, Snapshot};` so `tui/mod.rs:34`, `tui/app.rs:17`, `tui/ui.rs:13`
and all test imports keep compiling unchanged.

Constraints: zero behavior change, no signature changes, keep doc comments
with their items. `cargo test` green.

## Step 2 — `devsandbox status --json`: full snapshot as JSON

- Derive `serde::Serialize` on the five public snapshot types.
  - `Snapshot.collected_at: Instant` → `#[serde(skip)]` (TUI staleness
    only; a JSON consumer prints at collection time).
  - `ContainerStatus` → `#[serde(tag = "state", content = "text",
    rename_all = "lowercase")]` so it reads
    `{"state":"running","text":"Up 3 minutes"}` / `{"state":"missing"}`.
- New subcommand in `src/main.rs`: `Status { #[arg(long, required = true)]
  json: bool }` — `--json` mandatory for now, reserving plain `status` for a
  future human summary. Help text: "Machine-readable snapshot of sandboxes,
  instances, and services".
- New `src/commands/status.rs`: `snapshot::collect(dir)`, wrap in the
  `{"schema":1,"data":…}` envelope (small `Envelope<T>` struct in
  `commands/status.rs`, reused by step 3), `serde_json::to_string_pretty`,
  print. `Snapshot.error` is part of the payload — docker being down is data
  (rows still present from state), not a CLI failure; exit 0.
- Tests: serialize a hand-built `Snapshot` (reuse the constructors from the
  moved tests) and assert on the JSON string: envelope shape, status
  tagging, `collected_at` absent.

## Step 3 — `--json` on the existing read verbs

One `#[arg(long)] json: bool` per verb; when set, print
`{"schema":1,"data":…}` pretty JSON instead of the table and skip all
color helpers.

- `ps --json` (`src/commands/ps.rs:8`): serialize the sorted
  `Vec<ContainerRow>` — derive `Serialize` on `ContainerRow`
  (`src/runtime/mod.rs:32`). Same fields as the table (name, image, status).
- `stats --json` (`src/commands/stats.rs:10`): same treatment for
  `StatsRow` (`src/runtime/mod.rs:57`).
- `ls --json` (`src/commands/ls.rs:11`): emit `SandboxRow`s, not the ad-hoc
  table cells, so the UI gets `config_hash`, `issues`, structured
  `services`/`extends`. Requires factoring the sandbox-row construction out
  of `snapshot::collect` (data.rs:538-560 today) into
  `pub fn sandbox_rows(dir: &Path, config: &Config) -> Vec<SandboxRow>`
  used by both `collect` and `ls`. No docker calls in `ls` (unchanged).
  Empty config: `data` is `[]`, not the "no sandboxes" prose.
- `inspect --json` (`src/commands/inspect.rs:11`): print `pretty_inspect`
  output verbatim, skipping `paint_json_line`. No envelope — the payload is
  the runtime's own document, not our schema.

Tests per verb where logic exists (ls row building, envelope reuse);
plain-passthrough flags don't need new tests beyond compilation.

## Step 4 — document the JSON contract

`README.md`: a "JSON output" section — which verbs take `--json`, the
envelope, schema-versioning promise, the exec/PTY non-goal, and the parked
raw-metrics idea. Keep it short; the schema is the Rust types.

## Commit per step (conventional commits, lowercase)

1. `refactor(snapshot): extract snapshot collection from tui`
2. `feat(status): machine-readable snapshot via status --json`
3. `feat(json): --json output for ps, ls, stats, inspect`
4. `docs(json): document the json output contract`
