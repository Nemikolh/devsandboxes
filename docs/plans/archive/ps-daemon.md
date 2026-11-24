> Archived 2026-11-24. Historical, may be wrong; do not use as a reference.
> Current: [architecture/dashboard/snapshot-and-json.md#ps](../../architecture/dashboard/snapshot-and-json.md#ps)

# ps through the daemon

`devsandbox ps` reads what the dashboard reads: the daemon's federated
snapshot of the config root, not a collection of its own plus a hand-made
call per machine.

Today `commands::ps::ps` runs `snapshot::collect_from` in-process and, with
machines registered, `machines::cli::calls::machines_rows` (one
`instances.list` per machine at its first root, rows tagged client-side).
That skips `serve::snapshots::merge`'s rules (each instance once per root,
the control plane's view of routed machines, the exposure and agent-grant
cuts, `machines` entries with their link state and error), and it always
pays a fresh `stats` pass (~1.5 s on docker) even when the daemon's poll
already has one.

## Step 1: ps is a one-shot federated watch

- `ps` connects with `serve::client::connect` (it starts the daemon if
  needed, as the dashboard does), `subscribe`s to `instances`, sends
  `instances.watch {dir, machines: true}` (as `tui::daemon::watch_call`
  does), reads `instances.snapshot` pushes (`Conn::next_notification`)
  until one is complete or a deadline passes, then `instances.unwatch`es
  and renders the last one it got.
- **Complete:** a push reports whether its statuses are listed and whether
  cpu/mem were gathered. `Snapshot::stats` is `#[serde(skip)]` today, so
  the wire carries no such signal (a first push can be a disk-only one,
  every status `unknown`). Put that on the wire (e.g. `stats` serialized
  plus a `listed` flag, or one `depth` field) and update
  `npm/devsandboxes/index.d.ts`. Machines count as settled when each
  `machines` entry is connected with its rows in, or has an error. Only
  the deadline (~5 s) ends the wait otherwise; past it, `ps` prints what it
  has, with one `warning:` per machine that isn't settled.
- **MACHINE column:** shown when the snapshot's `machines` is non-empty,
  so `main.rs` no longer passes a `machines` flag. Unreachable machines'
  rows show as the merge leaves them (last known), and their `error`
  becomes a `warning:` line.
- **Fallback:** `--in-process` (`client::set_in_process`), or a daemon
  that can't be reached, runs the current local
  `RuntimeView::gather(Depth::Full)` + `collect_from` (no machines) after
  one `warning:` naming why. Unix only: elsewhere it always runs local,
  like the dashboard.
- **Removed:** `machines::cli::calls::machines_rows` and its test.
  `--machine m ps` (`calls::listing`) stays as it is, since it names one
  machine.
- Keep the protocol loop a pure fn over an injected connection (calls +
  notifications), so it's tested without a daemon: complete on the first
  push, waits through a disk-only one, the deadline with a machine
  connecting, the unwatch sent, and the fallback chosen.
- **Docs:** `docs/architecture/dashboard/snapshot-and-json.md` (`## ps`),
  `docs/architecture/daemon/machines.md` (where it describes bare `ps` on a
  host with machines), `docs/reference/api.md` (the new snapshot field).
