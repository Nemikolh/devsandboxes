# 0002: Config is the security boundary; mount sources come only from config

Status: accepted

## Context

Agents drive devsandbox: from the host CLI, and from inside containers
through the dispatcher control API. If any of those paths could name a host
path to mount, an agent could mount `~/.ssh` or `/` into a sandbox and the
sandbox would protect nothing.

## Decision

Host mount sources only come from the config file (`devsandboxes.toml`)
the user wrote: `caches`, shell history, and declared `mounts`, expanded
from a fixed set of `${…}` variables. No CLI flag, API parameter or control
op adds a mount: `run`'s per-invocation extras (`RunExtras`) are env and
dispatcher ownership only, and the dispatcher's `--env` names are filtered
(`control::denied_env`). Control requests are re-authorized against the
config as it is now (see 0030).

## Consequences

- An agent with CLI access gets start/exec/stop, not arbitrary host access.
- Any feature that wants a new host path in a container must go through a
  config field, never a flag; reviewing the config is reviewing the attack
  surface.
- Users who want an ad-hoc mount edit the config (which also changes the
  instance's `config_hash` and marks it drifted, see 0009).

Source: docs/high-level-architecture.md, "Principles" ("Config is the security boundary"); docs/devsandboxes.md, goals.
