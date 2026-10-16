# Snapshot and JSON output

The `Snapshot` is the one model of sandboxes, instances and services that
the dashboard renders and that `status --json` prints; the other `--json`
verbs share its row types and envelope. Read this before adding a field
to a row or a `--json` flag.

Code: `src/snapshot.rs` (`Snapshot`, `InstanceRow`, `SandboxRow`,
`ServiceRow`, `ContainerStatus`, `PhaseRow`, `Depth`, `RuntimeView`,
`collect`, `collect_with`, `collect_from`, `sandbox_rows`,
`service_rows`), `src/commands/status.rs` (`Envelope`, `SCHEMA`,
`status`), `src/commands/{ls,ps,stats,inspect,services,inbox,machine}.rs`,
`src/commands/run/mod.rs` (`RunRecord`, `run_record_json`),
`src/json_stdout.rs` (`JsonStdout`), `src/serve/api.rs`
(`InstancesList`), `src/tui/data.rs`.

## The snapshot

State is the truth for which rows exist; config and the runtime enrich
them. When the runtime or config is unavailable the rows still come from
state and `Snapshot.error` says why.

- `InstanceRow`: name, sandbox, container, `status`
  (`ContainerStatus`: `running`/`exited` with the runtime's text,
  `missing`, `unknown` before the runtime was asked), uptime, cpu/mem as
  the runtime's preformatted strings, folders, services, `drift`,
  `instance_id`, `dispatcher`, `done`, `phase` (`PhaseRow`, absent once
  ready), `root`, `machine`.
- `SandboxRow`: every configured sandbox, resolved or not (source `?`):
  source, folder, services, `extends`, config/build hashes, `issues`,
  `dispatcher`, `worktree_branch`, `root`, `machine`.
- `ServiceRow`: scope, source, ports, backing `containers` with status,
  `used_by`, `drift`, `root`, `machine`.
- Header data: `sandbox_count`, `runtime_name`, `runtime_version`.
  `collected_at` and `stats` are `#[serde(skip)]`.

`root` is the config root a row came from; `machine` is set only on a
federated snapshot's remote rows.

## Collection

`collect_with(dir, depth)` = `RuntimeView::gather(depth)` + a join of
state and config for `dir`. `Depth` is how much of the runtime is asked:
`Disk` (nothing: statuses `unknown`), `Listing` (`ps` + version: statuses
and drift), `Full` (plus `stats`, which dominates collection time).
`collect_from(dir, view)` joins against a `RuntimeView` already gathered,
which is how the daemon's one poll serves every watched root
([../daemon/host-poll.md](../daemon/host-poll.md)).

Callers: `status --json` (`collect`, `Full`), the daemon's poll
(`collect_from`, pushed as `instances.snapshot`), `instances.list`
(`collect_with` at the requested `depth`). The dashboard collects nothing
itself.

## The dashboard's copy

`instances.snapshot` and `instances.list` carry `InstancesList`: the
`Snapshot` flattened, plus `thread_children` (dispatcher children by
owner and key, for the Inbox thread pane). The row types derive
`Deserialize`, so the dashboard reads the same structs back
(`tui::daemon::snapshot_update`); the remote machines' link states ride
alongside. `src/tui/data.rs` re-exports the types and holds the
render-side helpers (`Node`, `visible_nodes`, header totals). Parity with
`status --json` holds by construction: one struct, one serde shape.

## JSON output

`Envelope { schema, data }`, `SCHEMA = 1`; additive fields keep the
number, a breaking change bumps it. Pretty-printed to stdout.

| verb | `data` |
|---|---|
| `status --json` (flag required) | `Snapshot` |
| `ls --json` | `SandboxRow`s (`sandbox_rows`, no runtime call) |
| `service ls --json` | `ServiceRow`s (`service_rows`) |
| `ps --json` | runtime `ContainerRow`s + `done` + `phase` |
| `stats --json` | runtime `StatsRow`s |
| `run --json` | `RunRecord` |
| `inbox ls --json` / `inbox show --json` | `ThreadSummary`s / `ThreadDetail` |

Not enveloped: `inspect --json` prints the runtime's own inspect document;
`machine ls --json` prints the daemon's `machines.list` answer as is.
(`inbox submit --json` is input, not output.)

`status --json` never fails on the runtime: its being down is data
(`Snapshot.error`), exit 0. An empty config gives `data: []` for `ls`/`service ls`.

## `run --json` and clean stdout

`run` spawns children that write to stdout (image builds, `docker run -d`,
git, lifecycle hooks, the autostart pass after it). `JsonStdout::capture`
runs before any work: it saves the real stdout and points fd 1 (or
`STD_OUTPUT_HANDLE`) at stderr, so every inherited child stream and stray
`println!` lands on stderr, and `emit` writes the one document to the
saved handle. `run_record_json` reads the record back from state. A
failure prints nothing on stdout and exits non-zero.

Limitations: [status.md](../../status.md#dashboard)
