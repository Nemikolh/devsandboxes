> Archived 2026-10-16. Historical, may be wrong; do not use as a reference.
> Current: [README.md](../../../README.md)

# Plans lead fragment

## Plans written

- docs/plans/active/helper-rpc.md: planned. Design only (source doc is untracked in git); no `channel::RPC`/`caps::RPC` in `src/devsbd/proto.rs`, no `rpc.rs`, no `HelperClient`; dispatcher runs still `exec_in` (`src/commands/dispatch.rs`) + polling `devsbd/src/ctl.rs` `wait_once`/`follow`. Folded in worktree-locking's stand-ins (`worktrees.sync`).
- docs/plans/active/windows-support.md: planned. Remote-only client direction, nothing built: `serve`/`api`/`machine` and `src/serve/machines/` are `#[cfg(unix)]` in `src/main.rs`; `--machine` refused off unix. Folded in the native named-pipe ssh-agent relay (as "Later") and the default-machine question.
- docs/plans/active/machines-next.md: planned. Rollout steps 6 (remote forwards) and 7 (VS Code spike) from client-server.md, remote Inbox threads, nameless commands across machines; parked: auto-update, PTY attach over the protocol, `instances.follow`, default machine (→ windows-support).
- docs/plans/active/cli-proxy.md: parked. No `cli-proxy` crate (workspace `members = [".", "devsbd", "test-utils"]`), `config::Service` has no `mounts`; live config added then removed its `tools` service (../.devsandboxes commits `5db58e3`, `8349c82`), shared gh/sentry auth still bind-mounted in `template.base` "until the cli-proxy plan lands". Folded in sandbox-helper.md "Later: API proxying" (`channel::HTTP_PROXY` reserved, `#[allow(dead_code)]`) as transport option 3.
- docs/plans/active/agents-skills.md: parked. Raw idea from ideas-agents-skills.md; no `.agents/skills`/`AGENTS_SKILLS_DIR` anywhere in `src/`, `devsbd/`.

## status.md entries

### In progress

(none: nothing found partly built and actively worked on)

### Planned

- **helper**: host → in-container daemon requests (`RPC` channel), daemon-owned runs with restart policy, worktree stand-ins. → [plan](plans/active/helper-rpc.md)
- **daemon**: Windows as a remote-only client of a machine (CLI, TUI, exec over ssh). → [plan](plans/active/windows-support.md)
- **machines**: remote forwards and `port`, VS Code on remote rows (spike), remote Inbox threads, nameless commands across every machine. → [plan](plans/active/machines-next.md)
- **helper**: run the ssh-agent relay on Apple `container` on a Mac once (built, never tested there; README says "untested on Mac").

### Parked

- **services**: credential-isolated `gh`/`sentry-cli` through a `tools` service (shims + policy), or HTTP proxying over the helper. → [plan](plans/active/cli-proxy.md)
- **instances**: auto-mount a shared `.agents/skills` folder read-only into every instance. → [plan](plans/active/agents-skills.md)
- **forwards**: forwards count as idle holders forever (narrow to live connections or a grace); one bridge per forward instead of the instance's; auto-detect listening ports (`/proc/net/tcp`); foreground `devsandbox port` forwards invisible to the daemon/TUI; UDP; `portsAttributes`/`otherPortsAttributes`/`appPort`.
- **daemon**: `rename`, `done`/`undone`, `gc`, `service ls` still run in the CLI process (API has `instances.rename`/`instances.done`).
- **daemon**: startup autostart runs in-process with no build log (`serve/host.rs` `autostart_all`); roots with `autostart` sandboxes but no recorded instance are never autostarted (`config_roots`).
- **daemon**: adopted verbs tracked by pid only (pid reuse); no start-time check.
- **npm**: typed verb helpers (`api.run(…)`); `ApiInstances` has only `list`.
- **config**: `runArgs` pass-through / `envFile` / secrets (config-toml-findings #7; `runArgs` in `ignored()`).
- **config**: other ignored devcontainer properties (`capAdd`, `securityOpt`, `updateRemoteUserUID`, `workspaceMount`, `waitFor`, `userEnvProbe`, `shutdownAction`, `hostRequirements`, `overrideFeatureInstallOrder`).
- **features**: metadata `capAdd`/`init` ignored, `dependsOn`/`roundPriority` unresolved, default user not read from the image, no BuildKit build contexts.
- **config**: `devsandbox init` config scaffolding.
- **instances**: `worktree-include`/`worktree-link` for `folders` worktrees; extra worktrees in JSON/dashboard; `folders` entries that are repo subdirectories.
- **instances**: `.worktreeinclude.local`, `**`/`[...]` in `worktree-link`, read-only store mounts, unlink/restore command, shared files served by `devsbd` from tmpfs.
- **automations**: `devsbd boot` exec'ing into the daemon (it spawns it as a child today); a detached lifecycle form for long-running `postStartCommand` loops.
- **automations**: `npx devsandboxes init dispatcher` scaffold; declarative `[automation.*]`; webhook sources instead of polling; finer branch hand-over ("was held" mark).
- **helper**: `devsandbox agent <instance>` foreground relay command; real arch probing (`devsbd::candidates`); `O_NOFOLLOW` hardening of the outbox/runs shared dirs (deferred Low security finding; regular-file filtering is in).
- **dashboard**: double-click word / triple-click line, vi-style keyboard selection, clipboard fallbacks without OSC 52, copying a selection's markdown source.
- **dashboard**: `--all` for `:stop`/`:start`/`:rebuild`; `:logs`/`:inspect` prompt commands (no `CommandSpec`).
- **dashboard**: one terminal path for instances and services (services use `<runtime> exec -it`); progress for `services.rebuild`.
- **json**: raw-number cpu/mem (schema v2); plain `status` as a human summary (today `--json` is required).
- **inbox**: listener grants (a child following a thread's events), auto-withdraw older open forms, threads shared across users.

## Unfinished work found in shipped plans

Folded into a plan:
- client-server.md § Rollout 6/7, § Machines *Forwards*, § VS Code on a remote machine; machines.md § Not now / Later: not built (`MachineUse::Refused("port"|"vscode"|"inbox")` in `src/serve/machines/cli.rs`, `not_yet_port`/`not_yet_code` in `src/tui/app/actions.rs`, `Plan::Here`) → machines-next.md.
- client-server.md § Exec and terminal *Later* (PTY attach over protocol), § Build log (`instances.follow`: absent from `src/serve/api.rs`, only `instances.buildLog`) → machines-next.md, parked.
- client-server.md / machines.md: default machine (no code) → windows-support.md open question.
- ssh-agent.md § Windows (native) "future native path": no pipe client, `host_agent()` reads a unix socket (`src/devsbd/bridge.rs`) → windows-support.md "Later".
- sandbox-helper.md § Later: API proxying (`channel::HTTP_PROXY` unused) → cli-proxy.md transport option 3.
- worktree-locking.md § Limits: stand-ins for worktrees devsandbox didn't make (not done) → helper-rpc.md `worktrees.sync`.
- processes-stop-colors.md § Later/parked `.agents/skills` → agents-skills.md.

One-line entries (all verified not built; evidence per entry above, sources):
- serve.md § Future work, port-forwarding.md § follow-ups/Known limitations: forwards items (`src/serve/forwards.rs` module doc and holder rule; `commands/port.rs` `port` builds its own `Forward`s; nothing for `/proc/net/tcp` or UDP).
- serve.md § Future work, client-server.md § Rollout 3: other verbs into the daemon (`src/main.rs` calls `commands::rename::rename`, `commands::done::done`, `commands::services::gc` directly).
- verbs-as-requests.md § Known limits / § Parked: in-process startup autostart; pid-only adoption (`InFlight::adopt`); npm typed helpers (`npm/devsandboxes/index.d.ts`).
- before-machines.md F10 / dashboard-client.md F16: no autostart for roots without instances (`serve/host.rs` `config_roots`).
- config-toml-findings.md #7 "OPEN (next session)", untouched since: `runArgs` in `SandboxProperties::ignored()`, no env-file.
- run-command.md § 5, devcontainer-features.md § v1 scope cuts: ignored properties and feature cuts (no `dependsOn` in `src/`).
- devsandboxes.md § CLI surface: `devsandbox init` (no `Init` command in `src/main.rs`).
- folders-worktrees.md / worktreeinclude.md § Out of scope (same as instances fragment).
- automations.md § Step 16 (boot zombie, `devsbd/src/boot.rs` spawns the daemon), § Decisions (detached lifecycle form), § Step 21 (deferred Low FIFO/symlink finding: `runs.rs` uses `symlink_metadata`, `outbox.rs` filters `file_type().is_file()`, no `O_NOFOLLOW`), § Future (scaffold, `[automation.*]`, webhooks).
- dispatcher-branch-holders.md § Future: "was held" mark.
- sandbox-helper.md § SSH forwarding (`devsandbox agent`: no such subcommand), § Installing (arch probing, `devsbd::candidates` doc "deferred").
- ssh-agent.md § Future: Apple `container` relay untested on a Mac.
- tui-selection.md § Parked (no double-click/`arboard`/`wl-copy`/`xclip` in `src/tui`); tui-prompt-spec.md § Parked (no `--all` in `src/tui/spec.rs` `SPECS`, no logs/inspect `CommandSpec`).
- dashboard-client.md § Known limitations: one terminal path for services (`<runtime> exec -it`), `services.rebuild` progress. The rest of that list (Ctrl-D removes without prompt, F14 detached start leaves `after_start` waiting, early `(stale Ns)`, lazily started daemon inheriting fds) are limitations, not plans: for the dashboard/daemon leads' Known limitations.
- json-output.md § non-goals / § Step: raw cpu/mem v2 (no raw fields in `src/snapshot.rs`), plain `status` (`Status` requires `--json`).
- inbox-redesign.md § Open questions: listener grants, auto-withdraw forms, shared threads.

Stale claims noticed while verifying:
- helper-rpc.md: control socket `/run/devsandbox/ctl` → `daemon::CTL` = `/run/devsandbox/devsbd.ctl`; line-number code refs throughout.
- cli-proxy.md: code landmarks cite `src/commands/run.rs` (now `run/`, `ensure_bind_source` in `run/mounts.rs`), `VarCtx` (now `config::substitute` + `MountContext`), and a recreate-on-drift in `ensure_services` (it only warns; `ensure_service`); `docs/plan-cli-proxy.md` name still in the live config's comment.
- windows-support.md: "local runtime on Windows out of scope" vs code: the Windows build already runs every verb in-process against a local runtime (platform-support.md table).
- dashboard-client.md: `current_exe()` captured at dashboard start → `exe::devsandbox()` re-resolved per spawn.

## Abandoned

- cli-proxy.md Step 2 (confirm prompt before recreating a drifted global service): superseded; drift only warns and `services rebuild` recreates and rewires in place (`commands/services.rs` `ensure_service`, `services::rebuild`).
- inbox-redesign.md § One API: `instances.stop`/`instances.start` methods: replaced by verb requests (`src/serve/verbs.rs`); no such methods in `src/serve/api.rs`.
- inbox-threads.md § VS Code at a line, step 1 snapshot of extension-host sockets: replaced by polling in `devsbd vscode-goto --wait`.
- run-command.md § Worktree: `git worktree repair`: replaced by the host-path mirror + git companion mount (`git_companion_mount` in `src/commands/run/worktree.rs`).
- devsandboxes.md § Open questions (service ports, features): answered by port forwarding and native features.
- Compose support (run-command.md § 4, config-toml-findings.md #3): removed from the code (also in the instances fragment).
