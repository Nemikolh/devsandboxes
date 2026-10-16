# 0014: One host daemon per user, on a unix socket, unix-only

Status: accepted

## Context

The live host side (bridges, port forwards, the API, machine links, verbs
others can watch) needs one long-lived process. It must be reachable by the
user's CLI, dashboard and the npm package, and by nobody else.

## Decision

- `devsandbox serve` runs once per user, started lazily by the first client
  that needs it (`serve::client::connect`) or as a user service
  (`serve install`).
- It listens on a unix socket only, in a 0700 directory with the socket at
  0600 (`serve::endpoint`): access control is file permissions, like
  `docker.sock`. The directory is chosen for a short path (macOS caps
  socket paths near 104 bytes). There is no TCP listener.
- The wire is JSON lines (`serve::proto`, `PROTOCOL`). Other processes reach
  it through `devsandbox api --stdio` (`serve::relay`), a byte relay, which
  is also what the npm `connect()` and remote machines (0020) spawn.
- The daemon is unix-only (`mod serve` is `cfg(unix)`); elsewhere the CLI
  runs everything in-process (0001).

## Consequences

- No auth tokens, no ports to collide or expose.
- Windows has no daemon; the planned direction is Windows as a remote-only
  client of a machine (see plans).
- Remote access is always ssh to the relay, never a network listener.

Source: docs/serve.md, "Paths"; docs/client-server.md; docs/api.md.
