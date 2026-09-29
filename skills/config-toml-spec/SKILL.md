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

Next to it devsandbox uses: `shared-volumes/` (`${sharedVolumes}`, persistent host state), `.worktrees/<instance-id>/` (repeat-instance git worktrees), `shared-files/<sandbox>/` (`worktree-link` store). Relative paths (`folder`, `folders` values, `shell-rc`, `build.dockerfile`, `build.context`) resolve against the config dir.

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
| `worktree-branch` | string | Branch pattern for worktree instances, `${…}` vars allowed. Default `sandbox/${instance}`. `run --branch` overrides. An existing local branch is checked out as is (`worktree-base` ignored); one only on `origin` becomes a local branch tracking it; else a new branch is created. A branch checked out in another worktree is an error. |
| `worktree-base` | string | Commit-ish start point (e.g. `origin/develop`). Default: remote's default branch. `run --base` overrides. |
| `worktree-include` | [string] | Gitignore-syntax patterns: gitignored, untracked files copied from the base repo into each worktree on `run` and again on every `rebuild` (never overwriting, so a copy deleted on purpose comes back on the next `rebuild`; `start` doesn't copy). Added after the repo's own `.worktreeinclude` (always honored), so `!pat` can negate it. See `docs/worktreeinclude.md`. |
| `worktree-link` | [string] | Gitignored paths from the repo root (`.env` = root only; leading `./` ok) shared live by all instances. `*`/`?` glob within one segment (`packages/*/.env`), matched against existing paths in the base repo and store; no `**`/`[...]`. For each: the base repo's file is moved to `shared-files/<sandbox>/<path>` and each tree gets an absolute symlink; that dir is mounted at its host path (works on Apple `container`). Tracked/non-ignored paths and existing conflicting files are skipped with a warning; `..`/absolute/`.git` entries error. Store kept on `rm`. |
| `shell-rc` | [string] | Host shell snippets sourced by `~/.zshrc`/`~/.bashrc`. Must exist and be files (no auto-create). Parent dir mounted read-only at `/devsandbox/rc/<i>`. |
| `folders` | {container path = host dir} | Extra VS Code workspace roots. Key must be absolute and differ from `workspaceFolder`; value must exist. |
| `autostart` | `true` \| `false` \| `"runtime"` | Once per host boot (per config root), the first `devsandbox run` / `start` or TUI launch starts this sandbox's stopped instances (full `start` path), or `run`s one if it has none; dispatcher children are never started and count as existing (a sandbox with only children gets no `run`); containerless instances are skipped with a note; while the runtime is unreachable the pass is skipped and retried on the next trigger. Any other value errors. `"runtime"` does the same and also creates the container with `--restart unless-stopped` on docker/podman (podman restarts at boot only with `podman-restart.service` enabled; devsandbox never installs it), so the runtime restarts the container itself. An in-container boot hook then brings back the helper daemon and re-runs `postStartCommand` (as `remoteUser`, in `workspaceFolder`, with `remoteEnv`; output appended to `/run/devsandbox/boot.log`) on every container start, so `devsandbox start` doesn't run it from the host; `run` still does at create. Host-side bridges/forwards resume only when a devsandbox process connects. Containers created before the hook keep the old behavior and warn on `start`: recreate with `devsandbox rebuild --force <instance>`. Apple `container` has no restart policy: warning, behaves as `true`. Not hashed for drift: `start` updates an existing container's restart policy in place. |
| `dispatcher` | `{ spawn = [string], max-instances = int }` | Makes this sandbox a dispatcher: its instances may create/start/stop/remove *child* instances through the in-container control API (`devsbd ensure|ls|stop|rm|exec|run …`, served only while the dashboard is open; see docs/automations-guide.md). `spawn`: sandboxes of this config root it may instantiate, `"*"` = every non-dispatcher sandbox of this root (granting the dispatcher their mounts, docker socket and other privileges); a sandbox that declares `dispatcher` is never spawnable, even when named (children can't be dispatchers; a request from a child is denied too); each other entry must name an existing sandbox (`devsandbox ls` errors otherwise); omitted = empty (nothing may be spawned). Arrays concatenate under `extends` like any other. `max-instances` (optional, default 10): cap on the dispatcher's children recorded in state (stopped ones count). A child's `--env K=V` (control API) may not name `PATH`, `HOME`, `SHELL`, `USER`, `ENV`, `BASH_ENV`, `IFS`, `CDPATH`, `PS4`, `PROMPT_COMMAND`, `SSH_AUTH_SOCK`, `TMPDIR`, `GCONV_PATH`, `NODE_OPTIONS`, `RUBYOPT` or anything starting `LD_`, `DYLD_`, `GIT_`, `PYTHON`, `PERL5` (case-insensitive; denied). Unknown keys error (e.g. `max_instances`). Children are ordinary instances named `<sandbox>-<key>` (key `[a-z0-9][a-z0-9-]{0,39}`), always on their own git worktree, labelled `devsandbox.dispatcher=<dispatcher instance id>` and recorded with that owner in `state.toml`; a dispatcher only reaches its own children. Children are kept when the dispatcher is stopped/removed/rebuilt. Re-read from config on every request and not hashed for drift. |

### Implemented devcontainer properties

| key | notes |
|---|---|
| `image` | Base image. Exactly one of `image` / `build` is needed. |
| `build` | `{ dockerfile (required), context = ".", args = {}, target, cacheFrom = str\|[str], options = [str] }`. Tag `devsandbox-img-<sandbox>`. `cacheFrom` ignored with a warning on runtimes without support. |
| `features` | `{ "<oci-ref>" = { opts } \| "version" \| true \| false }` (`false` skips it, handy to disable an inherited feature). OCI refs only (no local paths/tarballs); ordered by `installsAfter`, baked into `devsandbox-img-<sandbox>-feat`. Feature-declared `mounts` are applied, but a sandbox `mounts` entry on the same target replaces them, and one the host or runtime can't satisfy (missing bind source, single-file bind on Apple `container`) is skipped with a warning, as is one still holding an unsupported `${…}` variable. A feature declaring `privileged: true` runs the container `--privileged` (skipped with a warning on Apple `container`, which has no such flag). Feature `entrypoint`s run at every container start, in install order, before the image's own `ENTRYPOINT` (a failing one warns in the logs and the rest continue; see `docs/entrypoint.md`). Feature `capAdd` is ignored. |
| `workspaceFolder` | Container path of the working tree. Default `/workspaces/<folder basename>`. `${…}` vars allowed. |
| `mounts` | Docker shorthand `"source=…,target=…,type=bind[,readonly]"` or `{ type = "bind", source, target, readonly }`. Default type `bind` (needs `source`). One mount per target: the last entry wins (see merge rules). Missing bind sources are auto-created: a basename with a dot after any leading dots (`creds.json`) → file, otherwise → dir (`.zshrc` becomes a *dir*). A `volume` whose source uses `${instance}` / `${devcontainerId}` is per-instance: `rm` deletes it (`rebuild` keeps it); other named volumes are shared and never deleted. |
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
| `${devcontainerId}` | same as `${instance}` (devcontainer's stable per-container id; features name volumes with it) |
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

Each container is labelled with `config_hash` (hash of the merged sandbox table, or the service table) and `build_hash` (hash of the dockerfile *contents*). Any change to the merged config — including via a template — marks existing instances as drifted (`devsandbox rebuild <instance>`), except `autostart` and `dispatcher`, which are stripped before hashing. Build context and feature contents are not hashed.

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
