# Splitting the `devsandbox` crate for build speed

Status: explored, not agreed.

## Why

A clean `cargo build` takes about 20 s (32 cores, mold), and 16.4 s of
that is the one `devsandbox` bin crate. `cargo test --workspace --no-run`
takes about 32 s, 30.5 s of it the same crate built as a test. All 218
dependencies together finish in about 4 s, in parallel. Measured with
`cargo build --timings`, then `-Z time-passes` on the crate (stable
rustc, `RUSTC_BOOTSTRAP=1`):

| phase (bin / test) | s |
|---|---|
| front-end: macro expansion, `type_check_crate`, `MIR_borrow_checking` | ~7.5 / ~14 |
| `monomorphization_collector_graph_walk` | 2.5 / 4.3 |
| `codegen_to_LLVM_IR` | 5.4 / 8.7 |
| `LLVM_passes` (the only parallel part, by codegen unit) | 4.6 / 7.9 |
| link | 0.2 / 0.4 |

Most of the time runs on one thread for one 151k-line crate, so most
of the 32 cores sit idle. The link step isn't the cost: mold against
rustc's default LLD changed nothing. Test modules account for about 55k
of the lines, and they compile into a second full copy of the crate.

Cheaper knobs, measured the same way (clean crate, build / test, s):
baseline 19.3 / 30.5; `debug = "line-tables-only"` 16.4 / 28.5;
`debug = 0` 16.4 / 24.6; `-Z threads=8` (parallel front-end, nightly
flag) 16.0 / 21.9; the last two together 11.8 / 18.3. Generic bloat
isn't the lever either. `cargo llvm-lines` puts 3.5M LLVM lines in no
single hotspot: serde visitors (`visit_map`) are 4.6%, drop glue 2.6%,
all of serde about 15%, so de-bloating would gain about 10% at most.

A split gives rustc separate crates it can check and codegen in
parallel. It also gives each crate its own test binary, and an edit
leaves the crates below it untouched instead of re-checking one big
crate.

## Shape of the code

Lines per top-level module (tests included), and the modules it names
through `crate::`:

| module | lines | of which tests | depends on |
|---|---|---|---|
| `serve` | 63.5k | 20.7k | commands, devsbd, config, runtime, state, snapshot, build_log, inbox, … |
| `tui` | 28.0k | 11.6k | serve, inbox, snapshot, runtime, commands, … |
| `commands` | 24.1k | 9.7k | devsbd, serve, runtime, state, inbox, config, snapshot, … |
| `devsbd` (host side) | 10.9k | 4.9k | open, inbox, runtime, commands, serve |
| `inbox` | 6.6k | 2.2k | devsbd, state |
| `runtime` | 3.9k | 1.2k | devsbd, config, state |
| `config` + `features` + `worker_policy` + `daemon_config` + `key_guard` | 5.7k | | runtime |
| `snapshot`, `config_identity`, `build_log`, `open`, `state`, small ones | 9k | | various |

The cycles to break, with what crosses each one:

- `commands` ↔ `serve`, the big one. `commands` uses the client side of
  `serve`: `client::Conn`, `attach_hold`, `endpoint`, `machines::{Entry,
  MachineKind, link, changes, leave}`, `fleet::{identity, mesh,
  membership}`. `serve` uses `commands` as a library:
  `no_instance_matches`, `resolve_instance_noninteractive`, `pick`,
  `services::{project_id, service_container, ls_render}`,
  `run::cancel`, `ls::{Line, table}`, `ps::PsRow`, `done::status_line`,
  `exec::checked_session_id`.
- `devsbd` → `commands` (`dispatch`, `run::cgroup`,
  `port::listening_procs`) and → `serve` (`endpoint::socket_dir`,
  `agent_sockets`).
- `inbox` → `devsbd` (`notify`, `control`: the `#[path]`-shared codecs).
- `state` → `commands` (`run::BrowserSandbox`, `autostart::boot_id`);
  `snapshot` → `commands` (`drift_decision`, `services`);
  `config_identity` → `commands`/`serve` (`project_id`, `fleet::mesh`);
  `config` → `runtime` (`identity::Identity`), `runtime` → `devsbd`
  (`BIN`).
- `tui` → `serve` is one-way: wire types (`proto`, `api`,
  `snapshots::{SnapshotMachine, LinkState}`, `verbs::TtySize`,
  `client::Streamed`, `verbs_client::identity`). Its `commands` uses are
  small constants and pure fns.

## Proposed layers

Workspace crates, each `publish = false` except the binary (see
crates.io below):

1. `devsandbox-wire`: the `#[path]`-shared helper codecs (`devsbd/proto`,
   `bootfile`, `notify`, `control`, `escape`), so the helper crate and the
   host depend on a crate instead of `#[path]`.
2. `devsandbox-core`: `config`, `features`, `worker_policy`,
   `daemon_config`, `key_guard`, `state`, `runtime`, `exe`, `render`,
   `build_log`, `inbox`, `open`, `snapshot`, `config_identity`, plus
   `serve::endpoint`. Moving the few `commands` items they use
   (`drift_decision`, `services::project_id`, `BrowserSandbox`, `boot_id`)
   down here breaks the leaf-level cycles. About 25k lines.
3. `devsandbox-client`: `serve::{proto, client, endpoint}`, the wire
   views from `serve::api`/`snapshots` that clients read, and the
   `machines`/`fleet` registry and identity parts `commands` uses.
4. `devsandbox-commands`: `commands`, the host side of `devsbd`, and the
   shared name resolution and render fns `serve` calls today.
5. `devsandbox-serve`: the daemon (`serve` minus layer 3), on top of
   `commands`.
6. `devsandbox-tui`: on top of `client` and `commands`, independent of
   `serve`, so the two biggest crates (`serve` at about 50k after the move,
   `tui` at 28k) compile in parallel.
7. `devsandbox` (bin): `main.rs`, `json_stdout`, `build.rs`'s embedded
   helper blobs, wiring.

Rough expectation, not measured: the critical path becomes core →
client → commands → serve, against the full crate today, with `tui`,
each crate's test build and `devsbd` beside it. `serve` alone, about a
third of today's code, would take about 6 s clean. Its tests then get
a binary of their own, apart from the bin's.

## Open questions

- crates.io: `cargo install devsandbox` needs every crate it depends on
  published. Publish the internal crates too, drop the crates.io channel
  (`site/src/components/InstallTabs.astro` already marks it a degraded
  path), or keep a single crate for crates.io only.
- Where the `serve`/`commands` boundary really sits: the items listed
  above are the visible `crate::` paths. `use super::` chains and the
  `#[cfg(test)]` helpers shared across modules (`test_support`,
  `tui::app::test_support`, `serve::snapshots::tests`) need a pass of
  their own.
- `pub(crate)` everywhere turns into `pub` at crate boundaries; whether
  to accept that or add a `#[doc(hidden)]` convention.
- Whether to do it in one go or peel layers bottom-up (`wire`, then
  `core`), measuring after each.
- The cheap knobs above are independent of the split and can stack with it.
