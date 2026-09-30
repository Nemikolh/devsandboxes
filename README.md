<p align="center">
  <a href="https://devsandboxes.com">
    <picture>
      <source media="(prefers-color-scheme: dark)" srcset=".github/assets/logo-dark.svg">
      <img src=".github/assets/logo-light.svg" alt="devsandboxes" width="420">
    </picture>
  </a>
</p>

<h3 align="center">Let agents loose. Safely.</h3>

<p align="center">
  Every coding agent gets its own container and git worktree of your repo.<br>
  Watch them all from one dashboard, on docker, podman or Apple container.
</p>

<p align="center">
  <a href="https://devsandboxes.com/docs/quick-start"><img src="https://img.shields.io/badge/Get_started-devsandboxes.com-a9f36d?style=for-the-badge&labelColor=112014" alt="Get started at devsandboxes.com"></a>
  <a href="https://devsandboxes.com/examples"><img src="https://img.shields.io/badge/Examples-browse-91e6e8?style=for-the-badge&labelColor=112014" alt="Examples"></a>
  <a href="https://www.npmjs.com/package/devsandboxes"><img src="https://img.shields.io/npm/v/devsandboxes?style=for-the-badge&labelColor=112014&color=a9f36d" alt="npm version"></a>
</p>

```bash
npm i -g devsandboxes   # or a binary: https://devsandboxes.com/docs/quick-start#install
devsandbox run web      # a sandbox from your config.toml
devsandbox              # the dashboard
```

## CLI

| Command                             | Behavior                                                                  |
| ----------------------------------- | ------------------------------------------------------------------------- |
| `devsandbox`                        | TUI dashboard on a TTY; help otherwise                                    |
| `ls` / `ps [-a]`                    | Sandbox configs / instances                                               |
| `run [sandbox] [--name] [--branch]` | Create a fresh instance (picker without args)                             |
| `start` / `stop <name>\|--all`      | Restart (full path: services, DNS, `postStartCommand`) / stop             |
| `rebuild <name>\|--all [--force]`   | Recreate from current config when drifted; worktree and state kept        |
| `rm <name>`                         | Remove container, worktree, and state entry                               |
| `exec [-i] [-t] <name> [cmd…]`      | Exec honoring `remoteEnv` / `remoteUser`; no `cmd`: login shell           |
| `port <name> [--service s] <port…>` | Forward a container/service port to the host until Ctrl-C (unix only)     |
| `service ls` / `service rebuild`    | List services / recreate one and rewire running sandboxes in place        |
| `gc [--force]`                      | Reap unreferenced services, orphaned history files and agent links        |
| `status --json`                     | Full snapshot for scripts; `ps`/`ls`/`stats`/`inspect`/`run` take `--json` |

`<name>` matching an instance name (or id) exactly always picks that instance; sandbox and folder names only count when nothing matches exactly. `run --json` prints only the new instance's record on stdout (build and hook output goes to stderr); every JSON payload but `inspect`'s (the runtime's own document) is wrapped in `{"schema": 1, "data": …}`.

`devsandbox port` makes a port inside a running instance or service reachable on the host, on demand — no `-p` at create time, no restart. It runs in the foreground and stops on Ctrl-C.

```bash
devsandbox port api 3000                # localhost:3000 -> api's :3000
devsandbox port api 8080:3000           # bind host :8080 instead
devsandbox port api --service postgres 5432   # api's postgres, via the api container
devsandbox port --service redis 6379    # a global service, no instance named
devsandbox port api 3000 --address 0.0.0.0    # bind all interfaces (default 127.0.0.1)
```

An instance port tunnels straight into its container; a service port is reached through a running instance that references it, falling back to injecting the helper into the service container when none is available. Forwards are also available from the TUI's Ports tab — `p` to add, `d` to remove. A busy host port moves to the next free one up (`3000` → `3001`). Unix hosts only for now.

A sandbox's `forwardPorts` are forwarded by the TUI while it's open, for every running instance. Each instance keeps its host ports across dashboard restarts (saved in `state.toml`):

```toml
[sandbox.api]
services = ["db"]
forwardPorts = [3000, "8080:3000", "db:5432"]   # [host:][service:]port
```

## Automations

Sandboxes can come up on their own after a boot, notify you, and spawn child instances to do work, driven by a script you write:

```toml
[sandbox.pr-dispatcher]
folder = "../pr-dispatcher"
autostart = "runtime"       # true: devsandbox starts it once per boot; "runtime": docker/podman restart it
dispatcher = { spawn = ["web"], max-instances = 10 }
postStartCommand = "nohup ./babysit-loop.sh >babysit.log 2>&1 &"
```

Inside a sandbox, `devsbd notify "PR 123 needs you"` reaches the dashboard's Inbox tab and your desktop; a dispatcher manages its children with `devsbd ensure web --key pr-123`, `devsbd exec pr-123 --detach -- <agent>`, `devsbd rm pr-123`. Notifications and control need the dashboard open. Guide, sample dispatcher, and limitations: [`docs/automations-guide.md`](docs/automations-guide.md).

## Platform support

| Feature                     | Linux (docker/podman) | macOS (docker: OrbStack / Docker Desktop) | macOS (Apple `container`) | Windows (docker) |
| --------------------------- | :-------------------: | :---------------------------------------: | :-----------------------: | :--------------: |
| Instances, worktrees, services, caches | ✅         | ✅                                        | ✅                        | ⚠️ untested      |
| File bind mounts (`shell-rc`, feature mounts) | ✅  | ✅                                        | ❌ dirs only              | ⚠️ untested      |
| VS Code attach              | ✅                    | ✅                                        | ⚠️ experimental flag      | ⚠️ untested      |
| ssh-agent forwarding        | ✅                    | ✅ (relay only)                           | ⚠️ relay only, untested on Mac | ❌ use WSL2 |
| TUI dashboard + terminal    | ✅                    | ✅                                        | ✅                        | ⚠️ untested      |

Windows builds ship from the release workflow, but only the Linux build (e.g. inside WSL2) is exercised. Details: [`docs/runtimes.md`](docs/runtimes.md), [`docs/ssh-agent.md`](docs/ssh-agent.md).

ssh-agent forwarding uses an embedded in-container helper (`devsbd`) that relays the agent over `exec` stdio, so it needs no socket bind mount and works on every runtime, including Apple `container`. Release archives and the npm package embed it; **`cargo install` builds don't** (local builds do after `scripts/build-devsbd.sh`) and fall back to the bind mount (docker/podman on Linux; docker on macOS via Docker Desktop / OrbStack's `/run/host-services/ssh-auth.sock`). See [`docs/sandbox-helper.md`](docs/sandbox-helper.md).
