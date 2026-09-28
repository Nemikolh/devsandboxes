# Changelog

## Unreleased

Worktree instances can now bring along the gitignored files a repo needs to run, like `.env` files, instead of starting without them.

### Added

- **Copy gitignored files into new worktrees.** A repo's `.worktreeinclude` file is always honored, and `worktree-include` adds more patterns (gitignore syntax). Matching untracked files are copied from the base checkout into each **new** worktree and never overwrite existing files.
- **Share gitignored files across all instances.** Each `worktree-link` path is moved to one shared copy under `shared-files/<sandbox>/`, and every worktree gets a symlink to it. An edit in one instance shows up in all of them. This also works on Apple `container`, which can't bind single files. Globs work within one path segment (`*` / `?`), and a leading `./` is accepted.

```toml
[sandbox.web]
folder = "../web"
worktree-include = ["fixtures/*.local.json"]   # copied once, per worktree
worktree-link = [".env", "packages/*/.env"]    # one live copy shared by every instance
```

- A `CHANGELOG.md`, which is also used as the GitHub release notes.

### Fixed

- TUI: pressing `s` to start an instance refreshes the list shortly afterwards, so the new state appears right away.

<details><summary>Commits</summary>

- 52da316 docs(changelog): readable release notes with examples, written ahead of release via a release skill
- 9dff820 docs(changelog): backfill changelog and generate it on release so github releases carry notes
- c6925a0 feat(run): one-segment globs and leading ./ in worktree-link
- ebd7200 feat(run): carry gitignored files into worktrees via copy or shared link
- d836a5a fix(tui): when start is pressed, refresh instance list after 500ms
- 7005680 chore(ci): fix warnings reported

</details>

## 0.3.4

On-demand port forwarding, devcontainer features that need privileges or entrypoints (docker-in-docker works now), and a full `config.toml` reference.

### Added

- **`devsandbox port`**: makes a port inside an instance or one of its services reachable on the host, without publishing it when the container is created and without a restart. Forwards reconnect by themselves after the target restarts or is rebuilt. When the image has `lsof`, the command shows which process is listening. Unix hosts only.

```bash
devsandbox port api 3000                      # localhost:3000 -> api's :3000
devsandbox port api 8080:3000                 # bind host :8080 instead
devsandbox port api --service postgres 5432   # api's postgres, via the api container
devsandbox port --service redis 6379          # a global service, no instance named
```

- **TUI Ports tab**: lists active forwards; `p` adds one and `d` removes one. Quitting never hangs on a forward that is still connecting.
- **Privileged features and feature entrypoints.** A feature that declares `privileged: true` now runs the container `--privileged`, and feature entrypoints run at every container start, so features such as docker-in-docker work out of the box. `privileged` can also be set directly on a sandbox. Both are skipped with a warning on Apple `container`.

```toml
[sandbox.infra]
image = "mcr.microsoft.com/devcontainers/base:ubuntu"
[sandbox.infra.features]
"ghcr.io/devcontainers/features/docker-in-docker:2" = {}
```

- **`config-toml-spec` skill**: the complete reference for `config.toml` (every key, merge rules, `${…}` variables), written for both people and agents.

### Fixed

- `${devcontainerId}` in feature mounts resolves to the instance id. Per-instance volumes are deleted on `rm` and kept on `rebuild`.
- A sandbox mount on the same target now replaces a mount inherited from a template, instead of Docker rejecting the duplicate.
- Nested mounts are applied parent-first on every runtime, not only on Docker.

### Internal

- `run.rs` and the TUI `app.rs` are split into per-concern modules. Docker-dependent tests are now required to pass on CI.

<details><summary>Commits</summary>

- ff124fb fix: don't specify the model on the skill
- e45afbd docs: fix broken rustdoc intra-doc links
- 75e1732 refactor(tui): split app.rs into per-concern modules
- 704bbaf refactor(run): split run.rs into per-concern modules
- 155b3de fix(features): resolve ${devcontainerId} and reap per-instance volumes on rm
- 4f1512c feat(features): run feature entrypoints at container start
- 7fe0e4f feat(config): honor sandbox-level `privileged` instead of ignoring it
- feb8ec7 feat(features): honor feature `privileged` so docker-in-docker style features work
- facce5f fix(run): order mounts parent-first before handing them to the runtime, since only docker sorts nested mounts itself and that is undocumented
- c060d7c docs(skills): add a config-toml-spec skill as the full config.toml reference, so users get one accurate source and devsandbox-cli stops carrying a drifting copy
- a71ea70 fix(run): let a downstream mount on the same target replace an inherited template mount, since only feature mounts could be overridden and docker rejected the duplicate target
- aa37f71 test(ci): build the devsbd helper and run the whole workspace on ci, and gate docker/helper tests behind #[docker_test] so they skip locally but fail on ci instead of silently passing
- 2d1bb66 docs(repo): drop references to internal project names from docs and test fixtures
- e7c0d57 test(devsbd): retry the prefer-port test with a fresh port when a parallel test grabs the released one first
- 6df4d8b test(devsbd): retry the embedded helper exec on etxtbsy, since parallel tests' forks can briefly inherit the just-written binary's write fd
- 8bee843 docs(port-forwarding): document the forwarding protocol, flow control and caps routing, plus the port command and ports tab, so docs match what shipped
- fdcf7e7 feat(tui): run ports-tab forwards on a worker thread that owns them, and make forward teardown prompt so quitting never hangs on a handshake
- 5ef2538 feat(tui): add a ports tab with a port prompt and p/d shortcuts so forwards can be requested and listed from the dashboard
- cea5d40 feat(port): show which process listens on a forwarded port via lsof when the image has it
- dfe7777 feat(port): add a foreground devsandbox port command that forwards instance or service ports until ctrl-c
- f1e6783 feat(port): resolve forward routes to an instance, a service via a running instance, or the service container itself, re-evaluated on every reconnect so restarts and rebuilds heal
- 07e5a0f feat(devsbd): add the host forwarder engine that listens locally and tunnels each connection through a self-healing bridge
- 81de9a5 feat(devsbd): dial forwarded connects in the daemon and negotiate caps end to end so forwards only run against capable helpers and agent-less bridges never take ssh-agent routing
- 9328728 feat(devsbd): add per-stream credit flow control, half-close and host-initiated streams to the mux so bulk forwarded traffic can't stall other streams or the keepalive
- 2cc5523 refactor(devsbd): let mux streams be tcp as well as unix sockets so forwarded ports can reuse the stream routing
- 95a1904 feat(devsbd): add connect/window/eof/caps frames and close reasons so port forwarding can land without a protocol version bump
- c038810 docs(port-forwarding): plan on-demand port forwarding over devsbd so dev servers bound to localhost and services are reachable without recreating containers
- a7a3b7b chore(release): keep the readme's pinned download links on the released version
- dc734b1 docs(readme): recommend npm and release archives, which embed devsbd, over the limited cargo install; document the relay as the main ssh-agent path

</details>

## 0.3.3

Release pipeline only: npm packages are now published with npm trusted publishing (OIDC) instead of a long-lived token. No changes to the CLI.

<details><summary>Commits</summary>

- cff095b ci(release): publish to npm with trusted publishing instead of a long-lived token

</details>

## 0.3.2

The npm root package is renamed to **`devsandboxes`**, because npm rejected `devsandbox` as too similar to an existing package. The installed binary is still called `devsandbox`.

```bash
npm i -g devsandboxes
npx devsandboxes --help
```

<details><summary>Commits</summary>

- 53cedc3 fix(npm): rename the root package to devsandboxes since npm rejects devsandbox as too similar to dev-sandbox

</details>

## 0.3.1

The CLI is now published to npm, so a Rust toolchain is no longer needed to install it. Prebuilt binaries ship as per-platform packages (linux x64/arm64, macOS arm64, windows x64), and the package also has a typed Node API.

> The root package was renamed to `devsandboxes` in 0.3.2; install that version or later.

<details><summary>Commits</summary>

- 86df202 feat(npm): publish the cli to npm with per-platform binary packages and a typed node api, so npx devsandbox works without a rust toolchain

</details>

## 0.3.0

ssh-agent forwarding no longer needs a socket bind mount. A small static helper (`devsbd`) is embedded in the release binaries, installed into each container, and relays the host agent over `exec` stdio. This works on every runtime, including Apple `container`, and keeps working when the host agent socket changes.

### Added

- **Relayed ssh-agent forwarding** through the embedded `devsbd` helper, used by default. Relays detect wedged connections with keepalives and handshake timeouts, and restart the daemon if the container was restarted outside devsandbox. `git` over ssh works inside `postCreateCommand` and the other lifecycle commands.
- **`devsandbox vscode <name>`** attaches VS Code to an instance from the CLI, using the same launch path as the TUI's `o` key.

```bash
devsandbox vscode web
```

### Changed

- Release archives (and later the npm packages) embed the helper. `cargo install` builds don't, and fall back to the bind mount, which works only on Linux docker/podman.
- The TUI manages ssh-agent bridges on a worker thread, so the dashboard never blocks on them.

<details><summary>Commits</summary>

- 3401365 docs(readme): say release binaries embed the helper now that the release job builds it
- 0529cde fix(run): give lifecycle commands the forwarded ssh-agent so ssh git operations in setup commands work
- 45e7b2b chore(release): build and embed the devsbd helpers in release binaries, and stop excluding src/devsbd from the published crate
- afc6ddc docs(devsbd): plan the release helper job and agent access for lifecycle commands
- 8ec0e6b chore(devsbd): fail ci builds that lack the helper, keep helper-less builds incremental, and catch a stale embedded helper in tests
- 66be187 refactor(tui): own ssh-agent bridges on a worker thread so reconciling never blocks the dashboard, and back off on version mismatch
- a8071fc feat(devsbd): let the bridge start a missing daemon so containers restarted outside devsandbox keep their relay
- bc882e8 feat(devsbd): detect wedged relays with keepalive and a handshake timeout so agent requests can't hang forever
- 304bef7 feat(devsbd): leave room in the wire protocol for additive changes and bound what a container can make the host do
- 8012eb8 feat(devsbd): make the relay the default ssh-agent path so forwarding survives agent rotation and needs no socket mount
- 56e49d8 docs(devsbd): plan relay-first forwarding and the helper hardening found in review
- 7e5a8b0 feat(devsbd): relay the host ssh-agent through a helper daemon and stdio bridges so forwarding needs no socket mount
- 4d6f29d docs(agents): run workspace tests so the devsbd helper crate is checked too
- 9e89b74 feat(devsbd): add the shared host<->helper frame protocol so both sides decode one wire format
- 95ec425 feat(devsbd): install the helper on run/start so containers carry the build matching the cli
- 3681527 feat(devsbd): embed a static in-container helper so relays work on every runtime without network or file binds
- 2a522c5 docs(devsbd): plan an embedded in-container helper for host<->container relays
- 8840709 feat(vscode): add `devsandbox vscode` command to attach vs code from the cli, sharing the tui's launch path

</details>

## 0.2.4

Fixes the Windows build, which broke the 0.2.3 release: 0.2.3 has no published binaries, so this is the first release that ships its changes. Also clarifies the docs.

<details><summary>Commits</summary>

- b7449fc fix: clarify docs
- fffd63e fix: windows build

</details>

## 0.2.3

ssh-agent forwarding, worktrees that start from the remote's default branch, and process signalling from the dashboard.

> This tag never got a GitHub release (the Windows build failed); its changes shipped in 0.2.4.

### Added

- **ssh-agent forwarding.** The host `SSH_AUTH_SOCK` reaches every instance through a per-instance symlink mount, and `start` re-points it before the container starts. Forwarding therefore survives reboots and re-logins without a rebuild. `exec` sets `SSH_AUTH_SOCK` on every session. `rm` removes an instance's link and `gc` removes orphaned ones.
- **Worktrees start from the remote's default branch.** Instead of branching from whatever is checked out in the base repo, `run` fetches `origin` and branches from `origin/HEAD` (or `main`/`master`), without touching the base checkout. Use `run --base` or `worktree-base` to pick a different start point.

```toml
[sandbox.api]
folder = "../api"
worktree-base = "origin/develop"
```

- **TUI process rows can be signalled**: `t` sends SIGTERM and `K` sends SIGKILL to the selected pid. `x` closes an exited terminal where it is.

### Fixed

- Process listings use pids from the container's namespace, so signals reach the right process. Images without `ps` fall back to `docker top` and are shown as not signalable. The listing no longer includes its own `ps` process.

<details><summary>Commits</summary>

- 1c56b2f feat(run): start worktrees from a fetched origin default branch
- 5d339f3 fix(runtime): drop the self ps row from process listings
- f4a3737 feat(docker): fall back to docker top when the image lacks ps
- d6a6896 fix(docker): list container-namespace pids so signals hit the right process
- b72d8ff feat(tui): make process rows signal-only (SIGTERM/SIGKILL)
- 140977f feat(tui): close exited terminal with x in place
- 300e420 feat(gc): reap ssh-agent links; rm drops its own
- a9a8cd2 feat(start): re-point ssh-agent symlink before container start
- d28302d feat(exec): inject SSH_AUTH_SOCK on every exec
- 7255429 feat(run): forward host ssh-agent via per-instance symlink mount
- 9d77713 docs(ssh-agent): plan agent forwarding via re-pointable symlink mount

</details>

## 0.2.2

Renames no longer lose per-instance state, feature mounts are applied, and `rebuild --force` is added.

### Added

- **`rebuild --force`** recreates an instance even when no drift is detected. Use it to pick up devsandbox-side changes that the config hashes can't see.

```bash
devsandbox rebuild web --force
devsandbox rebuild --all --force
```

- **Feature-declared mounts are applied**, such as the host docker socket from docker-outside-of-docker. A sandbox mount on the same target overrides the feature's, and mounts the host can't satisfy are skipped with a warning.
- **Persistent instance ids.** Everything tied to an instance (`${instance}` mounts, worktree dir, shell history, isolated services) is keyed by an id that never changes, so renaming an instance is purely cosmetic.
- TUI prompt completion is driven by the same spec table as parsing, so every flag completes (including `rebuild --force`).

### Fixed

- Shell history survives in VS Code terminals: `HISTFILE` is pinned in the rc files, not only in the container env.
- `rename` requires the exact instance name and no longer hangs the TUI.
- Missing instances can be rebuilt from the TUI.

<details><summary>Commits</summary>

- ad8c40b fix: make sure the subagent has a model specified
- 947a512 docs(agents): note spec.rs in the tui module map
- 60676c6 feat(tui): spec-driven tab completion; rebuild --force completes
- 8fc0f0f refactor(tui): drive prompt parsing from the spec table
- b1aa1d9 feat(tui): declarative command spec table for the prompt
- 65a5377 docs(naming): drop plan- prefixes/suffixes, add tui prompt spec plan
- a12f65c feat: add a --force flag to rebuild to be able to account from devsandbox changes
- a9c8efa fix(tui): missing instances can now be rebuilt
- d85edd5 feat(state): persistent instance_id so rename never moves per-instance state
- 73d8025 feat(features): honor feature-declared mounts
- 4fa1783 fix(run): pin HISTFILE in the rc files, not just container env
- 8d62bbd fix(rename): require exact instance name and never write stdout in the tui
- f9db061 refactor(features): drop unused cache_dir method
- d0ff413 chore(cargo): exclude release.sh, .agents, .github from crate

</details>

## 0.2.1

Packaging only: the crate gains a description and the MIT license so it can be published on crates.io, and docs are excluded from it.

```bash
cargo install devsandbox
```

<details><summary>Commits</summary>

- 9c03b41 chore(cargo): add description, mit license, exclude docs from crate

</details>

## 0.2.0

Instance lifecycle commands (`start`, `stop`, `rebuild`, `rename`), an integrated terminal in the dashboard, JSON output for scripts, drift detection for dockerfiles and services, and Apple `container` support.

### Added

- **Separate lifecycle verbs.** `run` always creates a fresh instance. `start` / `stop` resume and suspend existing ones (`--all` for every instance), and `rebuild` (alias `recreate`) applies config changes in place while keeping the worktree, branch and per-instance state.

```bash
devsandbox run web --branch me/login-fix
devsandbox stop --all
devsandbox start web
devsandbox rebuild --all        # recreate every drifted instance
```

- **Integrated terminal in the TUI**: `t` opens a shell in the selected instance (zsh when available, otherwise bash) in its workspace folder. Terminals open as tabs; `[`/`]` cycle between them, `x` closes one, `ctrl-]`/`F12` leaves the terminal, and mouse scrollback works.
- **Machine-readable output.** `status --json` prints the full dashboard snapshot in a versioned envelope, and `ps`, `ls`, `stats`, `inspect` accept `--json` (contract in `docs/json-output.md`).

```bash
devsandbox status --json | jq '.data'
devsandbox ps --json
```

- **Service drift and rebuild.** Editing a dockerfile now counts as drift, for sandboxes and services alike. `service rebuild <name>` recreates a service and rewires running sandboxes without restarting them, and `service ls` lists services like `ls` lists sandboxes. Drifted services are marked in the TUI and can be rebuilt from there.

```bash
devsandbox service ls
devsandbox service rebuild postgres
```

- **`rename`** (CLI and `r` in the TUI), plus **`logs`**, **`inspect`**, **`stats`** subcommands that work the same on every runtime.
- **`shell-rc`**: host shell snippets sourced by the container's `~/.zshrc`/`~/.bashrc`.

```toml
[template.base]
shell-rc = ["rc/aliases.sh"]
```

- **Configurable worktree branch** via `worktree-branch` (supports `${instance}`) or `run --branch`.
- **Apple `container`** is supported as a runtime.
- TUI: coding-agent processes are highlighted in the process tree, and the detail panel shows an agent count.
- A `devsandbox-cli` skill that teaches coding agents to drive the CLI.

### Fixed

- Worktrees are created under the config dir instead of the base repo, and a branch name that already exists is rejected with a clear error.
- The base folder is reused correctly once its direct-mount instance is removed.

<details><summary>Commits</summary>

- 7c5beb0 ci: release on crates.io
- 5bd48a2 docs: document build-hash drift, service rebuild and service ls
- 781f636 feat(tui): surface service drift and rebuild services from the dashboard
- f1bf8b4 feat(services): service ls mirroring ls, sharing the tui's row builder
- e45c31e feat(services): rebuild a service without restarting its sandboxes
- 536dc85 feat(config): hash dockerfile contents so edits register as drift
- 736dfea feat(tui): probe zsh before bash for integrated terminal
- c08fc3d feat(rename): rename instances via r in the tui and a cli verb
- 08cae4e feat: add shell-rc
- 1e3de35 feat: set integrated terminal cwd for workspaces
- 7d6afef docs(json): document the json output contract
- dd7da88 feat(json): --json output for ps, ls, stats, inspect
- 1274c13 feat(status): machine-readable snapshot via status --json
- 5647ba3 refactor(snapshot): extract snapshot collection from tui
- 27dd37e docs(json): plan for machine-readable output
- 9e8386f chore: add support for apple container as fallback
- 33b65e1 docs: commit implemented plan
- e1d0c0e fix(tui): help bar shows t term and the real s verb
- 1781881 feat(tui): wire the terminal into the event loop and mouse
- c5587cf feat(tui): render the integrated terminal panel
- 904856e feat(tui): terminal tabs, focus model and key routing
- f86f54a feat(prompt): flag and branch-value completion for run
- b78ae47 feat(tui): pty session core for the integrated terminal
- 144130c docs(agents): record the conventional-commit format
- 0cb8abb feat(rebuild): add --all for batch drift fixes
- 6777b44 docs(skill): run never reuses an instance
- cdf458f Point drift docs at rebuild instead of rm + run
- 58e1d07 TUI: s on a stopped drifted instance rebuilds instead of bare-starting
- 7e7e3c0 Add rebuild (alias: recreate) to fix config drift in place
- 635ed25 Extract materialize() from run()
- cb9c929 fix: bug with base folder never reused
- 05194eb feat: separate instance lifecycle: run creates, start/stop restart
- f670ac6 Reject a worktree branch that already exists
- ac72c27 Make worktree branch name configurable
- d2678a8 Fix worktree created under base repo instead of config dir
- 44d8c48 cli: add logs, inspect, and stats subcommands
- a3fca98 feat: add skills/devsandbox-cli/SKILL.md skill to teach an agent how to use the tool
- 49facee tui: show on-demand agent count in the instance Detail panel
- 4cc7764 tui: highlight coding-agent processes blue in the process forest
- 04aaf9e docs: add README.md

</details>

## 0.1.1

First release. devsandbox runs devcontainer-style sandboxes directly on docker/podman from one `config.toml`, with no devcontainer CLI and no daemon.

### Highlights

- **devcontainer semantics in TOML.** Sandboxes use devcontainer property names (`image`, `build`, `mounts`, `containerEnv`, `remoteUser`, the lifecycle commands, VS Code extensions…). Unknown keys are a hard error. `extends` composes reusable templates: tables merge, arrays concatenate.
- **Native devcontainer features.** Features are fetched from their OCI registry and baked into a derived image, without the devcontainer CLI.
- **Concurrent instances of one repo.** A second `run` of the same folder gets its own git worktree and branch, so instances never share a checkout. Instance names are deterministic (`web`, `web-2`, …).
- **Services.** Sidecar containers are reachable by name. `scope = "isolated"` (the default) gives each instance its own copy; `scope = "global"` shares one per project. `gc` removes the ones no instance uses.
- **Shared caches and state.** `caches` shares package caches across instances, `persist-shell-history` keeps a per-instance history, and `${sharedVolumes}` / `${instance}` place other state per instance.
- **Config drift detection.** Each container records a hash of its merged config, so an edited config shows up as drift.
- **VS Code.** Attach to an instance as a named workspace, with extra roots from `folders` and `remoteUser` honored.
- **TUI dashboard** (run `devsandbox` with no arguments): a tree of sandboxes and instances with live CPU/memory, a process tree for each instance, a services view, a config explorer (original vs resolved, next to `docker inspect`), logs, and a `:` command prompt with history and completion.
- Static release builds for Linux (musl), macOS and Windows.

```toml
[services.postgres]
image = "postgres:16"

[template.base]
caches = ["pnpm", "cargo"]
persist-shell-history = true
[template.base.features]
"ghcr.io/devcontainers/features/node:1" = { version = "lts" }

[sandbox.web]
extends = "base"
folder = "../web"
image = "mcr.microsoft.com/devcontainers/base:ubuntu"
services = ["postgres"]
postCreateCommand = "pnpm install"
folders = { "/workspaces/shared-lib" = "../shared-lib" }
```

```bash
devsandbox run web             # first instance mounts ../web directly
devsandbox run web             # second instance: web-2, on its own worktree
devsandbox exec -it web-2 zsh
devsandbox rm web-2
```

<details><summary>Commits</summary>

- afb786a fix: remove unused file
- 362f295 gc: reap orphaned shell-history files
- b834316 rm: keep the managed shell-history file
- 415763a fix: now using devsandboxes, this file is no longer needed :tada:
- 020ff66 feat: ${sharedVolumes} and ${instance} mount variables
- 342e928 feat: open sandboxes as named VS Code workspaces via a folders property
- 377c54b fix(test): add connection timeout
- 00eaf4d fix: pin the rust version to the container version
- 4114307 feat: flag sandboxes with no image or build in the dashboard
- 9831fc8 feat: validate config entries in the dashboard and log run errors
- 392d028 docs: switch example sandboxes to native features
- a134871 run: bake devcontainer features into a derived image
- e326c19 features: fetch OCI feature artifacts natively
- cf318d4 config: type the devcontainer features property
- 1b2a410 docs: plan for native devcontainer features support
- 9260620 fix: failing test on local
- a4861d7 Add CI workflow to catch build/test breakage before release tags
- f90232f chore: fix CI release logic
- 155e30b agents: orchestrator and implementer skills
- fab60a2 docs: add the first plans
- f70d803 feat: add basic support for macOS
- ccc958f tui: split accent purple from selection green
- a225787 tui: config modal splits into config | docker inspect panes
- 3569eff tui: process forest under expanded instances
- f259245 stop: first-class command distinct from rm
- f50ec78 vscode attach: honor remoteUser in the name config
- 6b88849 tui: one-key run prefill and VS Code attach from the tree
- 303cae6 tui: instances tab as a sandbox tree
- 7bc7e89 tui: drop plan-step references from UI text and comments
- 6e6b1d3 tui: help overlay, instance logs, header totals, staleness marker
- 5da536e tui: command prompt with history and tab completion
- 012860a tui: config explorer modal with original vs resolved toggle
- 59d3139 tui: services view joined from config, state and docker
- 5b4f6fa tui: live instances view with background docker collection
- 7b48117 tui: dashboard scaffold behind bare invocation
- 6201081 ls: readable table with colors and an empty-state message
- 19acc23 Recursive/list extends, remote+container user, init, workspace vars, caches
- 7469421 Add managed per-instance shell history
- f80ee7c Implement mounts, additive merge, and global/isolated services
- 0a0df68 services: shared per-project network + service containers, add gc
- e415d9a run: deterministic naming, reuse, worktrees, config-hash drift; add rm
- 8f56f9c Add release CI: static builds for linux (musl), macos, windows on version bump
- a1ee668 Type sandbox config to the devcontainer schema and expand runtime support
- 722fe21 Add runtime basics: run, ps, exec, host-side state store
- cad57f9 Add config core: TOML model, extends deep merge, ls command
- 1fd3f93 initial commit

</details>
