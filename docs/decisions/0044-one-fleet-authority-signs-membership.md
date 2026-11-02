# 0044: One member, the fleet's authority, signs its membership

Status: accepted

## Context

A fleet link is ssh, and ssh vouches only for the end of one hop. Once
streams are routed through other members (fleet plan step 10b), a relay
could claim to carry another member's stream and get that member's
trust; the hub's own calls (removing a member, placing a run) were
admitted by the name a link announced. Every member needs a key, and
every member needs to agree on which keys belong to the fleet. Someone
has to decide that: the hub is the obvious candidate, but it is an
always-on server, the machine most exposed, and a hub that could change
the member list could hand its role, or the fleet, to anyone.

## Decision

- **Every member holds a key.** An ed25519 key in `member.key` next to
  `member.toml` (`fleet::identity::FileKeyStore`, behind the `KeyStore`
  seam, 0600), shown as a fingerprint (`devsandbox machine key`). No
  container devsandbox creates or image it builds reaches it, on any host
  or trust level (`key_guard::check_argv`, `key_guard::check_mount`), and
  devsandbox never loads it into an ssh-agent.
- **One authority.** The fleet's authority is one member, the host that
  ran the fleet-creating `machine add` (with `--hub`, the laptop, not
  the server it makes the hub). Its key alone signs membership: the
  genesis, adds, revokes, rotations and hub changes, as a hash-chained
  log (`fleet::membership::Log`, `membership.json`) every member verifies
  entry by entry (`membership::apply`). The authority never revokes
  itself; it rotates.
- **The hub is availability, not authority.** It stores and serves the
  log like any member; a key it hands over with no authority's (or
  introducer's) signature is refused. Its hub-only calls are admitted by
  its key in the log, not its name (`lent::admit_fleet`).
- **Changes typed elsewhere are proposed.** A member's `machine rm`,
  `machine key rotate` or introduction goes to the authority as
  `fleet.propose` (`machines::changes::propose`) and waits in
  `proposals.json` for the user's `machine approve|deny` there, with a
  desktop notification (`fleet::proposals`); the authority's key never
  signs on a remote request alone. A signed change is pushed to the
  authority's machines and floods on (`fleet.sync`,
  `membership_api::sync`); a member not reached catches up when a link
  compares heads (`fleet::proof::prove`).
- **Introductions are capped.** A non-authority member's `machine add`
  signs an introduction outside the chain
  (`membership::Introduction`), held by every member until the authority
  adds the member to the log. While trust is one setting per host, the
  cap is recorded and shown (`machine ls`) and a capped member
  introduces no one, proposes nothing, and removes no one
  (`membership::check_introduction`, `membership_api::can_sign`).
- **No fallback.** Each peer link proves both ends' keys against the log
  after its `Hello`s (`fleet::proof`); a link that doesn't prove is
  refused, and so is a host with no key or log.

## Consequences

- Adding or revoking a member waits until the authority is reachable,
  except through an introducer; until routing, a proposal reaches it only
  over a direct link.
- A compromised hub can withhold the log but not rewrite it: a fork or
  another fleet's log is refused at the next link.
- Losing the authority's key freezes membership; there is no recovery
  but making the fleet again.
- Fleets made before member keys have no log; their links are refused
  until the fleet is made again with `machine add`.
