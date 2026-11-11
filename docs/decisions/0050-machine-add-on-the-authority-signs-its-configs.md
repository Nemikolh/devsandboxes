# 0050: `machine add` on the authority signs its configs

Status: accepted (2026-11-11). Supersedes 0049's "on its own verbs only"
for `machine add`; the rest of 0049 holds.

## Context

Under 0049 the authority signs a config root only when a `run`, `rebuild`
or `start` typed there builds from it. A new member therefore had no
config to run until the user typed a verb on the authority first, a step
nothing in `machine add` hinted at. The first real fleet setup (a server
added from a desktop, driven from a laptop) needs the server to hold the
desktop's configs as soon as it's added.

## Decision

- **A local `machine add` on the authority signs its configs.** After the
  add's writes (this host's adopt, its `hub`) and before the links reload,
  the authority signs `~/.devsandboxes` and `member.toml`'s `roots`
  (`changes::sign_configs` over `changes::add_roots`), exactly as a verb
  signs its root (`changes::sign_config`): a new version only when the
  bundle's hash changed. `state.toml`'s other config dirs are never
  signed this way: the first sign binds a root's name for good
  (`mesh::bind`), so only roots the user put forward are.
- **Local only.** As for verbs, only a local client's add signs
  (`Table::Full`); an add from a link peer, even under `trust = "full"`,
  signs nothing. The authority still never signs on a remote request
  alone (0044, 0049).
- **No new transport.** The new member takes the newer log at its first
  link-up and installs the bundles from it (`daemon::follow_configs`).

## Consequences

- Adding a machine can bind root names (`mesh/roots.toml`) the user never
  typed a verb in: `~/.devsandboxes` and the picked roots.
- A sign that fails is a note in the add's answer (`Added::configs`),
  never a failed add: the add's writes are committed by then.
- An add on a member that isn't the authority signs nothing and says
  nothing of it.
