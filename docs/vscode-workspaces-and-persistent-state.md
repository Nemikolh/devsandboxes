# VS Code workspaces & per-instance persistent state

Two additions: sandboxes open as named multi-root VS Code workspaces, and mount
variables for persisting per-instance state under `shared-volumes/`.

## `folders`: extra VS Code workspace roots

```toml
[sandbox.devsandboxes]
folder = "../devsandboxes"
workspaceFolder = "/workspaces/devsandboxes"
folders = { "/workspaces/.devsandboxes" = "../.devsandboxes" }
```

- Maps container path → host folder (relative to the config dir, must exist).
  Each entry is bind-mounted at its key.
- `run` generates `/workspaces/<instance>.code-workspace` inside the container:
  `workspaceFolder` first, then the `folders` entries. Rewritten on every reuse,
  so old instances self-heal. Best-effort (an image without `sh` just warns).
- The TUI `code` action opens it with
  `code --file-uri vscode-remote://attached-container+<hex>/<file>`; instances
  without a recorded file fall back to the old `--folder-uri` folder open.
- The window title is the file name — that is how the workspace gets named after
  the instance (VS Code has no separate workspace-name property). First instance
  = sandbox name, worktree instances `<sandbox>-2`, …
- `folders` supplements `folder`/`workspaceFolder`; the primary keeps driving
  the workdir, `${localWorkspaceFolder}` and worktrees. Extra folders are plain
  shared mounts: no worktree treatment, shared between concurrent instances.

## `${sharedVolumes}` / `${instance}` mount variables

Substituted in `mounts`, cache sources, and `workspaceFolder`:

- `${sharedVolumes}` → `<configDir>/shared-volumes`
- `${instance}` → the instance's persistent id (`web`, `web-2`, …): its name at
  creation, kept unique across renames (a new instance reusing a freed name gets
  `name-1`), so these paths never move once created

Used to persist zidane sessions per instance, with the host-shared binary and
credentials layered on top (docker/podman sort mounts by target, so nested
binds are safe; Apple `container` untested):

```toml
[template.base]
mounts = [
  "source=${sharedVolumes}/zidane/${instance},target=/root/.zidane,type=bind",
  "source=${localEnv:HOME}/.zidane/zidane,target=/root/.zidane/zidane,type=bind",
  "source=${localEnv:HOME}/.zidane/credentials.json,target=/root/.zidane/credentials.json,type=bind",
]
```

- Missing bind sources are auto-created on first run (dot-less basename → dir).
- Like `persist-shell-history` files, `rm` does **not** delete these dirs:
  sessions survive removal and revive when a same-name instance is re-created.
- Existing containers pick mount changes up only on recreate; copy
  `/root/.zidane` out of a live container first if its sessions matter.
