# devsandbox

Rust CLI (`devsandbox`) that manages devcontainer-style sandboxes on docker /
podman / Apple container. Each sandbox config can spawn many *instances*
(containers named `devsandbox-<instance>`); repeat instances of one repo get
git worktrees so working trees are never shared.

## Module map

- `src/main.rs` — clap CLI; no subcommand on a TTY opens the TUI dashboard.
- `src/commands/*.rs` — one file per verb (`run`, `start`, `stop`, `rebuild`,
  `rm`, `ps`, `ls`, `exec`, `logs`, `inspect`, `stats`, `port`, `services::{gc,ls,rebuild}`). Shared name
  resolution in `commands/mod.rs` (`resolve_instance`: instance | sandbox |
  folder basename). `port.rs` resolves a forward route (instance | service via a
  running instance | injection fallback) and hosts the foreground `port` CLI.
- `src/config.rs` — TOML schema: `[template.*]`, `[sandbox.*]`, `[services.*]`.
  `extends` deep-merges (tables recurse, **arrays concatenate** — that's how
  `template.base` services reach every sandbox). Config drift is detected by
  hashing the merged table (`config_hash` label on containers) plus the build
  dockerfile's contents (`build_hash` label); the shared rule is
  `commands::drift_decision`.
- `src/commands/services.rs` — service/network lifecycle. `scope = "isolated"`
  (default): one container per instance on a per-instance network. `scope =
  "global"`: one shared container per config root. `gc` reaps unreferenced ones;
  `rebuild` recreates a service's containers and rewires running sandboxes in
  place (no restart, on any runtime).
- `src/runtime/` — backend abstraction over docker/podman/Apple container.
- `devsbd/` — static in-container helper (workspace member), built by
  `scripts/build-devsbd.sh` and embedded by `build.rs`; `src/devsbd.rs` holds
  the embedded blobs + install, `src/devsbd/proto.rs` the frame protocol
  shared with the helper via `#[path]`. See `docs/sandbox-helper.md`.
  `src/devsbd/forward.rs` is the host forwarder engine (unix-only): binds a
  local listener and tunnels each connection through a self-healing bridge, over
  the mux's flow-controlled `Connect` streams. See `docs/port-forwarding.md`.
- `src/tui/` — ratatui dashboard. `app.rs` is a deliberately I/O-free state
  machine (unit-tested); `mod.rs` owns the terminal + event loop and runs
  docker work on background threads; `prompt.rs` is the `:` command line
  (`spec.rs` is its declarative grammar table — parsing *and* tab completion
  derive from `SPECS`, so a new flag is one table entry);
  `term.rs` holds the integrated terminal's PTY sessions + tab strip
  (`TermSession`/`TermTabs`, `docker exec -it` shell rendered via vt100);
  `forwards.rs` is the Ports-tab worker thread that owns every live `Forward`
  (route resolution + docker work off the UI thread), modelled on `BridgeWorker`.

## Conventions & checks

- Docs in `docs/` are named after their topic, no `plan-` prefix or `-plan`
  suffix (e.g. `docs/rebuild.md`, not `docs/plan-rebuild.md`).
- Doc comments explain *why*; keep them current when moving logic.
- Tests live in `#[cfg(test)]` modules per file; most logic is factored to be
  testable without a container runtime.
- Checks: `cargo test --workspace` (covers the `devsbd/` helper crate too;
  plain `cargo test` only runs the root package). clippy/rustfmt are not
  installed in the default toolchain here.
- Commits follow Conventional Commits, all lowercase:
  `<type>(<what>): <description>` where `<type>` is `feat`, `fix`, `chore`,
  `docs`, `test`, `refactor`, … and `<what>` is the module or concept touched
  (`tui`, `rm` or any other command, `docker`, `apple`, `config`, …).
