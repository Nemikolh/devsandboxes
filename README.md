# devsandbox

A Rust CLI + terminal dashboard that manages devcontainer-based sandboxes directly on top of a container runtime (docker, podman, or Apple `container`) — no devcontainer CLI, no daemon. One `config.toml` defines templates, sandboxes, and shared services; devsandbox derives the containers, networks, mounts, and VS Code wiring from it.

Built for running multiple coding-agent sandboxes against the same repositories: instances are cheap, isolated, discoverable, and safe to throw away.

- **devcontainer.json semantics in TOML**, plus `extends` deep-merge over reusable templates.
- **Concurrent instances of one repo**: each extra instance gets its own git worktree.
- **Shared caches and services** (e.g. one postgres per project or per instance).
- **No daemon**: the runtime is the source of truth; everything is named `devsandbox-*`.

How it works (config reference, state, runtimes, layout): [`docs/high-level-architecture.md`](docs/high-level-architecture.md).

## Quick start

```bash
cargo install devsandbox
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
```

```bash
devsandbox run web          # start an instance in the background
devsandbox exec -it web zsh # name = instance, sandbox, or folder basename
devsandbox                  # no args on a TTY: interactive dashboard
```

`-C <dir>` points any command at a different config root.

## CLI

| Command                             | Behavior                                                                  |
| ----------------------------------- | ------------------------------------------------------------------------- |
| `devsandbox`                        | TUI dashboard on a TTY; help otherwise                                    |
| `ls` / `ps [-a]`                    | Sandbox configs / instances                                               |
| `run [sandbox] [--name] [--branch]` | Create a fresh instance (picker without args)                             |
| `start` / `stop <name>\|--all`      | Restart (full path: services, DNS, `postStartCommand`) / stop             |
| `rebuild <name>\|--all [--force]`   | Recreate from current config when drifted; worktree and state kept        |
| `rm <name>`                         | Remove container, worktree, and state entry                               |
| `exec [-i] [-t] <name> <cmd…>`      | Exec honoring `remoteEnv` / `remoteUser`                                  |
| `service ls` / `service rebuild`    | List services / recreate one and rewire running sandboxes in place        |
| `gc [--force]`                      | Reap unreferenced services, orphaned history files and agent links        |
| `status --json`                     | Full snapshot for scripts; `ps`/`ls`/`stats`/`inspect` also take `--json` |

## Platform support

| Feature                     | Linux (docker/podman) | macOS (docker: OrbStack / Docker Desktop) | macOS (Apple `container`) | Windows (docker) |
| --------------------------- | :-------------------: | :---------------------------------------: | :-----------------------: | :--------------: |
| Instances, worktrees, services, caches | ✅         | ✅                                        | ✅                        | ⚠️ untested      |
| File bind mounts (`shell-rc`, feature mounts) | ✅  | ✅                                        | ❌ dirs only              | ⚠️ untested      |
| VS Code attach              | ✅                    | ✅                                        | ⚠️ experimental flag      | ⚠️ untested      |
| ssh-agent forwarding        | ✅                    | ⚠️ relay only, untested on Mac            | ⚠️ relay only, untested on Mac | ❌ use WSL2 |
| TUI dashboard + terminal    | ✅                    | ✅                                        | ✅                        | ⚠️ untested      |

Windows builds ship from the release workflow, but only the Linux build (e.g. inside WSL2) is exercised. Details: [`docs/runtimes.md`](docs/runtimes.md), [`docs/ssh-agent.md`](docs/ssh-agent.md).

ssh-agent forwarding uses an embedded in-container helper (`devsbd`) that relays the agent over `exec` stdio, so it needs no socket bind mount and works on every runtime, including Apple `container`. Release binaries embed it; **`cargo install` builds don't** (local builds do after `scripts/build-devsbd.sh`) and fall back to the bind mount (docker/podman on Linux only). See [`docs/sandbox-helper.md`](docs/sandbox-helper.md).
