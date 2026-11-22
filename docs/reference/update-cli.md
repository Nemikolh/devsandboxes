# `devsandbox update`

Run a feature's update now: the command a feature bakes in its image to
refresh a `${sharedVolumes}` dir its sandboxes mount read-only (a tool's
binary, e.g. gh's). Read when scripting updates or changing the verb; what
a feature declares is in
[images-and-features](../architecture/instances/images-and-features.md#running-an-update).

Code: `src/main.rs` (`Command::Update`), `src/commands/update.rs`
(`update`, `candidate`, `newest`, `host_dir`, `update_argv`),
`src/updates.rs`.

```
devsandbox [-C <dir>] update <name> [args…]
```

`<name>` is the update's name as its feature declares it
(`updates::check_name`: `[a-z0-9][a-z0-9-]*`, at most 32 bytes). `args`
are appended to the update's `command` (a version, to pin or roll back:
`devsandbox update gh 2.60.0`); anything after `<name>` goes there,
flags included.

## Which image

For the config at `-C` (`update` → `candidate`), each sandbox's enabled
`features` are resolved (fetched into the cache like at `run`); the ones
whose feature declares `<name>` are candidates. A sandbox whose config or
features don't resolve, or whose features declare one update name twice
(`updates::check_unique`), is skipped with a ``warning: sandbox `<name>`:
…`` line. No candidate: ``no sandbox of this config has a feature
declaring update `<name>` ``.

Among the candidates' features images (`run::features_image`) that exist
locally (`Backend::image_id`), the newest by `Backend::image_created`
runs it (`newest`; an unknown date ranks below any known one, a tie goes
to the first sandbox in name order). None built: ``update `<name>`: no
image of sandbox `a`, `b` is built yet; `devsandbox run <sandbox>` builds
it``. The update's `mount` and `command` are read from that sandbox's
feature.

## The container

`host_dir` resolves the source of the feature's mount at the update's
`mount` with `${sharedVolumes}` = `<config root>/shared-volumes` and no
instance (a source naming `${instance}` / `${devcontainerId}` is refused:
every instance shares the dir), checks it lies under `shared-volumes/`
(`run::creatable_feature_source`), and creates it when missing, past a
worker policy's check (`run::guard_create`, `run::provision_bind_source`).

`update_argv`: `run --rm --pull=never`, the sandbox's identity plan
(`run::plan_for` with that image's facts, never privileged, then
`run::create_args`: the identity label, `--user`, `--userns`, the seccomp
profile, the dir's `--mount` read-write and relabeled per the plan), the
default network, `--entrypoint <command[0]>` (so the image's entrypoint
chain doesn't run), the image, `command[1..]`, then `args`. The argv
passes the guards a sandbox's `run` does (`key_guard::check_argv`,
`run::check_bind_argv` with no instance of its own,
`worker_policy::enforce_argv`). Its output goes to the terminal.

On exit 0 the time goes to `state.toml` as
`State.updates[<project id>][<name>]` (unix seconds, `State::update`);
any other exit is ``update `<name>` failed (exit N)`` and records
nothing. The config's `updates` table isn't read here.

`update` runs against this host's config only: `--machine` refuses it
(`machines::cli::machine_use`), and so does Apple `container`.
