# 0024: One in-container helper daemon; a different build takes over

Status: accepted

## Context

A container may be reached by host binaries of different builds over its
life (upgrades, a rebuilt helper reinstalled into a running container). Two
helper daemons in one container would fight over the agent and API
sockets; refusing the newcomer would leave an outdated daemon serving a
binary that no longer matches the one on disk.

## Decision

- One helper daemon per container, holding a pidfile lock for its lifetime.
- A starting daemon of a different build asks the holder to quit and takes
  over (`devsbd/src/daemon.rs` `take_over`). The comparison is build-hash
  *inequality*, not "newer": the binary on disk always matches the running
  daemon ("last start wins").
- The serve daemon owns the one long-lived host-side bridge per running
  container (`serve::host`; port forwards open short-lived bridges of their
  own, without notify/control); CLI commands that need it ask with `bridges.ensure`
  instead of spawning their own, so notify, control and agent reports have
  one place to go. Bridges reinstall a stale helper before (re)spawning.

## Consequences

- A downgrade also takes over; that's intended (disk wins).
- Notify and control streams go to the newest bridge whose host advertises
  the capability.

Source: docs/sandbox-helper.md, "Last start wins (takeover)"; docs/automations.md, "Helper self-heal"; `src/devsbd/bridge.rs` module doc.
