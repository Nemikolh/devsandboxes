# Changelog

## Unreleased

- c6925a0 feat(run): one-segment globs and leading ./ in worktree-link
- ebd7200 feat(run): carry gitignored files into worktrees via copy or shared link
- d836a5a fix(tui): when start is pressed, refresh instance list after 500ms
- 7005680 chore(ci): fix warnings reported

## 0.3.4

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

## 0.3.3

- cff095b ci(release): publish to npm with trusted publishing instead of a long-lived token

## 0.3.2

- 53cedc3 fix(npm): rename the root package to devsandboxes since npm rejects devsandbox as too similar to dev-sandbox

## 0.3.1

- 86df202 feat(npm): publish the cli to npm with per-platform binary packages and a typed node api, so npx devsandbox works without a rust toolchain

## 0.3.0

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

## 0.2.4

- b7449fc fix: clarify docs
- fffd63e fix: windows build

## 0.2.3

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

## 0.2.2

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

## 0.2.1

- 9c03b41 chore(cargo): add description, mit license, exclude docs from crate

## 0.2.0

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

## 0.1.1

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
