# 0015: The newest binary wins: version handoff between daemons

Status: accepted

## Context

Users upgrade the binary while a daemon runs. An old daemon serving a new
CLI would speak stale semantics; asking users to restart the daemon by hand
after every upgrade is error-prone.

## Decision

- A process's version is its semver plus its own binary's build stamp
  (mtime, `exe::own_build_stamp`), compared by `proto::Version::key`. A
  client whose `hello` is newer than the daemon makes it hand off: the old
  daemon drains and exits, the newer binary takes over.
- Spawns re-resolve the binary path every time (`exe::devsandbox`), so a
  daemon's subprocesses run the new binary at the same path; the stamp a
  process reports stays its own, so an old process never claims to be new.
- Machine links say hello as `0.0.0`, so a local daemon never hands off a
  remote one (0020).
- The user service restarts on failure only (systemd `Restart=on-failure`,
  launchd `KeepAlive {SuccessfulExit false}`, `serve::install`): a handoff
  exits 0 and must not respawn the stale binary.

## Consequences

- An upgrade takes effect at the next command, with no manual restart.
- Anything the daemon holds must survive a handoff via files (0001); in-flight
  verbs are adopted by the successor (0016).

Source: docs/serve.md, "Version handoff", "Boot start"; docs/before-machines.md, "Decisions" (Upgrades).
