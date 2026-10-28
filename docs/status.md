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

- **helper**: `exec` sessions after fleet step 1: surfacing detached
  sessions beyond the procs-fetched rows (maybe an Inbox note), a policy
  for sessions nobody reattaches to (`devsbd/src/sessions.rs`).
  → [plan](plans/active/resumable-exec.md)

- **helper**: daemon-owned runs (registry, run-state events, a `queued`
  state) with a restart policy, worktree stand-ins, container → host
  `RPC` (helper-rpc steps 4-7; the `RPC` channel and runs and sessions
  over it shipped), in the order of
  [helper-rpc](plans/active/helper-rpc.md#order-across-plans).
  → [plan](plans/active/helper-rpc.md)
- **machines**: VS Code on remote rows (spike),
  remote Inbox threads, nameless commands across every machine.
  → [plan](plans/active/machines-next.md)
- **machines**: a fleet: every machine a member (one `machine add`,
  links routed over a graph, `worker`/`full` trust per member, a
  swappable hub), soft-cap placement, devsandbox-owned code transfer
  between hosts, dispatcher children on any machine, checkpoints and
  re-placement: from fleet step 4b (one kind of machine) on, steps 1-4
  shipped on the two-kind model step 4b replaces (step 4's `PTY`,
  `TUNNEL` and `AGENT` on offered workers' links only).
  → [plan](plans/active/fleet.md)
- **daemon**: Windows as a remote-only client of a machine.
  → [plan](plans/active/windows-support.md)
- **helper**: drop the host-side `lsof` fallback for a forward's listening
  process (`port::listening_procs`, `ForwardSpec.probe`) once the helper
  can answer for a service reached via an instance and pre-`PORT_INFO`
  helpers are gone; the helper's `PortQuery` is then the only lookup.
  → [forwards](architecture/daemon/forwards.md#routes)
- **helper**: run the ssh-agent relay once on Apple `container` on a Mac
  (built, never tested there).

## Parked

- **instances**: repeat-instance worktrees in a container volume instead
  of a host bind (ownership stops mattering, faster I/O on VM-backed
  runtimes; the host reaches them through `host_git` and the helper).
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
- Feature cache entries never refresh: a mutable tag (`:1`) stays at its
  first pull. → [images](architecture/instances/images-and-features.md); `features::fetch_into`
- Features: `installsAfter` only (no `dependsOn`,
  `overrideFeatureInstallOrder`); metadata `capAdd`/`init` ignored;
  `_REMOTE_USER` falls back to `root` without reading the image user; no
  BuildKit build contexts. → [images](architecture/instances/images-and-features.md);
  `features::install_order`
- Feature entrypoints race lifecycle commands (nothing waits for e.g.
  dockerd after `run_container`). → [images](architecture/instances/images-and-features.md); `materialize`
- The `cargo` cache replaces the image's `CARGO_HOME`: its `config.toml`
  is not read and `cargo install` binaries land off `PATH` (in the shared
  cache's `bin/`). → [mounts](architecture/instances/mounts-and-state.md);
  `config::known_cache`
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
  `overrideFeatureInstallOrder`, `name`,
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
- Identity: only rootful docker (CI's `test` job), rootless podman (CI's
  `test-podman` job) and VM-backed runtimes are exercised; rootless docker,
  rootful podman, SELinux relabeling and `--socket-gid` have never run.
  → [identity](architecture/instances/identity.md)
- Identity: a `workspaceFolder` or `folders` target under `/root` isn't
  rewritten by the migration (`config_identity::Kind::RootPath`, no edit);
  under `identity = "host"` it stays unreachable for the adopted account
  until fixed by hand. → [identity](architecture/instances/identity.md)
- Identity modals (dashboard): the listing doesn't scroll, so one longer
  than the screen is cut (`ui.rs` `draw_confirm_modal`); the offer reads a
  machine's root only when its rows carry un-acked notes, so a machine's
  unused templates show up only in that machine's own dashboard
  (`identity::offer_routes`).
- Identity prompt (CLI): with no daemon, `run` without a sandbox name
  doesn't prompt (the in-process pick comes after it; `run`'s warning
  still shows), and off unix nothing prompts (no verb client).
  → [verbs](architecture/daemon/verbs.md)
- Identity under SELinux on docker: `--mount` binds aren't relabeled (only
  podman has `relabel=shared`), just warned about. `identity::create_args`
- Identity probe: a failed probe isn't cached, so a remote `DOCKER_HOST`
  (always ambiguous, the canary can't see its files) re-runs the canary and
  falls back to `identity = "image"` on every `run`. `run::identity::probe`
- Identity: rootless podman assumes the adopted account is 1000:1000
  (`--userns keep-id:uid=1000,gid=1000`) when the image's `/etc/passwd`
  can't be read (no `sh` in the image) or lacks the account; another uid
  then only gets a warning from `devsbd identity apply --expect`.
  `identity::ASSUMED`
- Apple `container` never reads the image's `/etc/passwd` (no throwaway
  container), so `${containerEnv:HOME}` falls back to `/home/<user>` (or
  `/root`) whatever home the image gives the account.
  `AppleContainer::image_passwd`
- `${containerEnv:…}` in `workspaceFolder` and `folders` stays verbatim:
  they resolve before the image is built. `run::materialize`
- Identity on rootless docker: VS Code attaches as root without the plan's
  `HOME` (`Instance::home` reaches execs only). `vscode::launch`
- Tool env: `PLAYWRIGHT_MCP_SANDBOX=false` and `CHROMIUM_FLAGS=--no-sandbox`
  are unverified knobs (no Playwright MCP or Debian `chromium` wrapper
  was tested). `run::tool_env::tool_env`
- Tool env: the Playwright library keeps its sandbox off (it adds
  `--no-sandbox`) and skips the root plan's browser wrapper unless a script
  launches with `chromiumSandbox: true` and `executablePath:
  $DEVSANDBOX_CHROME`; no env var does it. `run::tool_env::tool_env`
- Browser wrapper: never run under rootless podman (one more userns level);
  its test runs a fake browser under docker. `devsbd/src/browser.rs` `run`
- `rebuild --reprobe` without config drift does nothing unless `--force`
  is given too. `rebuild::rebuild`

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
- With the event stream live and nothing watched, a host agent appearing
  without a `bridges.ensure` is noticed only within `QUIET_POLL` (60 s).
  → [host poll](architecture/daemon/host-poll.md#the-poll); `host::tick`
- Apple `container` has no event stream, so its daemon lists containers
  every `POLL` (5 s) even when idle.
  → [host poll](architecture/daemon/host-poll.md#the-poll); `Backend::events_argv`

### machines

- A program run by a reconnecting `exec m/x` (or a remote dashboard tab)
  that exits 255 reports 254: 255 is kept for ssh's own failure.
  → [machines](architecture/daemon/machines.md#the-cli-clirs); `commands::exec::reported_code`
- A local daemon exit (handoff, idle, shutdown) cancels a held non-`detach`
  remote verb (every CLI verb): there is no spool to adopt it by.
  → [machines](architecture/daemon/machines.md); `machines::forward::verb`
- Daemon-side ssh is `BatchMode=yes`, one handshake per forwarded verb:
  passphrase-prompting keys fail. → [machines](architecture/daemon/machines.md); `ssh::api_argv`
- A CLI re-run's `machines.hold` isn't renewed: if the local daemon exits
  or hands over while the ssh session runs (`exec -it m/x` left open), the
  machine's agent relay goes with it for the rest of that session unless
  something else (a dashboard) wants the machine.
  → [machines](architecture/daemon/machines.md#the-cli-clirs); `machines::cli::hold`
- Every agent check on a machine with a live agent relay (`host_agent`'s
  liveness connect: each `Bridges::reconcile` of its daemon's poll, each
  verb line its API relay rewrites) opens and closes a stream through the
  relay to this host's agent. → [machines](architecture/daemon/machines.md#agent);
  `bridge::agent_live`
- Several hosts linking one machine with `agent` on each run a relay;
  processes there use the newest live socket (mtime), not their caller's.
  → [machines](architecture/daemon/machines.md#agent); `endpoint::agent_sockets`
- `machine add` has no `--command`: it sets `command` only when it finds or
  installs devsandbox in `~/.local/bin` off the login `PATH`; a remote devsandbox
  anywhere else off `PATH` is set by hand in `machines.toml`.
  → [machines](architecture/daemon/machines.md#registry-machinestoml)
- `machine add`'s install copies this binary, so only to a machine of the
  same OS and arch (`install::platform_matches`); updating a devsandbox
  that's a symlink replaces the link with a file (`install::INSTALL`'s
  `mv -f`). → [machines](architecture/daemon/machines.md#install)
- Remote rows live in memory: a fresh daemon shows a machine without rows
  until its link connects. → [machines](architecture/daemon/machines.md); `snapshots::Remotes`
- `vscode` and `inbox` are refused for a machine's instances on the CLI;
  the dashboard's `o` on a remote row answers "not supported yet" (its
  `p` and the CLI's `port` work); nameless commands are local only; no
  default machine. → [machines](architecture/daemon/machines.md); `machines::cli::route`, `not_yet_code`
- A forward to a machine's service reached via an instance shows no
  listening process (the header's `helper: false` gives
  `ProcessLookup::None`; locally the `lsof` probe covers it).
  → [forwards](architecture/daemon/forwards.md#remote-bridges); `tunnel::remote_opener`
- A remote forward needs the machine's embedded helper to speak this
  devsandbox's frame protocol; a mismatch is the bridge's version-mismatch
  error ("update devsandbox on <machine>").
  → [forwards](architecture/daemon/forwards.md#remote-bridges); `bridge::mismatch_message`
- A remote forward to an unreachable machine re-spawns ssh after every
  `RETRY` gap (2 s), with no backoff, until it's removed.
  → [forwards](architecture/daemon/forwards.md#the-engine-forward); `forward::retry_gap`
- The CLI's remote `port` waits `HEADER_WAIT` (10 s) for the tunnel header,
  so an interactive ssh prompt (host key, password) that takes longer
  fails. → [forwards](architecture/daemon/forwards.md#remote-bridges); `tunnel::remote_opener`
- The npm client's `forwards.list(dir)` can't pass `machines` (the typed
  `call('forwards.list', {machines: true})` can).
  → [api](reference/api.md#forwards); `npm/devsandboxes/index.js`
- `--machine <offered worker> ps` and `stats` are refused: no API call
  answers them over a fleet link yet.
  → [machines](architecture/daemon/machines.md#the-cli-clirs); `machines::cli::offered::refusal`
- `--machine <offered worker> gc` is refused: host maintenance on the
  worker is outside a fleet worker's trust.
  → [machines](architecture/daemon/machines.md#the-cli-clirs); `machines::cli::offered::refusal`
- `--machine <offered worker> service ls` is refused (not over a fleet
  link yet). → [machines](architecture/daemon/machines.md#the-cli-clirs); `machines::cli::offered::refusal`
- `logs --build --follow` on an offered worker's instance is refused
  (`logs --build` without it works). → [machines](architecture/daemon/machines.md#the-cli-clirs); `machines::cli::offered::refusal`
- An `exec` without a tty (piped, or `exec m/x -- cmd` without `-t`) on
  an offered worker's instance is refused: only session execs (`PTY`) go
  over the link. → [fleet](architecture/daemon/fleet.md#pty); `machines::cli::offered::Refused::ExecNoTty`
- `inspect` on an offered worker's instance prints no browser sandbox
  line (it comes from this host's record).
  → [machines](architecture/daemon/machines.md#the-cli-clirs); `machines::cli::offered::named`
- `--machine <offered worker> ls` lists only the config roots its
  instances name (placed configs without an instance left don't show).
  → [machines](architecture/daemon/machines.md#the-cli-clirs); `machines::cli::offered::config_roots`
- Service terminals on an offered worker are refused, as are terminals
  of its instances without a helper or whose helper daemon isn't root
  (`session.attach` is `needs_root`, e.g. `containerUser`).
  → [fleet](architecture/daemon/fleet.md#pty); `tui::remote::service_argv`, `fleet::pty`
- Verbs on an offered worker run with the worker's own env: host git
  there gets no `SSH_AUTH_SOCK` of the hub's agent, unlike an ssh
  machine's (`--session-env`).
  → [fleet](architecture/daemon/fleet.md#the-worker-table); `fleet::lent::admit`
- A hub notices a worker that's back (its `fleet-link` socket bound again)
  only at its offered link's next retry, up to 30 s later: a re-offer of a
  known worker doesn't wake the link.
  → [fleet](architecture/daemon/fleet.md#hub-mode); `fleet::offer::offer`
- A worker's `drain` in its offer (sent at the next link only: `lend on`
  ↔ `drain` keeps the current one) is only logged; the hub learns it from
  the worker's `capacity.get` and the worker refuses a `run` itself.
  → [fleet](architecture/daemon/fleet.md#lending-lend); `fleet::lender::transition`
- The capacity report's soft caps (`lend.toml` `[caps]`) are edited by
  hand (no `lend` flag), and measured against the whole host's memory and
  disk use, not what placed work takes; `cpus` caps nothing measured. The
  report rides the snapshots only while a root is watched (`capacity.get`
  builds it any time), and an Apple `container` host reports no cpus or
  memory.
  → [fleet](architecture/daemon/fleet.md#capacity); `capacity::over_caps`
- Under the worker policy a placed instance's plain `devsbd notify`
  messages (not threads, which need `inbox`) still reach the worker's
  Inbox and desktop.
  → [fleet](architecture/daemon/fleet.md#worker-policy); `inbox::decide`
- On a `Worker` connection, `containers.*` reach only instances'
  containers (not a fleet root's service containers), and an isolated
  service row's `containers` aren't cut to lent instances.
  → [fleet](architecture/daemon/fleet.md#the-worker-table); `fleet::lent::filter_snapshot`
- A hub's verb on a worker runs with the worker daemon's own process env
  (a systemd unit's or LaunchAgent's, not a login shell's as over ssh's
  `relay::SessionEnv`), so `PATH` additions from shell profiles are
  missing; fix: capture a login env once when lending starts.
  → [fleet](architecture/daemon/fleet.md#the-worker-table); `fleet::lent::admit`
- No build is marked published yet: the release workflow doesn't set
  `DEVSANDBOX_PUBLISHED=1`, so every hub says `dev` and a worker's
  download path never runs; a worker on another build than its hub takes
  the hub's binary over the link only with `dev-builds = true` in
  `lend.toml` and the same OS/arch, else stops offering. Dev builds from
  the same commit with different local changes count as the same build
  (the git hash has no dirty marker). A mixed-platform dev fleet is built
  per platform by hand.
  → [fleet](architecture/daemon/fleet.md#versions); `build.rs` `stamp_build`, `fleet::update::decide`
- A worker waiting for its hub's binary gives up only when the transfer
  hasn't started within 60 s (`PUSH_WAIT`) or fails; one that started and
  stalls while the link stays up waits until the link ends.
  → [fleet](architecture/daemon/fleet.md#versions); `fleet::worker` `Incoming::watch`
- A worker that won't follow its hub (dev build, failed update) is seen by
  the hub only as an offered machine whose link is gone (`unreachable`);
  the reason is on the worker (`lend.get`, `serve.log`).
  → [fleet](architecture/daemon/fleet.md#versions); `fleet::worker` `run_link`
- Placement's `has_image` reads only watched roots' snapshots (this
  host's while watched, a worker's last pushed), and `has_repo` is never
  filled (no repo locality yet).
  → [fleet](architecture/daemon/fleet.md#in-the-run-verb); `fleet::placed::candidates`
- A placed `run` is placed only by the daemon's `run` verb: a CLI `run`
  with no daemon to reach, the dispatcher's children and `--in-process`
  runs stay here (fleet step 6 for children), and only offered workers
  are candidates (an ssh machine needs `--machine` or a qualified name).
  The hub's `state.toml` doesn't record a placed instance (it's the
  worker's, seen as `<worker>/<name>` in federated views; fleet step 6).
  Moving the
  code runs before the forward and isn't cancelled by a hang-up or Ctrl-C
  meanwhile (the run, once forwarded, is); a worker that refuses the run
  (draining) keeps the code it was sent. A new `--branch` starts at the
  hub checkout's branch tip, not `origin/HEAD` as on the hub; a `--base`
  or `worktree-base` naming an `origin/…` ref fails on the worker (its
  checkout has no `origin`).
  → [fleet](architecture/daemon/fleet.md#in-the-run-verb); `fleet::placed::start_point`
- A placed `run` moves committed, pushed work
  only: one branch of each folder's `origin`, fetched by the hub; a
  folder with no `origin`, an unreachable one, a branch origin lacks, a
  `folder` inside the config root (a root that's a repo), a `folders`
  entry with a `${…}` variable or outside the root but not a checkout
  doesn't move. `folders` entries inside the root travel in its tar
  without their `.git`. The root's tar skips `.git`, `.worktrees`,
  `shared-volumes` and `shared-files` but not git-ignored files, and a
  symlink pointing out of the root refuses it. Everything is sent every
  time (`Transfer::haves` asks nothing; the worker keeps a root it has),
  the repo cache is keyed by origin URL (not by root commits) and never
  pruned, a worker's moved checkout keeps the branch it first checked out
  (later bundles update other branches' refs), and `roots/<hash>/` trees
  are never removed.
  → [fleet](architecture/daemon/fleet.md#moving-code); `fleet::code::offer`, `fleet::code::repo::land`
- After a worker's downgrade, a dashboard or other long-lived client still
  running the replaced, newer binary hands the new daemon off again at its
  next hello (its version is newer), restarting the older binary each
  time until that client is restarted.
  → [role](architecture/daemon/role.md#version-handoff); `daemon::hello`

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
- `devsbd boot`'s commands run without the user's supplementary groups
  (`cmd.uid/gid` clears them), unlike `docker exec -u`; fix: switch in
  `pre_exec` with `supplementary_groups`, as `rpc_runs::switch_user` does.
  → [in-container](architecture/helper/in-container.md); `boot::run_spec`
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
- A dispatcher's `run logs --follow` and `run wait` still poll the host with
  short control requests (`devsbd/src/ctl.rs` `wait_once`, `follow`), though
  the host → child leg is one held `run.wait`/`run.logs` stream; fix: a
  streaming control op (like `events-follow`) relaying `run.logs follow`.
  → [dispatchers](architecture/automations/dispatchers.md); `wait_once`

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
