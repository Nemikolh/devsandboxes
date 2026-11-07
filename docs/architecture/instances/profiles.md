# Profiles

Named collections of sandboxes: a config declares profiles under
`[profiles.*]`, and each sandbox lists the ones its instances may be in.
Each instance is in one of its sandbox's profiles, chosen at `run`.
Read before touching profile declaration, membership or an instance's
profile. Field reference:
[config-toml-spec](../../../skills/config-toml-spec/SKILL.md).

Code: `src/config.rs` (`Config::profiles`, `Profile`, `check_profile_name`,
`Config::validate`, `SandboxProperties::profiles`, `Config::resolve_sandbox`,
`Config::allowed_profiles`, `sandbox_hash`); `src/state.rs`
(`Instance::profile`, `State::last_profile`), `src/commands/run/profile.rs`
(`choose_profile`, `profile_note`), `src/commands/run/mod.rs` (`run`,
`materialize`, `RunExtras::profile`), `src/commands/dispatch.rs`
(`handle_with`, `ensure_args`); shown by `src/snapshot.rs`
(`InstanceRow::profile`, `profile_disallowed`, `allowed_now`),
`src/commands/ps.rs` (`PsRow`, `join_state`, `cells`),
`src/commands/inspect.rs` (`profile_line`) and `src/tui/ui.rs`
(`instance_tree_row`, `profile_detail_line`); used to open URLs by
`src/open.rs` (`HostProfile`, `parse_profiles`, `load_profiles`,
`open_argv`, `open_plan`, `open_on_desktop`).

## Declaration

`Config::profiles` maps each `[profiles.<name>]` table to a `Profile`. The
struct has no fields and `deny_unknown_fields`, so any key inside a
profile table is a parse error. `Config::validate`, run by both
`Config::parse` and `Config::parse_yaml`, checks every name with
`check_profile_name`: `[a-z0-9][a-z0-9_.-]*`. A bad name or key fails the
whole file, so every command that loads the config reports it.

## Membership

`SandboxProperties::profiles` is the sandbox's `profiles` array, in
preference order. It goes through `extends` like any array
(`deep_merge` concatenates, base first), so a template that sets it acts as
the collection and a sandbox can add to it. `resolve_sandbox` then drops
repeated names, keeping each one's first position, and fails on a name with
no `[profiles.<name>]` (including when the config declares none). Like
`forwardPorts`, this check runs whenever a sandbox resolves, not at load.

`Config::allowed_profiles` is a resolved sandbox's set: its `profiles` if
set, else every declared profile in name order (the `BTreeMap`'s), empty
when the config declares none.

## Per instance

`Instance::profile` in `state.toml` is the instance's profile; `None` when
its sandbox allowed none at creation. `State::last_profile` maps project id
(`services::project_id`) and sandbox to the profile last chosen with
`run --profile`. `rm` never prunes it, so it outlives the sandbox's last
instance.

`run` chooses with `choose_profile`, over `Config::allowed_profiles`,
before any work: a `--profile` must be in the allowed set (else the run
fails, naming the set, or saying the sandbox has no profiles); without
one, the sandbox's `last_profile` while it is still allowed, else the first
allowed; nothing when nothing is allowed. Never a prompt. When more than
one profile was allowed, `run` prints `profile: <p> (--profile to choose)`
on stderr (`profile: <p>` after a `--profile`; `profile_note`). The choice
goes on the provisional record, and a user's `--profile` sets
`last_profile` in that same `State::update`.

`run` hands `materialize` the chosen profile as `RunExtras::profile`;
`rebuild` passes none, so `materialize` keeps the recorded one, like
`dispatcher`. A recorded profile isn't re-checked: an instance whose
profile its sandbox no longer allows keeps it, flagged where it is shown
([Shown](#shown)).

`--profile` reaches `run` from the CLI and from the API's `run` verb
(`RunParams::profile`, [verbs](../daemon/verbs.md)), local, forwarded to a
machine or placed; the dashboard's `run` passes none. No control request
(`devsbd` `ctl.rs`, `commands::dispatch`'s `Request`) carries a profile, so a
container can never choose one.

A dispatcher's new child inherits the dispatcher instance's recorded
profile: `handle_with`'s `ensure` passes it as `--profile` (`ensure_args`),
and refuses (`denied`) when the child's sandbox doesn't allow it rather
than falling back. A profile-less dispatcher passes none, so its child
gets the default rule. An inherited `--profile` doesn't set
`last_profile`: it isn't the user's choice. Existing children (`start`,
`rebuild`) keep their own.

## Shown

`snapshot::profile_disallowed` is the one rule: an instance's profile is
flagged when its sandbox's `Config::allowed_profiles`, as the config reads
now, lacks it (including when the config declares no profiles any more).
When the allowed set can't be told (the sandbox doesn't resolve, or is
gone from the config) nothing is flagged. A flagged instance keeps its
profile; nothing moves it.

- **Snapshots** (`instances.list`, `instances.watch`'s pushes,
  `status --json`): `InstanceRow::profile` and `profile_disallowed`.
  `snapshot::join` already loads the root's config; it takes each
  resolved sandbox's allowed set into its `SandboxFacts`, resolving again
  only when the config declares profiles.
- **`ps`** (and `containers.list`, which shares `ps::rows`): `PsRow`
  carries `profile` and `profile_disallowed`. Only for an instance with a
  profile, `snapshot::allowed_now` loads its `Instance::config_dir`'s
  config (once per root, best-effort: unreadable, nothing flagged). The
  table grows a PROFILE column only when some row has a profile; a
  flagged one reads `<p> (not allowed)`.
- **`inspect`**: after the browser sandbox line, on stderr,
  `profile: <p>`, with ` (no longer allowed by the config)` when flagged
  (`profile_line`); nothing for an instance without one.
- **Dashboard**: the instance's tree row shows `[<p>]` after its name,
  dim, yellow when flagged; Detail has a `profile: <p>` line, which says
  `(not allowed by the config any more)` in yellow when flagged. The
  dashboard reads the row's fields; it loads no config.

## On the opening host

What a profile does on a host is that host's own `profiles.toml`, next to
`state.toml` (`open::profiles_path`): one `[<profile>]` table per profile,
with `open`, the command that opens a URL in it, a non-empty array of
strings; the URL is appended as its last argument. Any other key, or a
top-level key, is a parse error (`open::HostProfile` has
`deny_unknown_fields`; `open::parse_profiles`).

```toml
[work]
open = ["google-chrome", "--profile-directory=Profile 2"]

[personal]
open = ["firefox"]
```

`open::open_on_desktop` reads it (`open::load_profiles`) and `state.toml` on
every open, so an edit applies without a daemon restart. A missing file is
no profiles; an unreadable or invalid one is a `serve.log` line and counts
as none. `open::open_plan` finds the instance by `instance_id` (a rename
doesn't lose it) and opens with its profile's `open` (`open::open_argv`).
No instance, no profile or a profile the file doesn't map: the host's
opener (`xdg-open`, `open` on macOS). A profile `profile_disallowed` flags
(its sandbox's allowed set loaded with `snapshot::allowed_now`) also gets
the host's opener, with a `serve.log` warning (`<instance>: profile `<p>`
no longer allowed by the config; opening <url> with the default opener`).
The display rule is the same for every command
([bridges.md](../helper/bridges.md#opening-urls-kind-open)).

Commands only ever come from the opening host's own `profiles.toml`, never
from a config: only a profile's name comes from `state.toml`, so no config
(nor, over a fleet, any machine) can make this host run a command.

## Drift

`sandbox_hash` strips `profiles`: membership never changes the container,
so editing it is not drift ([config.md](../config.md#hashing)).
