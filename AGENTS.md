# devsandbox

Rust CLI (`devsandbox`) that manages devcontainer-style sandboxes on docker /
podman / Apple container. Each sandbox config can spawn many *instances*
(containers named `devsandbox-<instance>`); repeat instances of one repo get
git worktrees so working trees are never shared.

## Module map

- `src/main.rs` — clap CLI; no subcommand on a TTY opens the TUI dashboard.
- `src/commands/*.rs` — one file per verb (`run`, `start`, `stop`, `rebuild`,
  `rm`, `ps`, `ls`, `exec`, `logs`, `inspect`, `stats`, `services::gc`). Shared name
  resolution in `commands/mod.rs` (`resolve_instance`: instance | sandbox |
  folder basename).
- `src/config.rs` — TOML schema: `[template.*]`, `[sandbox.*]`, `[services.*]`.
  `extends` deep-merges (tables recurse, **arrays concatenate** — that's how
  `template.base` services reach every sandbox). Config drift is detected by
  hashing the merged table (`config_hash` label on containers).
- `src/commands/services.rs` — service/network lifecycle. `scope = "isolated"`
  (default): one container per instance on a per-instance network. `scope =
  "global"`: one shared container per config root. `gc` reaps unreferenced ones.
- `src/runtime/` — backend abstraction over docker/podman/Apple container.
- `src/tui/` — ratatui dashboard. `app.rs` is a deliberately I/O-free state
  machine (unit-tested); `mod.rs` owns the terminal + event loop and runs
  docker work on background threads; `prompt.rs` is the `:` command line.

## Conventions & checks

- Doc comments explain *why*; keep them current when moving logic.
- Tests live in `#[cfg(test)]` modules per file; most logic is factored to be
  testable without a container runtime.
- Checks: `cargo test` (clippy/rustfmt are not installed in the default
  toolchain here).
