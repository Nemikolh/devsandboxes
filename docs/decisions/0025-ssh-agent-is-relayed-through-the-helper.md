# 0025: The ssh agent is relayed through the helper, not bind-mounted

Status: accepted

## Context

Agents in sandboxes need git over ssh without the private key entering the
container. Bind-mounting the host's `SSH_AUTH_SOCK` doesn't work on Apple
`container` (no file binds), breaks when the host agent rotates (new socket
path), and hits uid mismatches between the socket owner and the container
user.

## Decision

- Relay first: on unix, whenever a helper is embedded, the agent is never
  bind-mounted (`run::mod` `relay_first`). The in-container helper daemon
  runs as root and serves `/run/devsandbox/ssh-agent.sock` at 0666; each
  connection is relayed over the bridge to a host agent.
- `SSH_AUTH_SOCK` is injected per `exec` at that one fixed container path
  (`exec::ssh_auth_sock_env`), never via `run -e`, so it applies to the CLI,
  dashboard terminals and lifecycle commands alike.
- The host agent is chosen per stream from the sockets clients report
  (`bridge::AgentCandidates`); the daemon's service unit doesn't capture
  `SSH_AUTH_SOCK`.
- Builds without an embedded helper fall back to a bind mount through a
  re-pointable symlink (`ssh_agent::ssh_agent_link`).

## Consequences

- One mechanism on every runtime; survives agent rotation without rebuild.
- Any uid in the container can use the agent; the blast radius is the one
  container, and the key never enters it.

Source: docs/sandbox-helper.md, "Precedence", "0666 agent socket"; docs/ssh-agent.md, "How it splits".
