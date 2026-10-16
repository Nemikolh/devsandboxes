# Autostart

Sandboxes that come up on their own after a host boot: the `autostart`
field, the once-per-boot pass that starts or creates their instances, and
the `"runtime"` restart policy. Read before touching `autostart`, `start`'s
restart-policy/boot-hook handling, or the daemon's startup pass. Field
syntax: [config-toml-spec](../../../skills/config-toml-spec/SKILL.md).

Code: `src/commands/autostart.rs` (`autostart`, `due`, `actions`,
`restart_decision`, `runtime_restarts`, `restart_update`,
`apply_restart_policy`, `boot_id`), `src/config.rs` (`Autostart`),
`src/state.rs` (`State.autostart_boot`), `src/serve/host.rs`
(`autostart_all`, `config_roots`), `src/main.rs` (`Run`/`Start`/`Rebuild`
arms), `src/commands/start.rs` (`start_logged`, `post_start_runner`),
`src/commands/run/mod.rs` (`boot_spec_for`, `BOOT_HOOK_LABEL`),
`src/devsbd.rs` (`sync_boot`).

## Modes

`Autostart` parses `false` → `Off` (default), `true` → `Devsandbox`,
`"runtime"` → `Runtime`; any other value is a config error. Both `true` and
`"runtime"` get the devsandbox-driven pass; `"runtime"` additionally sets a
runtime restart policy where the backend has one. `autostart` (like
`dispatcher`, `inbox`, `forwardPorts`) is stripped before `config_hash`, so
changing it is never drift.

## The pass

`autostart(dir)` for one config root:

1. No loadable config, no boot id, or no project id → silent no-op.
2. `due`: skip unless `State.autostart_boot[project_id]` differs from the
   current boot id.
3. No sandbox with `autostart` on → return *without* recording, so adding
   `autostart` later this boot still takes effect. Sandboxes that fail to
   resolve are skipped (the command that uses them reports it).
4. Runtime unreachable (`backend().list` fails: Docker Desktop not up, Apple
   `container system start` not run) → warning, not recorded, retried on
   the next trigger.
5. Claim: under the state lock (`State::update`) re-check `due` and record
   the boot id. Of two racing passes only one acts. Recording comes
   *before* acting, so a start that keeps failing doesn't retrigger on every
   invocation; the next boot retries.
6. `actions`, per autostart sandbox, over this project's instances only:
   - stopped → `Start` (`start::start_instance`: the full `start` path, into
     the instance's build log);
   - running, or `Unknown` (inspect failed) → nothing;
   - container gone → `SkipMissing` note (`devsandbox run` recreates);
   - `progress` set (in flight or failed, see
     [lifecycle](../instances/lifecycle.md)) → `SkipNotReady` note, not
     queried;
   - dispatcher children (`Instance.dispatcher` set) → ignored, not queried:
     their dispatcher `ensure`s them ([dispatchers.md](dispatchers.md));
   - no instance at all (children count) → `Run`: `run::run` creates one.

Nothing is fatal: every problem is a `warning:`/`note:` on stderr.

## Triggers

- The daemon: `Host` spawns a `serve-autostart` thread at start that runs
  `autostart_all`, the pass for every config root recorded in state
  (`config_roots`). While it runs `Host::autostarting` is 1, an idle
  holder, so the daemon doesn't exit mid-`run`
  ([role](../daemon/role.md), [host poll](../daemon/host-poll.md)).
- `devsandbox run`, `start`, `rebuild`: after the verb, for `-C`'s root.
  After, not before: `run web` for an autostart `web` must not create two
  instances. Then `ensure_running` starts the daemon (unix, not
  `--in-process`), whose own pass finds this boot done.
- The dashboard runs no pass of its own; opening it connects to (and
  lazily starts) the daemon, which does. No other command triggers it.

Boot after login without any command: `devsandbox serve install` registers
the daemon with systemd/launchd, whose start runs the pass.

## Boot id

`boot_id` is cached per process. Linux: `/proc/sys/kernel/random/boot_id`.
macOS: the braced `{ sec = …, usec = … }` part of `sysctl -n kern.boottime`
(the date suffix is locale-dependent). Other OSes: `None`, so the pass never
runs. The same id stamps an instance's `progress`, so a phase recorded
before a reboot reads as stale.

## `"runtime"`: restart policy

- `run` (`restart_decision`): on backends with `supports_restart_policy`
  (docker, podman) the container is created with `--restart
  unless-stopped`, so a container the user stopped stays stopped across a
  runtime restart. Podman gets a warning: it restarts at boot only with
  `podman-restart.service`, which devsandbox never installs. Apple
  `container` has no policy: warning, behaves as `true`.
- `start` (`apply_restart_policy` → `restart_update`): brings an existing
  container's policy in line with the mode (`unless-stopped` or `no`; empty
  counts as `no`), in place, since `autostart` isn't drift. A failing
  `update --restart` (old podman) warns and suggests `rebuild --force`.

## `"runtime"`: boot hook

The runtime restarts only the container; nothing devsandbox `exec`s comes
back. So when `runtime_restarts` (mode `Runtime` and backend support) and
the helper is installed, `boot_spec_for` yields a boot file that
`sync_boot` writes on `run`/`start`/`rebuild`; the container command's
`devsbd boot` restarts the helper daemon and runs `postStartCommand` from it
on every container start ([in-container](../helper/in-container.md#devsbd-boot)).
On `start`, `post_start_runner` then skips the host's own `postStartCommand`
(`Hook`); a container without `BOOT_HOOK_LABEL` (created before the hook)
runs it from the host with a warning to `rebuild --force`
(`HostPredatesHook`). Host-side bridges, forwards and control resume only
once the daemon runs.

Limitations: [status.md](../../status.md#automations)
