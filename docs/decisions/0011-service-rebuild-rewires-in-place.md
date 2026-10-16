# 0011: `service rebuild` recreates the service and rewires sandboxes in place

Status: accepted

## Context

Changing a service's image or config used to require recreating every
sandbox that uses it, losing their running processes and unpersisted state,
just to point them at a new service container.

## Decision

`devsandbox service rebuild <name>` (`services::rebuild`) recreates the
service's containers and rewires running sandboxes without restarting
them, on every runtime: docker/podman resolve the new container through its
network alias; Apple `container` gets the sandbox's `/etc/hosts` rewritten
(`Backend::wire_service_dns`). `run`/`start` only warn about a drifted
service; they don't recreate it.

## Consequences

- Rebuilding a service is non-disruptive for sandboxes; existing TCP
  connections to the old container drop and clients reconnect.
- Each runtime needs a `wire_service_dns` implementation (a no-op where
  aliases already resolve).
- Recreation is explicit: no command silently replaces a service.

Source: docs/service-rebuild.md, "Design decisions".
