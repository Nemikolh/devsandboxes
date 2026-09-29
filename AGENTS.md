# devsandbox

Rust CLI (`devsandbox`) that manages devcontainer-style sandboxes on docker /
podman / Apple container. Each sandbox config can spawn many _instances_
(containers named `devsandbox-<instance>`); repeat instances of one repo get
git worktrees so working trees are never shared.

## Module map

- `src/main.rs` — clap CLI; no subcommand on a TTY opens the TUI dashboard.
- `src/commands/*.rs` — one file per verb (`run`, `start`, `stop`, `rebuild`,
  `rm`, `ps`, `ls`, `exec`, `logs`, `inspect`, `stats`, `port`, `services::{gc,ls,rebuild}`). Shared name
  resolution in `commands/mod.rs` (`resolve_instance`: instance | sandbox |
  folder basename). `port.rs` resolves a forward route (instance | service via a
  running instance | injection fallback) and hosts the foreground `port` CLI.
  `run/` is split by concern: `mod.rs` (`run`/`materialize`, naming,
  `run_container`), `worktree.rs`, `git.rs` (`host_git`: the only way the
  host runs git — hooks/fsmonitor off, repo config allowlist-checked),
  `ssh_agent.rs`, `mounts.rs`, `image.rs`
  (feature image + Dockerfile/entrypoint generation), `lifecycle.rs`,
  `editor.rs` (VS Code name config); `mod.rs` re-exports what other commands use.
- `src/config.rs` — TOML schema: `[template.*]`, `[sandbox.*]`, `[services.*]`.
  `extends` deep-merges (tables recurse, **arrays concatenate** — that's how
  `template.base` services reach every sandbox). Config drift is detected by
  hashing the merged table (`config_hash` label on containers) plus the build
  dockerfile's contents (`build_hash` label); the shared rule is
  `commands::drift_decision`. The full user-facing format reference is the
  `skills/config-toml-spec/SKILL.md` skill: update it whenever a config field,
  merge rule, or `${…}` variable changes (the docs site renders it verbatim as
  `/docs/config`).
- `src/commands/services.rs` — service/network lifecycle. `scope = "isolated"`
  (default): one container per instance on a per-instance network. `scope =
"global"`: one shared container per config root. `gc` reaps unreferenced ones;
  `rebuild` recreates a service's containers and rewires running sandboxes in
  place (no restart, on any runtime).
- Automations (`docs/automations.md` design + steps, `docs/automations-guide.md`
  user guide): `src/commands/autostart.rs` — once-per-boot `autostart` pass
  (boot id per config root in `State.autostart_boot`) and the `"runtime"`
  restart-policy decisions; `src/commands/dispatch.rs` — host side of the
  dispatcher control API: authorization against current config, child naming
  (`<sandbox>-<key>`), ops as logged `devsandbox` subprocesses, run ops exec'd
  in the child's helper (executor injectable for tests).
- `src/runtime/` — backend abstraction over docker/podman/Apple container.
- `devsbd/` — static in-container helper (workspace member), built by
  `scripts/build-devsbd.sh` and embedded by `build.rs`; `src/devsbd.rs` holds
  the embedded blobs + install, `src/devsbd/proto.rs` the frame protocol
  shared with the helper via `#[path]`. See `docs/sandbox-helper.md`.
  `src/devsbd/forward.rs` is the host forwarder engine (unix-only): binds a
  local listener and tunnels each connection through a self-healing bridge, over
  the mux's flow-controlled `Connect` streams. See `docs/port-forwarding.md`.
  Also shared via `#[path]` (line-based, the helper has no serde):
  `bootfile.rs` (boot file for the in-container `postStartCommand` hook),
  `notify.rs` (notification record), `control.rs` (control request/response
  codec + exit codes), `escape.rs` (line escaping for all three). Host-only:
  `desktop.rs` (quiet `notify-send`/`osascript`). Helper-side:
  `devsbd/src/boot.rs` (`devsbd boot`: daemon + boot-file commands on container
  start), `outbox.rs` (`devsbd notify` queue, flushed by the daemon),
  `ctl.rs` (`devsbd ensure|ls|stop|rm|exec|run … <key>` clients over
  `api.sock`), `runs.rs` (`devsbd run start|ls|logs|wait`: tracked runs in a
  container).
- `src/tui/` — ratatui dashboard. `app/` is a deliberately I/O-free state
  machine (unit-tested): `App` + key/mouse dispatch in `app/mod.rs`, with
  `impl App` blocks split into `tree.rs` (selection/navigation), `view.rs`
  (modals, config/inspect view), `command_line.rs` (`:` prompt handling and
  completion), `terminal.rs`, `actions.rs` (one-key actions + pending queue),
  `procs.rs`, `inbox.rs` (Inbox tab: `devsbd notify` history, key dedupe,
  unread badges); shared test fixtures in `test_support.rs`. `tui/mod.rs` owns the terminal + event loop and runs
  docker work on background threads; `prompt.rs` is the `:` command line
  (`spec.rs` is its declarative grammar table — parsing _and_ tab completion
  derive from `SPECS`, so a new flag is one table entry);
  `term.rs` holds the integrated terminal's PTY sessions + tab strip
  (`TermSession`/`TermTabs`, `docker exec -it` shell rendered via vt100);
  `kitty.rs` emulates the kitty keyboard protocol for those sessions (vt100
  callbacks + key encoding; see `docs/tui-terminal.md`);
  `forwards.rs` is the Ports-tab worker thread that owns every live `Forward`
  (route resolution + docker work off the UI thread), modelled on `BridgeWorker`;
  it also reconciles sandboxes' `forwardPorts` against the running instances
  each snapshot, with host ports saved in `state.toml`.
- `site/` — Astro + MDX docs site (standalone pnpm project, not in the Cargo
  workspace; see `site/README.md`). Checks: `cd site && pnpm check && pnpm build`.

## Conventions & checks

- Docs in `docs/` are named after their topic, no `plan-` prefix or `-plan`
  suffix (e.g. `docs/rebuild.md`, not `docs/plan-rebuild.md`).
- Doc comments explain _why_; keep them current when moving logic.
- Tests live in `#[cfg(test)]` modules per file; most logic is factored to be
  testable without a container runtime.
- Tests that need docker or the embedded helper use
  `#[test_utils::docker_test]` / `#[test_utils::docker_test(helper)]` /
  `#[test_utils::helper_test]` (proc macros in `test-utils/`, a path-only,
  never-published dev-dependency) and return `Result<(), E>`. A missing
  requirement or `Err(why)` skips locally but **fails on CI** (`CI` env var);
  runtime side in `src/test_support.rs` (`gated`, `with_cleanup`).
- Checks: `cargo test --workspace` (covers the `devsbd/` helper crate too;
  plain `cargo test` only runs the root package). clippy/rustfmt are not
  installed in the default toolchain here.
- Commits follow Conventional Commits, all lowercase:
  `<type>(<what>): <description>` where `<type>` is `feat`, `fix`, `chore`,
  `docs`, `test`, `refactor`, … and `<what>` is the module or concept touched
  (`tui`, `rm` or any other command, `docker`, `apple`, `config`, …).
- Releases: follow the `release` skill (`.agents/skills/release/SKILL.md`).
  `CHANGELOG.md`'s `## Unreleased` section becomes the GitHub release body.
