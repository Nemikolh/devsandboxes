# The authority sets the fleet's version

Status: explored, not agreed. Would supersede
[ADR 0042](../../decisions/0042-fleet-workers-follow-their-hubs-version.md).

## Why

Today the control plane is the fleet's version reference
([fleet: Versions](../../architecture/daemon/fleet.md#versions)): members
follow its exact build (`fleet::update::decide`), and the control plane
follows no one (`commands::fleet_link::follows`). The authority, the
member whose key signs the membership log, has no part in this. When
the control plane is a remote server and the authority is the laptop
the user develops on, upgrading the laptop first cuts it off:

- the laptop is a member on a newer build, so it must follow the
  control plane down to its older build. With no `dev-builds` it
  refuses (`Stop`), and the control plane shows as `unreachable` from
  then on, redialed and refused again;
- with `dev-builds = true` it would take the control plane's *older*
  binary, which reverts the upgrade;
- the only way forward is copying the binary to the control plane by
  hand, and nothing in `devsandbox machine` does it.

The authority can already add and revoke any member, the control plane
included. Taking code from it adds no trust the fleet doesn't already
give it.

There is a second gap: a member accepts any binary its control plane
pushes (`update::install_received` checks only that the binary's
`fleet-hello` matches the control plane's `Hello`). A compromised
control plane can push anything to the whole fleet.

## Proposal

**The authority's build is the fleet's build, and it says so with a
signed statement.**

1. **Fleet build statement.** The authority signs a `fleet-build`
   `Statement` (`fleet::identity::Statement::sign`, a kind of its own,
   its `version` increasing). Its payload is the build (`Build`:
   semver, git, kind), the platform, and the sha256 of the binary for
   that platform: one entry per platform the fleet runs (a dev build
   for the authority's own platform only; a published build names its
   release, whose assets are already verified by `SHA256SUMS.sig`,
   [ADR 0047](../../decisions/0047-release-downloads-are-verified-by-signed-checksums.md)).
   `identity::Seen` already refuses an older `version` from the same
   key, so an old statement can't be replayed to force a downgrade.
   A downgrade is still possible, but only as a newer statement the
   authority signs.
2. **When it's signed.** The authority's daemon signs a new statement
   when it starts on a build other than the last one it signed (for
   example after `cargo build` and a handoff), and pushes it the way
   membership changes go (`fleet.sync`, over the link or a route).
   Every member keeps the latest one it accepted, next to the
   membership log.
3. **Who follows what.** Everyone, the control plane included, follows
   the latest accepted statement, not a peer's `Hello`. The authority
   follows no one. `update::decide` compares this host's build with the
   statement's; `follows` and `control_plane_pushes` stop deciding by
   who is the control plane.
4. **Opt-in.** A dev build from the authority's statement is taken
   without `dev-builds` or trust `full`, because being the authority is
   enough. `dev-builds` still matters for nothing else: no other host
   can make a member take a dev build any more.
5. **Where the bytes come from.** The binary is checked against the
   statement's sha256 before it's installed, so it can come from any
   member that has it: the authority, the control plane after it
   updated, any peer. The push (`update::push`, today only through
   `fleet-<member>.sock`) goes over `route::connect` like code
   transfers do, so a host with no direct link to the authority still
   gets the build. A published build is still downloaded from the
   release, then checked against both `SHA256SUMS.sig` and the
   statement.
6. **Wrong platform.** A dev build for another OS/arch stays a
   `Stop(why)`, as today. The authority shows it for each member it
   affects (`machine ls`, the dashboard header), so the user doesn't
   just see `unreachable`.

## Open questions

- **Linking during an update.** While a member waits for the build, the
  link serves only the binary transfer (`follow::Incoming`, as
  `Receive` does today). The authority's own link to the control plane
  must stay up during that wait, so it is never refused for a version
  mismatch.
- **Published builds on several platforms.** Either the authority signs
  one entry per release target (it fetches `SHA256SUMS` once), or the
  statement names only the release and members rely on the release
  key. The first keeps "signed by the authority" true for every byte.
- **Revoked or rotated authority key.** A `rotate` re-signs the current
  statement under the next key, and members accept statements from
  the authority's key as of the latest log entry they hold.
- **Mixed-platform dev fleet.** The authority can only sign builds it
  has. Signing a binary built elsewhere (`devsandbox machine build sign
  <path>`) is possible, but left out of a first version.
