# Status

The only place for project state: what's in progress, planned or parked,
and what doesn't work today. Living docs link here instead of keeping
their own limitations. Finishing a plan, parking an idea or fixing a
limitation means editing this file in the same commit.

- A **limitation** is a fact about today's code (verified, with a symbol
  to check). Its fix idea, if any, rides on the same line.
- A **parked** item is a feature nobody is building. **Planned** has a plan
  in `plans/active/`.

## In progress

(none)

## Planned

- **helper**: host → in-container daemon requests (an `RPC` channel),
  daemon-owned runs with a restart policy, worktree stand-ins.
  → [plan](plans/active/helper-rpc.md)
- **machines**: remote forwards in the dashboard, VS Code on remote rows (spike),
  remote Inbox threads, nameless commands across every machine.
  → [plan](plans/active/machines-next.md)
- **daemon**: Windows as a remote-only client of a machine.
  → [plan](plans/active/windows-support.md)
- **helper**: run the ssh-agent relay once on Apple `container` on a Mac
  (built, never tested there).

## Parked

- **services**: credential-isolated `gh`/`sentry-cli` through a `tools`
  service, or HTTP proxying over the helper. → [plan](plans/active/cli-proxy.md)
- **instances**: auto-mount a shared `.agents/skills` folder read-only into
  every instance. → [plan](plans/active/agents-skills.md)
- **daemon**: `rename`, `done`/`undone`, `gc`, `service ls` as daemon verbs
  (they run in the CLI process; the API has `instances.rename`/`instances.done`).
- **npm**: typed verb helpers (`api.run(…)`); `ApiInstances` has only `list`.
- **config**: `runArgs` pass-through, `envFile`, secrets; `devsandbox init`
  scaffolding.
- **instances**: `.worktreeinclude.local`, `**`/`[...]` in `worktree-link`,
  read-only store mounts, an unlink/restore command, shared files served by
  `devsbd` from tmpfs.
- **forwards**: auto-detect listening ports (`/proc/net/tcp`); UDP;
  `portsAttributes`/`otherPortsAttributes`/`appPort`.
- **automations**: `devsbd boot` exec'ing into the daemon (it spawns it as a
  child today); a detached lifecycle form for long-running
  `postStartCommand` loops; `npx devsandboxes init dispatcher` scaffold;
  declarative `[automation.*]`; webhook sources; a "was held" branch mark.
- **helper**: `devsandbox agent <instance>` foreground relay; `O_NOFOLLOW`
  hardening of the outbox/runs shared dirs.
- **dashboard**: double-click/triple-click and keyboard selection; clipboard
  fallbacks without OSC 52; copying a selection's markdown source; `--all`
  for `:stop`/`:start`/`:rebuild`; `:logs`/`:inspect`; progress for
  `services.rebuild`.
- **json**: raw-number cpu/mem (schema v2); plain `status` without `--json`.
- **inbox**: listener grants (a child following a thread's events),
  auto-withdraw older open forms, threads shared across users.

## Known limitations

### instances

- **Bug**: `gc` never removes networks on docker: it matches network names
  against `network ls -q`, which prints IDs. → [services](architecture/instances/services.md);
  `services::gc`
- `gc` counts only running (or in-flight) instances as live, so it reaps a
  stopped instance's isolated services; the next `start` recreates them
  empty. → [services](architecture/instances/services.md); `services::gc`
- A drifted service is only warned about on `run`/`start`, and the warning
  suggests `gc` + re-run instead of `devsandbox service rebuild`.
  → [drift](architecture/instances/rebuild-and-drift.md); `services::ensure_service`
- Drift misses the build context and OCI feature contents; those need
  `rebuild --force`. → [drift](architecture/instances/rebuild-and-drift.md);
  `config::build_hash`, `sandbox_build_hash`
- Image tags and feature build contexts aren't namespaced by config root
  (`devsandbox-img-<sandbox>`, `-feat`, `devsandbox-img-svc-<name>`,
  `features/build-<sandbox>/`): two roots with the same names clobber each
  other. → [images](architecture/instances/images-and-features.md);
  `run::image::base_image_for`, `services::service_image`
- Feature cache entries never refresh: a mutable tag (`:1`) stays at its
  first pull. → [images](architecture/instances/images-and-features.md); `features::fetch_into`
- Features: `installsAfter` only (no `dependsOn`,
  `overrideFeatureInstallOrder`); metadata `capAdd`/`init` ignored;
  `_REMOTE_USER` falls back to `root` without reading the image user; no
  BuildKit build contexts. → [images](architecture/instances/images-and-features.md);
  `features::install_order`
- Feature entrypoints race lifecycle commands (nothing waits for e.g.
  dockerd after `run_container`). → [images](architecture/instances/images-and-features.md); `materialize`
- `caches` mount at fixed `/root/…` targets whatever `remoteUser` is.
  → [mounts](architecture/instances/mounts-and-state.md); `config::known_cache`
- `worktree-include`/`worktree-link` apply to the primary repo only, not to
  `folders` worktrees; a `folders` entry that is a repo subdirectory is
  always bind-mounted; extra `folders` worktrees don't show in `--json` or
  the dashboard. → [worktrees](architecture/instances/worktrees.md);
  `materialize`, `folders::needs_worktree`, `RunRecord`
- Worktrees devsandbox didn't create stay unlocked (only
  `gc.worktreePruneExpire=never` covers them), and the lock file is
  writable from the container. → [worktrees](architecture/instances/worktrees.md);
  `lock_worktree`, `gc_keeps_worktrees_env`

### config

- Many devcontainer properties parse but are ignored with a warning at
  `run` (`runArgs`, `appPort`, `portsAttributes`, `workspaceMount`,
  `capAdd`, `securityOpt`, `overrideCommand`, `shutdownAction`,
  `hostRequirements`, `waitFor`, `secrets`, `userEnvProbe`,
  `updateRemoteUserUID`, `overrideFeatureInstallOrder`, `name`,
  `customizations.vscode.settings`). → [config](architecture/config.md);
  `SandboxProperties::ignored`
- `extends` can't remove an inherited array entry; a diamond duplicates
  entries. → [config](architecture/config.md); `config::deep_merge`

### runtimes

- Apple `container`: no `--cache-from` (dropped with a warning), no
  `--privileged`, no restart policy (`autostart = "runtime"` acts as
  `true`), no file binds (feature file mounts skipped, no ssh-agent mount
  fallback), CPU shows `-`. → [runtimes](architecture/runtimes.md);
  `AppleContainer::supports_*`
- Podman: `autostart = "runtime"` restarts at boot only with
  `podman-restart.service`, which devsandbox never installs.
  → [runtimes](architecture/runtimes.md); `autostart::restart_decision`

### daemon

- Unix-only: the daemon and everything that needs it (dashboard, `api`,
  `machine`, `port`, `-d`, `--machine`, the ssh-agent relay). Off unix,
  verbs run in the CLI process. → [platform support](architecture/daemon/platform-support.md)
- Every live forward is an idle holder, so a running instance with
  `forwardPorts` or a saved ad-hoc forward keeps the daemon up forever.
  → [role](architecture/daemon/role.md); `Shared::holders`
- Each forward spawns its own bridge `exec` instead of sharing the
  instance's. → [forwards](architecture/daemon/forwards.md)
- Foreground `devsandbox port` forwards are invisible to the daemon, the
  Ports tab and the API, and vice versa. TCP only.
  → [forwards](architecture/daemon/forwards.md); `commands::port::port`
- The service injection route needs `/bin/sh`, root `exec` and a writable
  `/run` in the service image. → [forwards](architecture/daemon/forwards.md); `Planned::Inject`
- An adopted verb is tracked by pid only: a reused pid holds the daemon
  until that process exits. → [verbs](architecture/daemon/verbs.md); `InFlight::adopt`
- A CLI that loses its daemon mid-verb follows the build log from the
  start, re-showing streamed lines. → [verbs](architecture/daemon/verbs.md); `verbs_client`
- The startup autostart pass runs in the daemon process (no build log) and
  only walks roots with a recorded instance, so a root with `autostart`
  sandboxes but no instance never autostarts, dashboard open or not.
  → [host poll](architecture/daemon/host-poll.md); `Host::start`, `config_roots`
- The daemon doesn't enforce `hello`. → [api](reference/api.md); `serve::daemon::handle`

### machines

- A local daemon exit (handoff, idle, shutdown) cancels a held non-`detach`
  remote verb (every CLI verb): there is no spool to adopt it by.
  → [machines](architecture/daemon/machines.md); `machines::forward::verb`
- Daemon-side ssh is `BatchMode=yes`, one handshake per forwarded verb:
  passphrase-prompting keys fail. → [machines](architecture/daemon/machines.md); `ssh::api_argv`
- A remote login shell that prints to stdout breaks the hello.
  → [machines](architecture/daemon/machines.md); `ssh::Duplex`
- `machine add` has no `--command`; a remote devsandbox off `PATH` is set by
  hand in `machines.toml`. → [machines](architecture/daemon/machines.md)
- Remote rows live in memory: a fresh daemon shows a machine without rows
  until its link connects. → [machines](architecture/daemon/machines.md); `snapshots::Remotes`
- `vscode` and `inbox` are refused for a machine's instances on the CLI;
  the dashboard's `o` and `p` on a remote row answer "not supported yet"
  (the CLI's `port` works); nameless commands are local only; no default
  machine. → [machines](architecture/daemon/machines.md); `machines::cli::route`, `not_yet_port`

### helper

- Builds without an embedded helper (`cargo install` from crates.io) have
  no notifications, control requests, runs, port forwards or boot hook;
  ssh-agent falls back to the bind mount. → [helper](architecture/helper/overview.md); `devsbd::embedded`
- Images without `/bin/sh` can't get the helper, which in relay-first
  builds means no ssh-agent at all. → [ssh-agent](architecture/helper/ssh-agent.md); `devsbd::INSTALL_SCRIPT`
- No ssh-agent forwarding on native Windows hosts. Mount mode (no-helper
  builds) forwards nothing on macOS + podman / Colima-style docker or on
  Apple `container`. Instances created in mount mode keep it until
  recreated. → [ssh-agent](architecture/helper/ssh-agent.md); `relay_mode`
- On `start` of a `"runtime"`-restart instance the boot hook fires before
  the host rewrites the boot file, so that start runs the previously
  recorded `postStartCommand`/env. → [in-container](architecture/helper/in-container.md); `devsbd::sync_boot`
- Images with a tmpfs `/run` lose the helper and boot file on a runtime
  restart: `postStartCommand` doesn't rerun until a host `start`.
  → [in-container](architecture/helper/in-container.md)
- Two CLI builds used on the same containers evict each other's
  in-container daemon on every install, cutting live agent streams.
  → [protocol](architecture/helper/protocol.md); `devsbd` `take_over`
- An older CLI exec'ing a newer helper sends no `Caps`, so that bridge
  never carries agent streams. → [protocol](architecture/helper/protocol.md); `known_host_caps`
- A record the host can't decode (helper newer than the daemon) stalls the
  outbox at that record. → [bridges](architecture/helper/bridges.md); `handle_notify`
- The helper's arch is found by a blind second try, no probe.
  → [helper](architecture/helper/overview.md); `devsbd::candidates`

### inbox

- `devsandbox inbox done|reopen|act` and the API's `inbox.thread.*` change
  the thread only; the child instance's `done` flag flips only from the
  dashboard (`d`/`u` or a `done: true` button). → [model](architecture/inbox/model.md);
  `wire::decide_done`, `mark_thread_child`
- `devsandbox inbox` has no draft-save, notify dismiss or mark-read
  (API-only). → [inbox cli](reference/inbox-cli.md); `InboxCommand`
- The Inbox is per host: each host has its own store; the dashboard shows
  only this host's. → [model](architecture/inbox/model.md)
- An `inbox.json` from a newer schema version is unreadable by an older
  build. → [model](architecture/inbox/model.md#store); `Inbox::from_json`
- `thread ls` fails ("too large to list") past ~512 KiB of JSON.
  → [threads](architecture/inbox/threads-and-feed.md); `EVENTS_BODY_CAP`

### automations

- Autostart never runs off Linux/macOS (no boot id). → [autostart](architecture/automations/autostart.md);
  `read_boot_id`
- After a runtime restart, bridges, forwards, notify delivery and control
  resume only once the daemon runs (`serve install` or the next command).
  → [autostart](architecture/automations/autostart.md)
- `ensure --env` on an existing child changes only the saved env applied
  to `devsandbox exec` (the container's own env changes on rebuild);
  `ensure --branch` is ignored for existing children.
  → [dispatchers](architecture/automations/dispatchers.md); `ensure_action`
- Dispatcher lifecycle ops are serialized host-wide: one long `ensure` (up
  to 30 min) blocks every other dispatcher's `ensure`/`stop`/`rm`/`done`.
  → [dispatchers](architecture/automations/dispatchers.md); `dispatch_control` `LOCK`

### dashboard

- A thread action waiting on a detached start (`after_start`) never fires
  if the start fails after it was registered.
  → [inbox tab](architecture/dashboard/inbox-tab.md); `start_answered`
- `d` on an instance ignores modifiers: ctrl-d removes it without a prompt.
  → [tabs](architecture/dashboard/tabs-and-actions.md); `dispatch_key`
- Service terminal tabs run `<runtime> exec -i -t` directly, not
  `devsandbox exec` (two terminal paths). `TERM=xterm-256color` reaches
  only the host-side child. → [terminal](architecture/dashboard/terminal.md); `term_argv`
- Kitty keyboard emulation implements only "disambiguate".
  → [terminal](architecture/dashboard/terminal.md); `kitty::DISAMBIGUATE`
- OSC 52 is the only clipboard path: copies are cut at 1 MiB, and terminal
  app copies over the base64 bound are dropped silently.
  → [selection](architecture/dashboard/selection.md); `clipboard::MAX_COPY`
