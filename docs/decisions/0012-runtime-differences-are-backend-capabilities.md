# 0012: Runtime differences are `Backend` capabilities, not runtime-name checks

Status: accepted

## Context

devsandbox runs on docker, podman and Apple `container`. They differ in
real ways (Apple has no restart policy, no `--privileged`, no file binds, no
`--cache-from`). Scattering `if runtime == "container"` checks through the
commands makes each new runtime or each runtime upgrade a hunt.

## Decision

`src/runtime/` exposes one `Backend` trait; docker and podman share the
`Dockerlike` implementation, Apple has `AppleContainer`. Differences are
capability methods (`Backend::supports_*`) and per-backend operations
(`wire_service_dns`, stats parsing); commands ask the capability and
degrade (skip, warn, fall back) instead of naming a runtime.

## Consequences

- Adding a runtime is implementing `Backend`, not editing every command.
- A few name checks remain where the difference is outside the runtime's
  API (VS Code specifics for `"container"` in workspace-file writing and
  `vscode::launch`; a podman-specific warning in
  `autostart::restart_decision`). Keep them rare.

Source: docs/runtimes.md.
