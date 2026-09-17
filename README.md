# devsandbox

A Rust CLI + terminal dashboard that manages devcontainer-based sandboxes directly on top of a container runtime (docker, podman, or Apple `container`) — no devcontainer CLI, no daemon. One `config.toml` defines templates, sandboxes, and shared services; devsandbox derives the containers, networks, mounts, and VS Code wiring from it.

Built for running multiple coding-agent sandboxes against the same repositories: instances are cheap, isolated, discoverable, and safe to throw away.

## Why

- **devcontainer.json semantics, better config.** Sandboxes accept devcontainer properties (`image`, `build`, `features`, lifecycle commands, `containerEnv`, `mounts`, `customizations.vscode.extensions`, …) in TOML, plus an `extends` deep-merge over reusable templates.
- **Concurrent instances of one repo.** A second `run` against a folder already in use gets its own git worktree (under `.worktrees/`, on a fresh `sandbox/<instance>` branch by default — customizable via `worktree-branch` or `run --branch`), so two containers never share a checkout.
- **Shared caches.** `caches = ["pnpm", "cargo", …]` bind-mounts per-config-root package-manager caches into every sandbox and sets the env vars to match (supported: pnpm, cargo, npm, yarn, go, pip).
- **Shared services.** `[services.*]` containers (e.g. postgres) run alongside sandboxes on a per-project network — `isolated` (one per instance, default) or `global` (one per config root).
- **No daemon.** Docker is the source of truth for liveness; host-side state is a single TOML file in the user data dir. Everything devsandbox creates is prefixed `devsandbox-` so it is discoverable with plain `docker ps`.
- **Security boundary.** Mount sources come from the config layer (caches, shell history, declared `mounts` with a fixed variable set) — an agent driving the CLI gets start/exec/stop, not arbitrary host mounts.

## Quick start

```bash
cargo build --release
```

Create a `config.toml` (or just run `devsandbox run` — on a TTY it offers to generate an example):

```toml
[services.database]
image = "postgres:16"
scope = "global"            # one container shared by all instances; default is "isolated"

[template.base]
caches = ["pnpm"]
persist-shell-history = true
[template.base.features]
"ghcr.io/devcontainers/features/node:1" = { version = "lts" }

[sandbox.web]
extends = "base"
folder = "../web"           # host repo, mounted as the workspace
services = ["database"]
image = "mcr.microsoft.com/devcontainers/base:ubuntu"
postCreateCommand = "pnpm install"
customizations.vscode.extensions = ["dbaeumer.vscode-eslint"]
```

Then:

```bash
devsandbox run web          # start an instance in the background (name printed)
devsandbox ps               # list running instances (-a includes stopped)
devsandbox exec -it web zsh # exec into it (name = instance, sandbox, or folder)
devsandbox stop web         # stop it (--all: everything running)
devsandbox start web        # restart it (--all: everything stopped)
devsandbox rm web           # container + worktree + state entry
devsandbox gc               # reap unreferenced services and orphaned history files
devsandbox                  # no args on a TTY: interactive dashboard
```

`-C <dir>` points any command at a different config root.

## CLI

| Command                        | Behavior                                                                                                                                 |
| ------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------- |
| `devsandbox`                   | TUI dashboard (bare invocation on a TTY); help otherwise                                                                                 |
| `ls`                           | Sandbox configs defined in `config.toml`                                                                                                 |
| `ps [-a]`                      | Sandbox instances, joined from state + runtime                                                                                           |
| `run [sandbox] [--name n] [--branch b]` | Create a fresh instance in the background; interactive picker without args. Never restarts a stopped instance (that is `start`). `--branch` sets the worktree branch, overriding the sandbox's `worktree-branch` |
| `start <name>` / `start --all` | Restart a stopped instance, full path: config-drift warning, services recreated/started, service DNS rewired, `postStartCommand`         |
| `stop <name>` / `stop --all`   | `docker stop` of the instance and its isolated services; `start` restarts it                                                             |
| `rm <name>`                    | Remove container, worktree, and state entry (managed shell history is kept so a rebuilt instance inherits it)                            |
| `exec [-i] [-t] <name> <cmd…>` | Exec in an instance, honoring `remoteEnv` / `remoteUser`                                                                                 |
| `gc [--force]`                 | Remove services no live instance references, orphaned shell-history files                                                                |

`<name>` may be an instance name, a sandbox config name, or a repository folder basename; ambiguity prompts on a TTY and errors otherwise.

## The dashboard

Running `devsandbox` with no subcommand opens a ratatui dashboard: an instance tree grouped by sandbox with live status/CPU/mem, an expandable process forest per instance, a services view, a config explorer (original vs. `extends`-resolved TOML, side-by-side with `docker inspect`), instance logs, a command prompt with history and tab completion, one-key run and VS Code attach, and a help overlay. Config entries are validated in place; broken sandboxes are flagged.

One deliberate asymmetry: the `s` key stops a running instance and starts an exited one **on a background thread without leaving the dashboard**, so it uses a quiet bare start (`docker start` of the instance + its isolated services). It skips service recreation, DNS rewiring, and `postStartCommand` — their output (image builds, lifecycle commands with inherited stdio) would corrupt the alternate screen. For the full path, use `:start <instance>` (or CLI `devsandbox start`), which suspends the TUI and hands over the terminal — the same split as `s`-stop vs `:stop`.

## config.toml reference

Three top-level tables:

- **`[template.<name>]`** — reusable property sets. Sandboxes (and templates) reference them with `extends = "name"` or `extends = ["a", "b"]` (merged left-to-right, cycles detected). Merge is deep: nested tables merge recursively, **arrays concatenate** (so a sandbox adds to a template's `mounts`/`extensions` without restating them), scalars override.
- **`[sandbox.<name>]`** — a devcontainer definition plus devsandbox extras. Unknown keys are hard errors; valid-but-unimplemented devcontainer properties (e.g. `forwardPorts`, `runArgs`) parse but produce a warning at `run`.
- **`[services.<name>]`** — `image` xor `build`, plus `env`, `ports`, `command`, and `scope = "isolated" | "global"`.

devsandbox extras on a sandbox:

| Key                     | Meaning                                                                                               |
| ----------------------- | ----------------------------------------------------------------------------------------------------- |
| `extends`               | Template name(s) to deep-merge under this sandbox                                                     |
| `folder`                | Host project folder (relative to the config dir) mounted as the workspace                             |
| `folders`               | Extra VS Code workspace roots: container path → host folder; generates a `.code-workspace`            |
| `services`              | Service names to start and network alongside the instance                                             |
| `caches`                | Package-manager caches to persist and share across the config root                                    |
| `persist-shell-history` | Per-instance `.zsh_history` on the host; survives rebuilds, never shared between concurrent instances |
| `worktree-branch`       | Branch created for a worktree instance (supports `${instance}`); defaults to `sandbox/${instance}`; `run --branch` overrides it |

Mount sources support devcontainer-style variables: `${configDir}`, `${localWorkspaceFolder}`, `${localWorkspaceFolderBasename}`, `${localEnv:VAR}`, plus devsandbox's `${sharedVolumes}` (the config root's persistent-state dir) and `${instance}` (the running instance name).

`features` are fetched natively as OCI artifacts (via `curl`, no registry crates), cached per user, ordered by `installsAfter`, and baked into a derived image at `run`.

## Container runtimes

All runtime calls go through a `Backend` trait (`src/runtime/`): `docker` and `podman` share the docker-CLI shape; Apple's `container` (macOS 26+) has its own backend working around missing `--filter`, Go templates, `network connect`, and `top`. Selection: `DEVSANDBOX_RUNTIME=docker|podman|container`, else `container` on macOS, `docker` elsewhere. Details in `docs/runtimes.md`.

## State

- Instances: `~/.local/share/devsandbox/state.toml` (`$XDG_DATA_HOME` honored) — name → container, folder, worktree, workspace, remote env/user.
- Persistent per-config-root data (caches, shell history, `${sharedVolumes}`): `<config dir>/shared-volumes/`.
- Networks and containers are namespaced by a short hash of the config root, so two projects never collide.

## Repository layout

```
src/
  main.rs             CLI (clap) and dispatch; bare TTY invocation → TUI
  config.rs           config.toml model, extends deep-merge, mounts, validation
  features.rs         devcontainer features: OCI fetch, metadata, install order
  state.rs            host-side instance store (no daemon)
  commands/           ls, ps, run, start, rm, stop, exec, services (+gc)
  runtime/            Backend trait; dockerlike (docker/podman) and Apple container
  tui/                ratatui dashboard: app state, data collection, procs, prompt, ui
docs/                 design plans and findings that drove each milestone
.agents/skills/       orchestrator/implementer skill definitions for agent-driven development
.github/workflows/    ci.yml (check + test), release.yml (static builds on v* tags)
release.sh            version bump + tag from main
```

`devcontainers-cli/` (if present) is a local reference clone of the upstream [devcontainers/cli](https://github.com/devcontainers/cli), excluded from version control — kept only to study how the official CLI starts containers and installs features.

## Development

```bash
cargo test    # unit tests live next to the code they cover
cargo check
```

CI runs both on every push/PR to `main`. Releases: `./release.sh [patch|minor|major]` bumps `Cargo.toml`, tags `v*`, and the release workflow builds static binaries for linux (x86_64/aarch64 musl), macOS (aarch64), and windows (x86_64).

The project was built milestone by milestone — config core → runtime basics → devcontainer compatibility (features, mounts, services) → TUI → polish (worktrees, shell history, gc, macOS support) — with the plan documents for each phase preserved in `docs/`.
