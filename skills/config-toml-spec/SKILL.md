---
name: config-toml-spec
description: Reference for devsandbox's `config.toml` format — `[template.*]`, `[sandbox.*]`, `[services.*]`, `extends` merge rules, `${…}` variables, and which devcontainer properties are implemented. Use when writing, editing, reviewing, or debugging a devsandbox config (e.g. porting a devcontainer.json to it).
---

# devsandbox `config.toml` spec

`config.toml` is devsandbox's single source of truth: one file, outside any project, describing every sandbox. It replaces per-repo `devcontainer.json` (same property names, TOML syntax, plus devsandbox extras).

## File layout

`config.toml` lives in the *config dir* (`-C <dir>`, default `.`). Three top-level tables, all optional:

```toml
[template.<name>]   # reusable, never run directly
[sandbox.<name>]    # runnable; `devsandbox run <name>`
[services.<name>]   # sidecar containers a sandbox lists in `services`
```

Next to it devsandbox uses: `shared-volumes/` (`${sharedVolumes}`, persistent host state), `.worktrees/<instance-id>/` (repeat-instance git worktrees). Relative paths (`folder`, `folders` values, `shell-rc`, `build.dockerfile`, `build.context`) resolve against the config dir.

## Templates and `extends`

- `extends = "name"` or `extends = ["a", "b"]`, on sandboxes *and* templates
  (recursive). Only templates can be extended. Cycles and unknown names error.
- Merge order: listed templates left-to-right, then the table's own body.
- Merge rules:
  - tables merge recursively (`containerEnv`, `build.args`, `customizations`…)
  - **arrays concatenate**, base first (`mounts`, `caches`, `services`,
    `shell-rc`, `customizations.vscode.extensions`…). There is no way to
    remove an inherited entry. Concatenation is what the two main use cases
    need:
    - VS Code extensions are inherited: a template's
      `customizations.vscode.extensions` stay installed when a sandbox adds
      its own.
    - Mounts are inherited: a template's `mounts` (agent binary,
      credentials, shared auth) reach every sandbox without restating them.
      A downstream table overrides an inherited mount by mounting the same
      target again: the last entry for a target wins, the earlier ones are
      dropped. A more specific target *inside* an inherited mount is not an
      override; both apply, in any order in the config: devsandbox mounts
      parents before the mounts nested inside them, on every runtime.
  - scalars: the later value wins.
- Diamond inheritance duplicates concatenated array entries (not deduped).
- After merging, the result must be a valid sandbox (below). Templates are
  only validated through the sandboxes that extend them.

## Sandbox properties

Keys are devcontainer camelCase, except devsandbox extras noted in kebab-case. **Unknown keys are a hard error**, so typos surface immediately.

### devsandbox extras

| key | type | meaning |
|---|---|---|
| `folder` | string | Host project dir (relative to config dir, must exist). Required by `run`. A second instance of the same folder gets a git worktree. |
| `services` | [string] | Names from `[services.*]` to start and attach. |
| `caches` | [string] | Shared package caches: `pnpm`, `cargo`, `npm`, `yarn`, `go`, `pip`. Each = bind mount under `shared-volumes/<sub>` + env vars pointing the tool at it (targets are `/root/...`). Unknown name errors. |
| `persist-shell-history` | bool | Per-instance `shared-volumes/history/<id>/.zsh_history`, dir mounted at `/commandhistory`, `HISTFILE` set (env + rc line). Kept on `rm`. |
| `worktree-branch` | string | Branch pattern for worktree instances, `${…}` vars allowed. Default `sandbox/${instance}`. `run --branch` overrides. |
| `worktree-base` | string | Commit-ish start point (e.g. `origin/develop`). Default: remote's default branch. `run --base` overrides. |
| `shell-rc` | [string] | Host shell snippets sourced by `~/.zshrc`/`~/.bashrc`. Must exist and be files (no auto-create). Parent dir mounted read-only at `/devsandbox/rc/<i>`. |
| `folders` | {container path = host dir} | Extra VS Code workspace roots. Key must be absolute and differ from `workspaceFolder`; value must exist. |

### Implemented devcontainer properties

| key | notes |
|---|---|
| `image` | Base image. Exactly one of `image` / `build` is needed. |
| `build` | `{ dockerfile (required), context = ".", args = {}, target, cacheFrom = str\|[str], options = [str] }`. Tag `devsandbox-img-<sandbox>`. `cacheFrom` ignored with a warning on runtimes without support. |
| `features` | `{ "<oci-ref>" = { opts } \| "version" \| true \| false }` (`false` skips it, handy to disable an inherited feature). OCI refs only (no local paths/tarballs); ordered by `installsAfter`, baked into `devsandbox-img-<sandbox>-feat`. Feature-declared `mounts` are applied, but a sandbox `mounts` entry on the same target replaces them, and one the host or runtime can't satisfy (missing bind source, single-file bind on Apple `container`) is skipped with a warning. A feature declaring `privileged: true` runs the container `--privileged` (skipped with a warning on Apple `container`, which has no such flag). Feature `capAdd`/`entrypoint` are ignored. |
| `workspaceFolder` | Container path of the working tree. Default `/workspaces/<folder basename>`. `${…}` vars allowed. |
| `mounts` | Docker shorthand `"source=…,target=…,type=bind[,readonly]"` or `{ type = "bind", source, target, readonly }`. Default type `bind` (needs `source`). One mount per target: the last entry wins (see merge rules). Missing bind sources are auto-created: a basename with a dot after any leading dots (`creds.json`) → file, otherwise → dir (`.zshrc` becomes a *dir*). |
| `containerEnv` | `docker run -e`. Not `${…}`-substituted. |
| `remoteEnv` | Applied on every `exec` and lifecycle command. |
| `containerUser` | `docker run --user`. |
| `remoteUser` | `exec -u` for `exec`, lifecycle commands, VS Code attach. |
| `init` | `true` → `docker run --init`. |
| `privileged` | bool; `true` → `docker run --privileged`. OR-ed with any enabled feature's `privileged`, so `false` can't veto a feature that needs it. Skipped with a warning on Apple `container` (no such flag). |
| `initializeCommand` | Runs on the **host**, cwd = config dir, before anything is created. |
| `onCreateCommand`, `updateContentCommand`, `postCreateCommand`, `postStartCommand`, `postAttachCommand` | Run in that order in the container after create (as `remoteUser`, in `workspaceFolder`, with `remoteEnv`). `start` re-runs only `postStartCommand`. |
| `customizations.vscode.extensions` | [string]; installed via VS Code (workspace recommendations on Apple `container`). |

Lifecycle command forms: `"shell string"` (→ `sh -c`), `["argv", "..."]`, or a table `{ name = "cmd" | [argv] }` — the named form runs **sequentially**, in key order, not in parallel.

### Accepted but ignored (warned at `run`)

`name`, `forwardPorts`, `appPort`, `portsAttributes`, `otherPortsAttributes`, `runArgs`, `workspaceMount`, `overrideFeatureInstallOrder`, `updateRemoteUserUID`, `userEnvProbe`, `overrideCommand`, `shutdownAction`, `capAdd`, `securityOpt`, `hostRequirements`, `waitFor`, `secrets`, `customizations.vscode.settings`, any other `customizations.vscode.*` or `customizations.<tool>`.

Not accepted at all (hard error): compose keys (`dockerComposeFile`, `service`, `runServices`) — compose support was removed.

## `${…}` variables

Substituted in `mounts` (source/target), `workspaceFolder`, `worktree-branch`, `shell-rc`, and `folders` (keys and values). Unknown expressions are left verbatim.

| var | value |
|---|---|
| `${configDir}` | absolute config dir |
| `${sharedVolumes}` | `${configDir}/shared-volumes` |
| `${localWorkspaceFolder}` | absolute host `folder` (the base repo, even for worktree instances) |
| `${localWorkspaceFolderBasename}` | its basename |
| `${instance}` | persistent instance id (name at creation; stable across renames) |
| `${localEnv:VAR}` | host env var, empty if unset |

## Services (`[services.<name>]`)

Unknown keys error. No `extends` for services.

| key | type | meaning |
|---|---|---|
| `scope` | `"isolated"` (default) \| `"global"` | isolated: one container per instance on a per-instance network; global: one container per config root. |
| `image` / `build` | as sandbox | exactly one required. |
| `env` | {string = string} | `-e` vars. |
| `ports` | [string] | `docker run -p` specs (`"5432"`, `"5432:5432"`, `"127.0.0.1:5432:5432"`). A host port already published by another container errors. |
| `command` | string \| [string] | Appended after the image. A string is passed as **one** argv item, not shell-split. |

Sandboxes reach a service by its name as hostname (network alias, or `/etc/hosts` on runtimes without aliases). `devsandbox services gc` reaps unreferenced containers; `services rebuild <name>` recreates and rewires in place, without restarting the sandboxes.

## Drift

Each container is labelled with `config_hash` (hash of the merged sandbox table, or the service table) and `build_hash` (hash of the dockerfile *contents*). Any change to the merged config — including via a template — marks existing instances as drifted (`devsandbox rebuild <instance>`). Build context and feature contents are not hashed.

## Checking a config

- `devsandbox -C <dir> ls` resolves and validates every sandbox (merge +
  types) without touching docker. It does not check that `folder`, `shell-rc`
  or `folders` paths exist, or that `services` names are defined — `run` does.
- The TUI config explorer shows both the original and the resolved (merged)
  table for a sandbox.

## Example

```toml
[template.base]
init = true
remoteUser = "root"
worktree-branch = "me/${instance}"
caches = ["pnpm", "cargo"]
persist-shell-history = true
mounts = ["source=${sharedVolumes}/gh,target=/root/.config/gh,type=bind"]

[services.postgres]
image = "postgres:16"
env = { POSTGRES_PASSWORD = "postgres" }

[sandbox.web]
extends = "base"
folder = "../web"
build = { dockerfile = "dockerfiles/web.Dockerfile" }
services = ["postgres"]
containerEnv = { DATABASE_URL = "postgres://postgres:postgres@postgres/postgres" }
postCreateCommand = "pnpm install"
customizations.vscode.extensions = ["dbaeumer.vscode-eslint"]
```
