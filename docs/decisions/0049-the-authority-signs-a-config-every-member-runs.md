# 0049: The fleet's authority signs a config every member runs

Status: accepted (2026-11-11). Supersedes 0002's per-member config
authority for the config roots the fleet's authority signs; 0002 holds
for every other root.

## Context

A config root belongs to the host it was written on: that host's user
wrote `devsandboxes.toml`, and the config is the security boundary
(0002). With a fleet that's too narrow. An always-on server should run
the sandboxes of a config written on a desktop, driven from a laptop,
while the desktop is off for days. The server then runs a config its
own user never wrote, so something has to say which config, and which
version of it, is the one to run, without the writer online.

The membership log (0044) already carries any signed entry to every
member without its signer: a hash chain only the authority signs, each
member verifying it entry by entry and taking a newer one from any peer.

## Decision

- **The authority's signature is the config's authority.** For a config
  root the authority signs (`Op::Config {root, version, hash}` in the
  membership log, `fleet::membership`), what the signed version says is
  what every member may build, not what that member's user wrote. The
  authority signs on its own verbs only (`changes::sign_config`: a
  `run`, `rebuild` or `start` from a local client there, when the root's
  bundle hash changed), never on a remote request.
- **Latest signed, never older.** A `config` entry needs a version above
  its root's current one, so a log never takes a root back. Every member
  installs the latest signed version it holds (`fleet::mesh::install`,
  refused unless the bundle hashes to the log's head) and builds from its
  copy only while the copy is that head (`fleet::mesh::check_root`), even
  while the authority is offline; never an older one.
- **It overrides a `worker` member's policy.** The copy lives under
  `<data>/devsandbox/mesh/tree/`, outside `fleet/`, so no worker policy
  applies to it (`worker_policy::Policy::detect`): the signature is the
  consent. `key_guard`'s checks keep running everywhere.

## Consequences

- The authority's key is worth root on every member: whoever signs with
  it can have every member build a privileged sandbox. A privileged
  sandbox with the runtime socket on the authority can read that key
  (`key_guard` covers binds and builds only).
- A member's own user can't change a signed config on that member: a copy
  edited in place is refused, then reinstalled from the bundle the member
  stored (`fleet::mesh::follow`), with no new signature and the authority
  offline or not.
- A copy is checked once, when a build starts: a writer racing the build
  (a privileged sandbox mounting the copy read-write) isn't stopped.
  That sandbox is root on the host anyway.
- Losing the authority's key freezes every signed config at its last
  version, as it freezes membership (0044).
- Roots the authority never signs keep 0002 as is: their host's config is
  the boundary. A peer's build from a root named like a signed config but
  not its copy is refused (`fleet::mesh::check_peer_root_in`, from
  `daemon::check_mesh_copy`): the peer asked for the signed config, not
  this host's unsigned dir of the same name. A local client's isn't.
