# high-level architecture

What devsandbox is made of, where it keeps state, and how each moving part behaves. The README is the pitch plus a quick start; this is the reference.
Topic docs in `docs/` go deeper on single features.

## Principles

- **No daemon.** The runtime is the source of truth for liveness. Host-side
  state is a single TOML file. Everything devsandbox creates is prefixed
  `devsandbox-`, so it shows up in plain `docker ps`.

- **Config is the security boundary.** Mount sources only come from the config
  layer: caches, shell history, and declared `mounts` with a fixed variable
  set. An agent driving the CLI gets start/exec/stop, not arbitrary host
  mounts.

- **Instances are cheap and disposable.** A second `run` against a folder
  that's already in use gets its own git worktree (under `.worktrees/`, on a
  fresh `sandbox/<instance>` branch by default). Two containers never share a
  checkout.

- **Host git never runs repo-controlled code.** Containers can write the
  repo's `.git` (worktree instances bind-mount the base repo's), so every host
  git call goes through one wrapper (`commands/run/git.rs`): hooks and
  `core.fsmonitor` are disabled on the command line, and the repo's local
  config files are parsed (never via `git config`, which follows includes)
  and checked against an allowlist of keys that can't run commands. Anything
  else, an `ext::` remote, or an `objects/info/alternates` file refuses the
  operation with the file and key named; the user reviews and removes it.

## devsandboxes.toml

Three top-level tables:

- **`[template.<name>]`**: reusable property sets. Sandboxes (and templates)
  reference them with `extends = "name"` or `extends = ["a", "b"]`, merged
  left-to-right, with cycles detected. The merge is deep: nested tables merge
  recursively, **arrays concatenate** (so a sandbox adds to a template's
  `mounts`/`extensions` without restating them), and scalars override.
  Mounts are then collapsed to one per target, last entry wins, so a
  downstream table overrides an inherited mount by re-mounting its target.

- **`[sandbox.<name>]`**: a devcontainer definition plus devsandbox extras.
  Unknown keys are hard errors. Valid devcontainer properties that aren't
  implemented yet (e.g. `forwardPorts`, `runArgs`) parse but warn at `run`.

- **`[services.<name>]`**: `image` xor `build`, plus `env`, `ports`,
  `command`, and `scope = "isolated" | "global"`.

devsandbox extras on a sandbox:

| Key                     | Meaning                                                                                               |
| ----------------------- | ----------------------------------------------------------------------------------------------------- |
| `extends`               | Template name(s) to deep-merge under this sandbox                                                     |
| `folder`                | Host project folder (relative to the config dir) mounted as the workspace                             |
| `folders`               | Extra VS Code workspace roots: container path → host folder; generates a `.code-workspace`            |
| `services`              | Service names to start and network alongside the instance                                             |
| `caches`                | Package-manager caches to persist and share across the config root (pnpm, cargo, npm, yarn, go, pip)  |
| `persist-shell-history` | Per-instance history dir on the host, mounted at `/commandhistory` with `HISTFILE` set via container env and pinned in the rc files (VS Code's shell integration resets the env value); survives rebuilds, never shared between concurrent instances |
| `shell-rc`              | Host shell snippets (e.g. `["${configDir}/shell/aliases.sh"]`) mounted read-only and sourced by `~/.zshrc` and `~/.bashrc` in the container; concatenates under `extends` |
| `worktree-branch`       | Branch for a worktree instance (supports `${instance}`); defaults to `sandbox/${instance}`; `run --branch` overrides it. An existing branch (local, or `origin/<branch>` → tracking branch) is checked out rather than created |
| `autostart`             | `true` / `"runtime"`: bring instances up once per boot (see _Automations_); not hashed               |
| `dispatcher`            | `{ spawn, max-instances }`: instances may manage child instances over `devsbd` (see _Automations_); not hashed |

Mount sources support devcontainer-style variables: `${configDir}`, `${localWorkspaceFolder}`, `${localWorkspaceFolderBasename}`, `${localEnv:VAR}`. devsandbox adds `${sharedVolumes}` (the config root's persistent-state dir) and `${instance}` (the instance's persistent id: its name at creation, kept unique and unchanged by `rename`, so host paths anchored on `${instance}` never move).

`features` are fetched natively as OCI artifacts (via `curl`, no registry crates), cached per user, ordered by `installsAfter`, and baked into a derived image at `run`. Feature-declared `mounts` are applied too, and a feature declaring `privileged: true` runs the container `--privileged` (skipped with a warning on Apple `container`). Feature `entrypoint`s run at every start, chained ahead of the image's own `ENTRYPOINT` (`docs/entrypoint.md`). A sandbox mount with the same target overrides the feature's. A mount the host or runtime can't satisfy (missing bind source, file bind on Apple `container`, an unsupported `${…}` variable) is skipped with a warning. `${devcontainerId}` resolves to the instance id, and a volume named with it (or `${instance}`) is recorded in state and deleted by `rm`. See `docs/devcontainer-features.md`.

## Drift

Each container records two labels at creation:

- `config_hash`: a hash of the merged config table.
- `build_hash`: a hash of the build dockerfile's *contents*.

`autostart` and `dispatcher` are stripped before hashing: they apply without recreating the container. A mismatch flags the instance or service for `rebuild`, both in CLI warnings and in the TUI. The build context and devcontainer features aren't hashed, so edits to those go undetected; `rebuild --force` covers them. Shared rule: `commands::drift_decision`. See `docs/rebuild.md`.

## Services

- `scope = "isolated"` (default): one container per instance, on a per-instance network.
- `scope = "global"`: one shared container per config root.

`gc` reaps services nothing references. `service rebuild` recreates a service's containers and rewires running sandboxes in place, without restarting them. docker/podman resolve the new container via its network alias; Apple `container` gets its `/etc/hosts` rewritten. See `docs/service-rebuild.md`.

## Container runtimes

All runtime calls go through a `Backend` trait (`src/runtime/`):

- `docker` and `podman` share the docker-CLI shape.
- Apple `container` (macOS 26+) has its own backend, which works around missing `--filter`, Go templates, `network connect`, and `top`.

Selection:

1. `DEVSANDBOX_RUNTIME=docker|podman|container`, if set.
2. Otherwise on macOS: `docker` when the `docker` client is on `PATH` (e.g.
   OrbStack, which ships it and auto-selects its own context, or Docker
   Desktop), else Apple `container`.
3. Elsewhere: `docker`.

See `docs/runtimes.md`.

### VS Code attach on Apple `container`

VS Code attach (`o` in the dashboard) uses the Remote-Containers extension. On Apple `container` it needs `"dev.containers.experimentalAppleContainerSupport": true` in your VS Code settings. Two limitations come from the extension, not from devsandbox:

- Per-container extension auto-install (`nameConfigs`) is Docker-only. On
  Apple `container`, `customizations.vscode.extensions` are written as
  **recommendations** in the generated `.code-workspace`, so VS Code offers to
  install them on open instead of installing silently.

- The flag exists in VS Code proper. Forks (e.g. Cursor) may not expose it.

### ssh-agent forwarding

Relay-first. When the build embeds the `devsbd` helper (npm package, local builds after `scripts/build-devsbd.sh`), nothing is mounted: the in-container `devsbd` daemon listens on `/run/devsandbox/ssh-agent.sock`, and the host runs `exec -i <c> devsbd bridge` to carry each agent connection over exec stdio to the host `$SSH_AUTH_SOCK` (re-read per connection, so agent rotation needs no restart). The bridge lives for a CLI `exec`, for lifecycle hooks during `run`/`start`, and per running instance while the TUI is open. Needing only `exec`, it works on every runtime, including Apple `container`.

Builds without the helper (`cargo install`) fall back to bind-mounting the host `$SSH_AUTH_SOCK` through a per-instance symlink, which needs file binds (docker/podman). An instance keeps the mode it was created with until it's recreated (`devsbd::relay_mode`).

Either way, `SSH_AUTH_SOCK` is injected per exec, never baked into the container (`exec::ssh_auth_sock_env`); relay mode injects it only when the host has a live agent. On native Windows forwarding is compiled out; run devsandbox inside WSL2 instead. Details: `docs/sandbox-helper.md` (relay), `docs/ssh-agent.md` (mount path, Windows).

## The dashboard

Running `devsandbox` with no subcommand opens a ratatui dashboard:

- an instance tree grouped by sandbox, with live status/CPU/mem
- an expandable process forest per instance
- a services view
- a config explorer: original vs `extends`-resolved TOML, side by side with `docker inspect`
- instance logs
- a `:` command prompt with history and tab completion
- an integrated terminal
- one-key run and VS Code attach

Config entries are validated in place, and broken sandboxes are flagged.

One deliberate asymmetry: `s` starts or stops an instance on a background thread without leaving the dashboard. Its start is a quiet bare start (just
`docker start` of the instance and its isolated services). It skips service recreation, DNS rewiring, and `postStartCommand`, because their output would corrupt the alternate screen. For the full path, use `:start <instance>`, which suspends the TUI. See `docs/tui.md`.

## Automations

Sandboxes that start themselves and manage their own child instances, with the logic in a user script (`docs/automations.md`, user guide `docs/automations-guide.md`). Still no host daemon: everything host-side runs inside a devsandbox process.

- **`autostart`**: once per host boot per config root (boot id recorded in
  state), the dashboard load or the user's own `run`/`start` starts the
  sandbox's stopped instances, or `run`s one (`commands::autostart`).
  `"runtime"` also creates the container with `--restart unless-stopped`
  (docker/podman; Apple `container` falls back), applied in place since
  `autostart` isn't hashed.
- **Boot hook**: new containers run `devsbd boot &` before the keep-alive, so
  a runtime restart brings back the helper daemon and, from the boot file the
  host leaves at `/run/devsandbox/boot`, `postStartCommand` (log
  `/run/devsandbox/boot.log`).
- **Notify and control over devsbd**: in-container `devsbd notify` queues
  records in an outbox; `devsbd ensure|ls|stop|rm|exec|run …` send requests to
  the daemon's `/run/devsandbox/api.sock`. The daemon relays both over the
  existing frame channel (`channel::NOTIFY`, `channel::CONTROL`) to the newest
  bridge whose host advertises the cap. Only the dashboard's bridges do (one
  per running helper instance), so notifications queue and control fails with
  exit 75 while it's closed. The host re-checks `dispatcher` on every request
  and denies everyone else.
- **Dispatch as subprocesses**: the control handler runs on the dashboard's
  bridge worker, so each child operation is a `devsandbox -C <config root>
  run|start|rebuild|stop|rm` subprocess logging to
  `<data-dir>/devsandbox/logs/dispatch-*.log`, never the alternate screen;
  state-writing ops are serialized host-wide. Runs are `devsbd run start|ls|logs|wait` exec'd in
  the child, which stores them under `/var/lib/devsandbox/runs/`.

## JSON output

The read verbs emit JSON for scripts and external UIs:

- `status --json` returns the full dashboard snapshot (`instances`,
  `sandboxes`, `services`, runtime `name`/`version`). An unavailable runtime
  or config is data, not failure: an `error` field carries the reason and the
  exit code stays `0`.
- `ps`, `ls`, `service ls`, and `stats` return the same rows as their tables.
- `inspect --json` returns the runtime's own document, with no envelope.
- `run --json` returns the new instance (`RunRecord` in
  `src/commands/run/mod.rs`: name, id, container, workspace, host folder,
  worktree, branch). Stdout is redirected to stderr for the whole command
  (`src/json_stdout.rs`), so build/hook/git output can't corrupt the document.

Everything except `inspect` is wrapped as `{ "schema": 1, "data": … }`, and `schema` is bumped on breaking changes (`SCHEMA` in
`src/commands/status.rs`). Field contract: the serialized types in `src/snapshot.rs` (and `RunRecord`). See `docs/json-output.md`.

## State

- **Instances:** `~/.local/share/devsandbox/state.toml` (`$XDG_DATA_HOME`
  honored). Maps each name to its container, folder, worktree, workspace,
  remote env/user, ssh-agent target, config root (`config_dir`), and owning
  dispatcher (`dispatcher`, for children). It's global across config roots
  (`src/state.rs`); `autostart_boot` maps each config root's project id to
  the boot id its autostart pass last ran for.

- **Dispatch logs:** `<data-dir>/devsandbox/logs/dispatch-<unix>-<op>.log`.

- **ssh-agent links** (bind-mount fallback only): `<data-dir>/devsandbox/agent/<instance>.sock`.

- **Per-config-root data** (caches, shell history, `${sharedVolumes}`): `<config dir>/shared-volumes/`.

- **Worktrees:** `<config dir>/.worktrees/`.

- **Namespacing:** networks and containers are named with a short hash of the config root, so two projects never collide.

## Repository layout

```
src/
  main.rs             CLI (clap) and dispatch; bare TTY invocation → TUI
  config.rs           devsandboxes.toml model, extends deep-merge, mounts, validation
  features.rs         devcontainer features: OCI fetch, metadata, install order
  state.rs            host-side instance store (no daemon)
  snapshot.rs         serializable rows shared by JSON output and the TUI
  commands/           one file per verb; shared name resolution in mod.rs
  runtime/            Backend trait; dockerlike (docker/podman) and Apple container
  tui/                ratatui dashboard: app state machine, data, prompt, terminal, ui
docs/                 design notes and findings, one per topic
.agents/skills/       orchestrator/implementer skills for agent-driven development
.github/workflows/    ci.yml (check + test), release.yml (static builds on v* tags)
release.sh            version bump + tag from main
```

`devcontainers-cli/` (if present) is a local reference clone of [devcontainers/cli](https://github.com/devcontainers/cli), excluded from version control.
