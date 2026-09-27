# CLI improvements surfaced by writing `data/config.toml`

Modeling the six real devcontainer projects under `data/` against one shared
template exposed the gaps below. Ordered by how much they block the goal
(one common template that sets up the agent + persistent caches + shell history).

## Status

- **Done:** #1 mounts, #2 shell history, #4 template composition, #5 remoteUser/
  containerUser, #6 features, #8 parity items (init, workspaceFolder vars, caches).
- **Obsolete:** #3 — compose support was removed; `config.toml` is now the single
  source of truth, so there is no compose service to inject into.
- **Open (next session):** #7 runArgs + env-file/secrets.

## 1. Implement `mounts` (blocker) — DONE
Implemented: `type=bind` with `source`/`target`/`readonly`, `${localEnv:VAR}` /
`${configDir}` / `${localWorkspaceFolder(Basename)}` substitution, auto-created
bind sources (file vs dir heuristic), wired to `docker run --mount`. Original
notes below.

`mounts` is parsed but ignored (listed under "valid, not implemented"). It is
the mechanism the whole template relies on: the agent binary + credentials, the
shared pnpm/cargo caches. Minimum viable support:
- `type=bind` with `source=`/`target=` (and optional `readonly`).
- Variable substitution in `source`:
  - `${localEnv:VAR}` — host env (e.g. `${localEnv:HOME}`), devcontainer-compatible.
  - `${configDir}` — directory holding `config.toml` (new; anchors `shared-volumes`).
  - `${localWorkspaceFolder}` / `${localWorkspaceFolderBasename}` for parity.
- Auto-create missing bind `source` dirs (caches) before `docker run`, so a
  first run doesn't fail on a non-existent host path. Files vs dirs matters:
  `.zsh_history` must be a file, `pnpm-store` a dir.
- Merge, don't replace: array fields like `mounts` from a template should
  concatenate with the sandbox's, not be overwritten (see #4).

## 2. Managed per-instance shell history (feature) — DONE
Implemented via a `persist-shell-history` knob: a per-instance directory
`${configDir}/shared-volumes/history/<instance>/` is provisioned, bind-mounted
to `/commandhistory`, and `HISTFILE=/commandhistory/.zsh_history` is set on the
container. A directory (not a file) is mounted because Apple's `container`
cannot bind-mount a single file. The file path is recorded in state and kept on
`rm` so a rebuilt same-name instance inherits its history. Orphaned dirs are
reaped by `gc` (confirmed per entry, or unconditionally with `--force`).
Original notes below.

The user wants `.zsh_history` persisted but unique per sandbox. Since instances
already have deterministic names, devsandbox can own this end to end:
- On run, provision `${configDir}/shared-volumes/history/<instance>.zsh_history`
  (touch the file) and bind-mount it to `/root/.zsh_history`.
- Key on instance name so worktree instances (`repo-2`, …) get their own file.
- Gate with a `persist-shell-history` knob on the sandbox/template (a real,
  known field — `deny_unknown_fields` rejects unknown keys today, so this must
  be added to `SandboxProperties`).
- `rm` should delete the instance's history file. (Revised: it is kept, so
  history survives rm + run rebuilds.)

## 3. `mounts`/env for compose sandboxes — OBSOLETE
Resolved by removal: `dockerComposeFile`/`service`/`runServices` and the compose
code path are gone. The compose projects are modeled with `image`/`build` +
shared `services`, so there is no compose service for the template to reach.
Original notes below.

Two projects were compose devcontainers whose agent mounts lived
in `docker-compose.yml`. The template's `mounts`/`containerEnv` never reach a
compose service today. Either:
- inject them via the generated compose override (we already write one for the
  network), or
- keep steering people toward `image`/`build` + shared `services` (what the
  config now does) and document that compose sandboxes opt out of the template.

## 4. Template composition — DONE
Implemented: `extends` accepts a list (`["node", "rust"]`, merged left-to-right)
and resolves recursively (templates extending templates) with cycle detection.
Table merges recurse and array merges concatenate, so a sandbox adds without
restating the template. Caveat: a diamond (two bases sharing an ancestor)
duplicates concatenated array entries; not deduped. Original notes below.

Single-level, single-`extends` forces one fat `base` template. Real projects are
node **and** rust (webcontainer) or go **and** rust (goj). Two options:
- allow `extends = ["node", "rust"]` (list), merged left-to-right; and/or
- allow templates to `extends` other templates (recursive resolve).
Also make table/array merges additive for `mounts`, `containerEnv`, `extensions`
so a sandbox can add without restating the template's values.

## 5. `remoteUser` (and `containerUser`) — DONE
Implemented: `remoteUser` → `docker exec -u` for lifecycle commands and `exec`
(stored on the instance); `containerUser` → `docker run --user`. Targets in #8's
caches are still `/root`-based; revisit if a non-root `remoteUser` home is needed.
Original notes below.

Every project sets `remoteUser` (mostly `root`). It's ignored, so `exec` and
lifecycle commands run as the image default. Apply it as `docker exec -u` (and
`--user` on run where appropriate).

## 6. `features` — DONE
Implemented natively: `features` is a map of OCI feature ref → options (an
options table, a bare version string, or `true`). Features are fetched over the
network with `curl` and unpacked with `tar` (no external CLI), cached under
`$XDG_CACHE_HOME/devsandbox/features`, ordered by their `installsAfter`
metadata, and baked into a derived image `devsandbox-img-<sandbox>-feat` built
from a generated Dockerfile — the layout mirrors the official devcontainer CLI.
Feature options are passed as env files and feature `containerEnv` becomes `ENV`.

v1 limitations:
- OCI refs only — no local paths or tarballs.
- Feature `capAdd` and `entrypoint` are ignored (`mounts` and `privileged` are
  applied; `privileged` is skipped with a warning on Apple `container`).
- `_REMOTE_USER` defaults to `root` unless `remoteUser`/`containerUser` is set.
- Cached tags don't auto-refresh; clear `$XDG_CACHE_HOME/devsandbox/features`
  to force a re-fetch.
- Ordering honors `installsAfter` only (no `dependsOn` /
  `overrideFeatureInstallOrder`).

Original notes below.

`goj`, `wasm-typescript` and others use devcontainer features
(go, rust, node, common-utils/zsh). Unsupported, so today they must be baked
into images or `postCreateCommand`. Full feature support is a large, separate
effort — worth a decision: support the common ones, or declare features
out of scope and lean on prebuilt images.

## 7. `runArgs` + env-file — OPEN (next session)
`runArgs` is still parsed-and-ignored (surfaced by `ignored()`).
`webcontainer` needs `--env-file .devcontainer/devcontainer.env` and
`--add-host=host.docker.internal:host-gateway`. Supporting `runArgs`
pass-through (or a first-class `envFile`) covers it. Related: **secrets** —
`data/webcontainer/devcontainer.env` has committed live tokens; devsandbox
should encourage host-only env files and never copy them into images.

## 8. Smaller parity items — DONE
- `init: true` → `docker run --init`. Done.
- `${localWorkspaceFolderBasename}` in `workspaceFolder` — done (same
  substitution engine as mounts).
- `cache-folder` was removed and replaced by a `caches` array (`pnpm, cargo,
  npm, yarn, go, pip`): each expands into a shared bind mount under
  `${configDir}/shared-volumes/<name>` plus the env vars that point the tool at
  it. Unknown names error.
