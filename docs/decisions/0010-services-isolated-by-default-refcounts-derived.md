# 0010: Services are isolated by default; `global` is opt-in; refcounts are derived, never stored

Status: accepted

## Context

Sandboxes declare services (databases, caches). Sharing one container
across instances is cheap but lets agents corrupt each other's data;
one per instance is safe but heavier. Tracking who uses a shared service
with a counter in state drifts the moment a command crashes.

## Decision

- `scope = "isolated"` (the default, `config::ServiceScope::Isolated`): one
  service container per instance, on a per-instance network.
- `scope = "global"`: one shared container per config root; an instance
  joins the root's global network only when it uses a global service.
- Who references a service is computed each time from `state.toml` plus
  runtime labels (`services::service_refs`), never stored. `gc` reaps
  services nothing references.

## Consequences

- Isolation is the safe default; sharing is an explicit config choice.
- No counter can go stale; the cost is a runtime listing per `gc`/`stop`/`rm`.
- `gc` counts only running (or in-flight) instances as live, so it reaps a
  stopped instance's isolated services; its next `start` recreates them
  empty (tracked as a known limitation).

Source: docs/run-command.md §4; docs/high-level-architecture.md, "Services".
