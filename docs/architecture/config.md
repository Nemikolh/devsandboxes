# Config internals

How a config root's `devsandboxes.toml` is found, merged, validated and
hashed. Read before changing the schema, `extends`, `${…}` variables or drift
hashing. The user-facing field reference is
[config-toml-spec](../../skills/config-toml-spec/SKILL.md); update it with any
field, merge rule or variable change. This doc does not repeat it.

Code: `src/config.rs` (`find`, `migrate_legacy`, `Config::load`,
`Config::resolve_sandbox`, `resolve_extends`, `deep_merge`,
`SandboxProperties`, `SandboxProperties::ignored`, `Service`,
`resolve_service`, `substitute`, `MountContext`, `ContainerEnv`,
`ContainerEnv::resolve`, `sandbox_hash`, `config_hash`, `build_hash`,
`sandbox_build_hash`, `short_hash`), `src/config_identity.rs`,
`src/config_identity/ops.rs`, `src/commands/run/facts.rs`,
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
rebuild and `rename`), `localEnv:VAR` (unset → empty), and
`containerEnv:NAME` through `MountContext::container_env` (unset → empty).
Unknown expressions are left verbatim (feature mounts using one are skipped
with a warning), and so is `containerEnv:` when the context has no
`ContainerEnv` (`snapshot::validate_sandbox`, `ls`: no image, not an issue).

Expanded at `run`, not at load, in: `mounts` source/target, `folders` keys
and paths, `shell-rc` paths, `workspaceFolder`, the `worktree-branch`
pattern (a dispatcher's `--branch` is taken literally, never expanded, so it
can't read host env), and `containerEnv` / `remoteEnv` values. Other fields
are not substituted.

`ContainerEnv` exists once the image is built: `materialize` builds the
image (`image_for`), reads its facts (`run::facts::image_facts`), then
resolves `ContainerEnv::resolve` before the mounts, so `mounts`,
`shell-rc`, feature mounts, `containerEnv` and `remoteEnv` see it;
`workspaceFolder` and `folders` resolve earlier and leave it verbatim. A
name resolves to the sandbox's `containerEnv` (substituted), else the
image's `ENV`, else for `HOME` the home of the account the container runs
as (`run::facts::home`, `identity::container_home`;
[identity](instances/identity.md#containerenvhome)). Inside `containerEnv`
values it reads only the image's `ENV` and that home
(`ContainerEnv::resolve`), as devcontainer's does: `PATH =
"${containerEnv:PATH}:…"` extends the image's `PATH`, and no order between
keys matters. `Instance::remote_env`
holds the substituted `remoteEnv`.

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

## Identity findings

`config_identity::identity_findings` lists what in a config breaks under
`identity = "host"`: `/root` mount targets (`RootTarget`), env values
(`RootEnv`) and workspace paths (`RootPath`), and a winning
`remoteUser`/`containerUser = "root"` (`RootUser`). Each `Finding` names
the template or sandbox table that defines it and the sandboxes it
reaches; its id is `short_hash` of table, key and current value, so an
edited value is a new finding.

`config_identity::warning` is `run`'s one-line warning for a sandbox: its
findings not acked, counted, pointing at the prompt `run`, `start` and
`rebuild` offer on a terminal
([verbs](daemon/verbs.md#the-identity-prompt)).

`config_identity::rewrite` / `keep_root` edit the file's text on a
`toml_edit` document (comments and layout kept): `/root` becomes
`${containerEnv:HOME}`, a root user is removed, or kept with
`identity = "image"`. `migrate_file` applies them to the config file
(a `.bak` copy, tmp + rename, reread and checked; the `.bak` is removed
once the check passes and left only when something fails; refused for
YAML and for a file changed since its `file_hash`). The rewrite changes the sandboxes' `sandbox_hash`, so their
instances drift; `config_identity::ops::drifted` names them from the hash
before and after, without the runtime.

"Continue as is" is an ack: `State::identity_acks`, project id
(`services::project_id`) to finding ids, kept out of the config so it
doesn't change the hash. `config_identity::ops::ack` prunes ids no longer
among the root's findings on every write. The ops (`findings`, `migrate`,
`ack`) are the API's `config.*` methods ([api.md](../reference/api.md),
*Config*), and what the CLI calls with no daemon.

## daemon.config.toml

`<data-dir>/devsandbox/daemon.config.toml` is per user, not per root: settings
for `devsandbox serve` (`[serve] keep-alive`; `[serve] hub`, a
[fleet hub](daemon/fleet.md#hub-mode), which implies keep-alive). Unknown keys are ignored so a
newer binary's file still loads. See [daemon](daemon/).

Limitations: [status.md](../status.md#config)
