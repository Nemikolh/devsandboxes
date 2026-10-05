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
devsandbox run web      # a sandbox from your devsandboxes.toml
devsandbox              # the dashboard
```

## CLI

| Command                                    | Behavior                                                                  |
| ------------------------------------------ | ------------------------------------------------------------------------- |
| `devsandbox`                               | TUI dashboard on a TTY; help otherwise                                    |
| `ls` / `ps [-a]`                           | Sandbox configs / instances                                               |
| `run [sandbox] [--name] [--branch]`        | Create a fresh instance (picker without args)                             |
| `start` / `stop <name>\|--all`             | Start / Stop an instance                                                  |
| `rebuild <name>\|--all [--force]`          | Recreate from current config when drifted; worktree and state kept        |
| `rm <name>`                                | Remove sandbox: container, worktree, and state entry are removed          |
| `exec [-i] [-t] <name> [cmd…]`             | Exec honoring `remoteEnv` / `remoteUser`; no `cmd`: login shell           |
| `vscode <name> [--goto PATH[:LINE[:COL]]]` | Open VS Code attached to the instance, optionally at a file and line      |
| `done` / `undone <name>`                   | Mark an instance done (kept as is, shown dimmed) / clear the mark         |
| `port <name> [--service s] <port…>`        | Forward a container/service port to the host until exited                 |
| `service ls` / `service rebuild`           | List services / recreate one and rewire running sandboxes in place        |
| `gc [--force]`                             | Reap unreferenced services, orphaned history files and agent links        |
| `status --json`                            | Full snapshot for scripts; `ps`/`ls`/`stats`/`inspect`/`run` take `--json` |

`devsandbox port` makes a port inside a running instance or service reachable on the host:

```bash
devsandbox port api 3000                # localhost:3000 -> api's :3000
devsandbox port api --service postgres 5432   # api's postgres, via the api container
```

## Automations

Sandboxes can come up on their own after a boot, notify you, and spawn child instances to do work, driven by a script you write:

```toml
[sandbox.pr-babysitter]
folder = "../pr-babysitter"
autostart = "runtime"       # true: devsandbox starts it once per boot; "runtime": docker/podman restart it
dispatcher = { spawn = ["web-app"], max-instances = 2 }
postStartCommand = "nohup ./babysit-loop.sh >babysit.log 2>&1 &"
```

Inside a sandbox, `devsbd notify "PR 123 needs you"` reaches the dashboard's Inbox tab and your desktop; a dispatcher manages its children with `devsbd`, an embedded in-container helper present in all sandboxes to interact with the host. Notifications and control need the dashboard open. Guide, sample dispatcher, and limitations: [`docs/automations-guide.md`](docs/automations-guide.md).

## Platform support

| Feature                     | Linux (docker/podman) | macOS (docker: OrbStack / Docker Desktop) | macOS (Apple `container`) | Windows (docker) |
| --------------------------- | :-------------------: | :---------------------------------------: | :-----------------------: | :--------------: |
| Instances, worktrees, services, caches | ✅         | ✅                                        | ✅                        | ⚠️ untested      |
| File bind mounts (`shell-rc`, feature mounts) | ✅  | ✅                                        | ❌ dirs only              | ⚠️ untested      |
| VS Code attach              | ✅                    | ✅                                        | ⚠️ experimental flag      | ⚠️ untested      |
| ssh-agent forwarding        | ✅                    | ✅ (relay only)                           | ⚠️ relay only, untested on Mac | ❌ use WSL2 |
| TUI dashboard + terminal    | ✅                    | ✅                                        | ✅                        | ⚠️ untested      |

Windows builds ship from the release workflow, but only the Linux build (e.g. inside WSL2) is exercised. Details: [`docs/runtimes.md`](docs/runtimes.md), [`docs/ssh-agent.md`](docs/ssh-agent.md).
