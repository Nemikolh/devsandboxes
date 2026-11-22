# 0059: Feature updates run in the feature's image, never on the host

Status: accepted (2026-11-22).

## Context

A feature's tool (gh) should stay current in every sandbox without a
rebuild. Letting any sandbox write the shared binary (as zidane does) lets
one sandbox change what every other runs, which a credential-brokered tool
can't allow. Running the feature's update script on the host gives a
feature's code the host.

## Decision

- A feature declares the shared dir (a read-only `${sharedVolumes}` bind of
  its own) and the update command baked in its image
  (`customizations.devsandbox.updates`).
- devsandbox runs that command in a throwaway container from the newest
  local features image that has the feature, under the sandboxes' identity
  plan and argv guards, with that one dir writable and nothing else
  (`commands::update`).
- The daemon runs due updates at start and hourly (default every 7 days,
  `updates.<name>` in the config overrides or turns it off;
  `serve::updates`); `devsandbox update <name> [args…]` runs one now.

## Consequences

- The update code is trusted exactly as much as the feature's `install.sh`.
- A sandbox never has the dir writable, so it can't swap the binary.
- No update runs until a sandbox with the feature has been built.
