# Config internals

How a config root's `devsandboxes.toml` is found, merged, validated and
hashed. Read before changing the schema, `extends`, `${…}` variables or drift
hashing. The user-facing field reference is
[config-toml-spec](../../skills/config-toml-spec/SKILL.md); update it with any
field, merge rule or variable change. This doc does not repeat it.

Code: `src/config.rs` (`find`, `migrate_legacy`, `Config::load`,
`Config::resolve_sandbox`, `resolve_extends`, `deep_merge`,
`SandboxProperties`, `SandboxProperties::ignored`, `Service`,
`resolve_service`, `substitute`, `MountContext`, `sandbox_hash`,
`config_hash`, `build_hash`, `sandbox_build_hash`, `short_hash`),
`src/commands/services.rs` (`project_id`), `src/daemon_config.rs`.

## Config root

The config root is the `-C <dir>` directory (global flag, default `.`). It
holds the config file and, next to it, `dockerfiles/`, `shared-volumes/` and
`.worktrees/` (see [mounts and state](instances/mounts-and-state.md)).

`config::find` accepts exactly one of `devsandboxes.toml`,
`devsandboxes.yaml`, `devsandboxes.yml`; several is an error, not a
precedence pick. A legacy `config.toml` is read only when none exist and it
has a `sandbox`/`template`/`services` table; `main` renames it to
`devsandboxes.toml` at startup (`migrate_legacy`, not in `Config::load`,
which also runs on TUI threads). `run` with no sandboxes offers to write an
example config.

YAML parses (`serde_saphyr`) into the same `toml::Table`s, so merging,
validation and hashes are identical across formats.

`project_id(dir)`: first 8 hex chars of `short_hash` (FNV-1a) of the
canonical config root. It namespaces networks and global services (see
[services](instances/services.md)) and keys `State.autostart_boot`.
Instance state itself is global, not per root (`state.toml`; each instance
records its `config_dir`).

## Three tables

`Config` keeps `[template.*]`, `[sandbox.*]` and `[services.*]` as raw
`toml::Table`s so `extends` can merge them untyped; typing happens after.

- **Sandboxes**: `resolve_sandbox` merges, drops `extends`, hashes, then
  deserializes into `SandboxProperties` (`deny_unknown_fields`: unknown keys
  are hard errors). `SandboxProperties` is the devcontainer.json schema plus
  devsandbox extras. Valid devcontainer properties devsandbox doesn't act on
  still parse; `ignored()` lists them and `run` warns before creating the
  container. `SandboxProperties::identity` folds devcontainer's
  `updateRemoteUserUID` into the `identity` field (`false` means `image`
  when `identity` is unset). `forwardPorts` entries are deduplicated after merge and each
  named service must be in the sandbox's `services`.
- **Templates** are never validated on their own, only through the
  sandboxes that extend them.
- **Services** have no `extends`. `resolve_service` hashes the raw table,
  deserializes into `Service` (`deny_unknown_fields`), and requires exactly
  one of `image` / `build`.
- Cross-sandbox check: `validate_dispatcher` (every `dispatcher.spawn` entry
  names a sandbox of this root, or `"*"`); run by `resolve_all`
  (`devsandbox ls`).

## extends and deep_merge

`resolve_extends`: `extends` is a template name or an array of names
(anything else errors). Listed templates resolve recursively, merge
left-to-right, then the table's own body merges on top. A resolution stack
detects cycles (`cyclic \`extends\`: a -> b -> a`); unknown names error.

`deep_merge(base, over)`:

- tables recurse;
- **arrays concatenate**, base first. A sandbox adds to a template's
  `mounts`, `services`, `caches`, `shell-rc`, extensions without restating
  them; this is how services declared in a shared template (e.g.
  `template.base`) reach every sandbox that extends it. Nothing removes an
  inherited entry; a diamond duplicates entries.
- scalars (and type mismatches): `over` wins.

Overriding an inherited mount happens later, at `run`: mounts collapse to the
last entry per target (`run::mounts::last_wins_by_target`).

## Variables

`substitute` expands devcontainer-style `${…}` from a `MountContext`:
`configDir`, `localWorkspaceFolder`, `localWorkspaceFolderBasename`,
`sharedVolumes` (`<configDir>/shared-volumes`), `instance` and its alias
`devcontainerId` (the instance id: the name at creation, stable across
rebuild and `rename`), and `localEnv:VAR` (unset → empty). Unknown
expressions are left verbatim (feature mounts using one are skipped with a
warning).

Expanded at `run`, not at load, in: `mounts` source/target, `folders` keys
and paths, `shell-rc` paths, `workspaceFolder`, and the `worktree-branch`
pattern (a dispatcher's `--branch` is taken literally, never expanded, so it
can't read host env). Other fields are not substituted.

A volume mount whose source uses `${instance}` / `${devcontainerId}` is
`per_instance`: recorded in state and deleted by `rm`.

## Hashing

Drift compares labels written at container creation against the current
config ([rebuild and drift](instances/rebuild-and-drift.md)):

- `sandbox_hash`: `short_hash` of the merged sandbox table's TOML (a
  `BTreeMap`, so key-sorted and stable) minus `autostart`, `dispatcher`,
  `inbox` and `forwardPorts` (they apply without recreating the container).
  `folders` entries that only restate the default (`{ path = "x" }`,
  `worktree = "auto"`) are normalized to `"x"` first.
- Service `config_hash`: the raw `[services.<name>]` table.
- `build_hash`: the build dockerfile's contents (not its path, which the
  config hash covers; not the build context). `sandbox_build_hash` folds in
  the contents of enabled local-folder features
  (`features::local_features_fingerprint`); OCI feature contents are not
  hashed.

`sandbox_text` / `resolved_table` give the dashboard's config view and the
daemon's `sandboxes.get` the original table, the merged one and its hash.

## daemon.config.toml

`<data-dir>/devsandbox/daemon.config.toml` is per user, not per root: settings
for `devsandbox serve` (`[serve] keep-alive`). Unknown keys are ignored so a
newer binary's file still loads. See [daemon](daemon/).

Limitations: [status.md](../status.md#config)
