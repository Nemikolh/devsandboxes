# 0029: `autostart = true` runs once per boot, keyed on the host boot id

Status: accepted

## Context

Automation sandboxes (dispatchers, triage agents) must come back after a
reboot. Runtime restart policies alone don't cover it: Apple `container`
has none, podman needs `podman-restart.service`, and a bare container
restart skips devsandbox's own start steps (helper install,
`postStartCommand`, bridges).

## Decision

- `autostart = true`: devsandbox starts every autostart sandbox's stopped
  instances (as `devsandbox start`; a sandbox with no instance gets one)
  once per host boot. The pass runs at daemon start (every known config
  root) and after a user's `run`/`rebuild`/`start`; no other command
  triggers it.
- "Once" is keyed on the boot id (Linux `/proc/sys/kernel/random/boot_id`,
  macOS `kern.boottime`), recorded per config root in `state.toml`
  (`autostart_boot`). An instance the user stops after autostart stays
  stopped until the next boot.
- `autostart = "runtime"`: the container gets `--restart unless-stopped`
  where the backend supports it; unsupported backends behave as `true`
  with a warning; podman warns that it needs `podman-restart.service`,
  which devsandbox never installs.
- `autostart` is stripped from `config_hash` (0009).

## Consequences

- No unit files or launch agents per sandbox; the user's daemon (or their
  next command) brings automations back.
- Without the daemon installed as a service, autostart waits for the
  user's first command after boot.

Source: docs/automations.md, "autostart".
