> Archived 2026-11-22. Historical, may be wrong; do not use as a reference.
> Current: [daemon/updates.md](../../architecture/daemon/updates.md),
> [update CLI](../../reference/update-cli.md),
> [images and features](../../architecture/instances/images-and-features.md),
> [ADR 0059](../../decisions/0059-feature-updates-run-in-the-features-image.md)

# Feature updates

Status: agreed; building.

## Goal

A feature's tool (gh first) stays current in every sandbox with zero setup
from the user, without a sandbox ever being able to change the binary the
others run.

Today the gh feature asks the user to add a read-only mount to their config
and run `curl … gh-update.sh` on the host by hand. Instead:

- the feature itself declares the shared dir (a read-only `${sharedVolumes}`
  mount, which devsandbox already creates and applies for feature mounts)
  and an update command baked in its image;
- devsandbox's host daemon runs that command on a schedule (default: every
  7 days, checked when the daemon starts and then hourly), in a throwaway
  container from a sandbox image built with the feature, with that one dir
  mounted read-write and nothing else;
- the config can change the interval per update, or turn it off.

Trust: the update command is the feature's own code, run where its
`install.sh` already runs (a container from its image), never on the host.
Sandboxes mount the dir read-only, so none of them can swap the binary.

## Step 1: parse feature updates and the config's intervals

- Feature metadata: `customizations.devsandbox.updates`, a list of
  `{ "name", "mount", "command", "every" }` (`name`: `[a-z0-9][a-z0-9-]*`,
  unique across features; `mount`: the target of one of the feature's own
  `mounts`, which must be a `${sharedVolumes}` bind; `command`: argv run in
  the image; `every`: optional, default `7d`). Parsed next to the claims in
  `FeatureMetadata::parse` (`src/features.rs`, `claim::from_customizations`
  is the model: read `/devsandbox/updates`, refuse unknown fields inside).
- Config: a top-level `updates` table on `Config` (`src/config.rs`), keyed
  by update name: `gh = "1d"` or `"never"`. A small duration parser (`<n>h`,
  `<n>d`, `<n>w`; no new dependency).
- Tests: parse and refusals; the duration parser; TOML and YAML.
- Docs: `docs/architecture/instances/images-and-features.md`,
  `skills/config-toml-spec/SKILL.md` (the `updates` table).

## Step 2: run an update now, `devsandbox update <name> [args…]`

- `src/commands/update.rs`: for the current config root, pick the sandboxes
  whose resolved `features` include the feature declaring `<name>`, and
  among their features images that exist locally
  (`image::features_image`, `Backend::image_id`) the newest. None: refuse,
  naming `devsandbox run` (the image builds with the sandbox).
- The container: a pure argv builder modelled on `dockerlike.rs`'s
  `canary_argv`: `run --rm --pull=never`, the sandbox's identity plan
  (`run/identity.rs` `plan_for` + `create_args`: user, userns, seccomp,
  relabel), the resolved host dir at `mount` read-write as the only bind,
  default network, entrypoint `command[0]`, args `command[1..]` + the CLI's
  extra args (a version, a rollback). The argv guards `run_container` runs
  (`key_guard::check_argv`, `binds::check_argv`,
  `worker_policy::enforce_argv`) run on it too.
- Output streams to the terminal; the time of a successful run goes to
  `State` (new `updates: BTreeMap<project, BTreeMap<name, u64>>`, written
  with `State::update`).
- Tests: the argv (pure), image choice (pure over a list), a
  `#[test_utils::docker_test]` running a `sh` command from a small image
  that writes into the dir.
- Docs: `docs/reference/` CLI page for the verb, images-and-features.md.

## Step 3: the daemon runs them on schedule

- A `serve-updates` thread started in `Host::start` (`src/serve/host.rs`,
  next to `serve-autostart`): at start, then hourly, for each config root
  (`config_roots`), each declared update whose last success is older than
  its interval (config override, else the feature's `every`) runs as
  `devsandbox --in-process update <name>` in that root, a logged subprocess
  like the dispatcher's (`commands/dispatch.rs` `Subprocess`, `bounded`
  with a timeout), one at a time. The slot is claimed in `State` before it
  runs (as `autostart` claims its boot id), so two daemons never race.
- A failure keeps the last success and retries after 6 h; it's a line in
  the daemon log and an Inbox notice once per failure streak.
- Holds the daemon from idling out while one runs (`Host.autostarting`'s
  pattern).
- Tests: `due` as a pure fn over times and intervals; the pass with an
  injected executor.
- Docs: `docs/architecture/daemon/` (a page for updates, linked from the
  daemon README), `docs/status.md`.

## Step 4 (my addition): approve a changed claim from `run`

Today a feature release that changes its claim makes `run` refuse until
`devsandbox access approve <credential>`. On a TTY, `run` should show the
claim's diff and ask, approving in place (`run/claims.rs`
`check_credentials`; the TTY prompt path of `verbs_client`); off a TTY it
refuses as today.

- Docs: `docs/reference/credentials-cli.md`, images-and-features.md.

## Step 5: the gh feature uses it (github.com/devsandboxes/features)

- `devcontainer-feature.json`: the read-only mount
  `${sharedVolumes}/gh-bin` → `/var/lib/devsandbox/gh/bin`, and
  `updates: [{ "name": "gh", "mount": "/var/lib/devsandbox/gh/bin",
  "command": ["/usr/local/lib/gh/gh-update", "/var/lib/devsandbox/gh/bin"] }]`;
  `install.sh` installs `gh-update` there; the version bumped.
- README rewritten short: what it does, the config snippet, login (one
  command per token kind), what the permissions table is for (the PAT's
  permissions, each with the gh commands needing it), the repo fence as
  bullets, updates (automatic, the `updates.gh` interval, `devsandbox
  update gh 2.104.0` to pin or roll back). No git, ssh-rewrite or
  `access.log` details.
- Commit and push.

## Closing

Living docs checked against the code, ADR for "feature updates run in the
feature's image, never on the host", plan archived.
