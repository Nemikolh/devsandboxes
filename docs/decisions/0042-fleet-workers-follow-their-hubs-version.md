# 0042: Fleet workers follow their hub's version, downgrades included

Status: accepted

## Context

A hub can't install anything on a worker: it reaches it only through the
lent API (0041). Both ends of a fleet link run devsandbox, and a worker
whose build differs from its hub's speaks whatever that build speaks.
Between daemons on one host the newest binary wins (0015), but a hub is
the fleet's reference: a laptop that upgraded first must not run work for
an older hub with newer semantics, nor wait for the user to fix it by
hand.

## Decision

- Each end of the fleet link says its build in its `Hello`
  (`fleet::update::HELLO`: semver, git hash, `published` or `dev`, stamped
  by `build.rs`). Before it serves anything, the worker compares the hub's
  with its own (`fleet::update::decide`): the same triple is the same
  build; otherwise the worker follows the hub's **exact** build, older or
  newer.
- A published hub build is downloaded from its GitHub release asset for
  this platform over HTTPS (curl, TLS 1.2 at least), checked by running
  its `--version` before and after it is installed in place over
  `exe::devsandbox()` with `machines::install::INSTALL`'s steps (temp file
  next to it, rename). There is no checksum asset: TLS and the version
  check are the verification.
- The daemon then hands off to the installed binary through a **forced
  hello** (`HelloParams::force`): a hello that hands off whatever the
  client's version, honored only on a local connection (`Full` table) whose
  peer runs as the daemon's own uid; a `Worker` connection never hands
  off, forced or not. The successor starts as any handoff's does.
- On any failure the worker stops offering (no link), `lend.get` says why,
  and the next link attempt, after the usual backoff, tries again.
- A hub on an unpublished build the worker isn't on is not followed yet
  (fleet step 2c: the hub's binary over `TRANSFER`); the worker stops
  offering, saying so.

0015 stays for everything else: a forced hello is the one exception to
"the newest binary wins", and only a worker following its hub sends one.

## Consequences

- A fleet converges on the hub's version without the user touching the
  workers; upgrading the hub upgrades the fleet, downgrading it too.
- A worker's own CLI and dashboard become the hub's version as well: the
  binary at the worker's path is replaced.
- Until release builds are marked published (`DEVSANDBOX_PUBLISHED=1` in
  the release workflow), every build is a dev build and only the 2c path
  applies.
