# 0040: Configs that assume root are migrated in place, asked per verb

Status: accepted. An exception to the pre-1.0 no-migrations rule, at the
user's request.

## Context

Under `identity = "host"` (0038), configs written for root-in-container
break: `target=/root/...` mounts land in a 0700 dir the dev user can't
reach, and `remoteUser = "root"` defeats the model.

## Decision

- `${containerEnv:NAME}` resolves at create time from the sandbox's
  `containerEnv`, the image's env, and for `HOME` the home of the account
  the container runs as. It is allowed in mounts and substituted in
  `containerEnv`/`remoteEnv`, beyond the devcontainer spec.
- Findings (`config_identity::identity_findings`) name the table that
  defines each `/root` entry or root user, so a shared template is fixed
  once. A finding's id hashes table, key and value, so an edited entry
  warns again.
- The fix is an in-place rewrite (`toml_edit`, comments and layout kept,
  a file-hash guard, a `.bak` only during the write); YAML configs get
  the edits printed. "Continue as is" is an ack in `state.toml`, except
  for a root user in TOML, where it writes `identity = "image"`.
- The daemon writes the user's config for `config.migrate`, because the
  dashboard and machines have no other way to write; a machine's config
  is rewritten by that machine's daemon.
- The choice is asked before `run`/`start`/`rebuild` (CLI and dashboard)
  and offered when the dashboard opens; off a terminal, `run` prints one
  warning line instead. Autostart may run before the offer is answered.

## Consequences

- A rewrite changes `config_hash`, so affected instances show drift and
  are named with the `rebuild` that fixes them.
- `workspaceFolder`/`folders` under `/root` are flagged but fixed by hand.

Source: the identity plan, steps 11–17.
