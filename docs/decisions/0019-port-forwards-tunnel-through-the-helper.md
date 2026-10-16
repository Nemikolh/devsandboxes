# 0019: Port forwards tunnel through the helper and are owned by the daemon

Status: accepted

## Context

Users need host ports reaching a dev server in an instance or a service.
Publishing with `-p` can't be added to a running container on any runtime
(recreating loses state), and a socat-style sidecar can only reach
`0.0.0.0` binds, while dev servers often bind `127.0.0.1` inside the
container's network namespace.

## Decision

- A forward binds a host listener and tunnels each connection over the
  helper's frame channel (`src/devsbd/forward.rs`, mux `Connect` streams
  with flow control); the in-container `devsbd` dials `127.0.0.1:<port>`
  (instance) or `<service>:<port>` through a running instance that
  references the service. With no usable instance, the helper is injected
  into the service container on demand. Routes re-resolve on every
  reconnect, so a restart or `service rebuild` heals without restarting the
  forward.
- Configured (`forwardPorts`) and Ports-tab forwards live in the daemon
  (`serve::forwards`, one registry per config root); their host ports are
  saved (`state.toml`, ad-hoc ones in `forwards.toml`) so a successor
  restores them on the same ports. `devsandbox port` in the foreground is
  the CLI-owned exception, alive until Ctrl-C.

## Consequences

- Forwards work on every runtime and can be added to running containers.
- Needs the helper (or root `exec` + `/bin/sh` + writable `/run` in the
  service image for injection; distroless images fail).

Source: docs/port-forwarding.md, "Why tunnel through devsbd (not -p, not a socat sidecar)", "Design decisions".
