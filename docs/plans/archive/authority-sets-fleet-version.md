> Archived 2026-11-17. Historical, may be wrong; do not use as a reference.
> Current: [architecture/daemon/fleet.md#versions](../../architecture/daemon/fleet.md#versions),
> [ADR 0053](../../decisions/0053-the-authority-pins-the-fleets-build.md)

# The authority sets the fleet's version

Status: shipped (1f19141, 770e9be, da38bf6, c2bbb2a, 62dbd40).

## Why

Today the control plane is the fleet's version reference
([fleet: Versions](../../architecture/daemon/fleet.md#versions)): a
member follows its exact build (`fleet::update::decide`: semver, git and
`published` all equal), in either direction, upgrade or downgrade, and
the control plane follows no one (`commands::fleet_link::follows`). The
authority, the member whose key signs the membership log, has no part in
this. When the control plane is a remote server and the authority is the
laptop the user develops on, upgrading the laptop first breaks it:

- control plane on a **published** build: `decide` says `Download`
  whatever `dev-builds` says, so the laptop installs the older release
  over its own binary and hands off. The upgrade is silently reverted;
- control plane on a **dev** build: without `dev-builds` (or trust
  `full`) the laptop refuses (`Stop`), and shows the control plane as
  `unreachable: <why>` from then on, redialed and refused again; with
  `dev-builds` it takes the control plane's older binary (`Receive`),
  reverting the upgrade;
- the way out is copying the binary to the control plane by hand.
  `machine add`'s install step (`machines::install::decide`,
  `install::install`) copies this binary only when the remote is missing
  or on another semver, and only at `add`; there is no update verb.

The authority can already move the control plane and then revoke it
(`membership::apply` refuses revoking the control plane until another is
named), and can make every member run any config (ADR 0049). Taking code
from it adds no trust the fleet doesn't already give it.

Second gap: a member accepts any binary its control plane pushes.
`update::install_received` runs the received file's `fleet-hello` and
compares its build words with the control plane's `Hello`
(`check_build`); the transfer header's sha256 comes from the sender. A
compromised control plane can run code on every member that takes dev
builds on its platform, at check time already.

## Design

**The authority's build is the fleet's build, pinned by an entry in the
membership log.**

1. **`Op::Build` in the log.** The pin is a membership log entry, like
   `Op::Config` ([ADR 0049](../../decisions/0049-the-authority-signs-a-config-every-member-runs.md)),
   not a standalone `identity::Statement`: `identity::Seen` is in memory
   only (`graph::Graph`) and keyed per signing key, so a restart (which
   every update's handoff is) or a `rotate` would reopen replay. The log
   gives what the pin needs: persisted (`membership.json`), applied only
   in order and only forward, each entry checked against the authority's
   key as of that entry (so `rotate` and `control-plane.md`'s `recover`
   need nothing extra), delivered by the link proof's sync
   (`proof::prove`, before `follows` runs) and `fleet.sync`. Only the
   latest `Op::Build` counts. A downgrade is possible only as a newer
   entry the authority signs.
   - **Dev build:** the build label (`update::Build`: semver, git,
     `published`), the platform (`update::platform_of`), and the sha256
     of the binary. One platform, the authority's own.
   - **Published build:** the release (semver) and the sha256 of its
     `SHA256SUMS` file: one hash binds the whole signed checksum list,
     so every target is covered without the authority fetching assets.
2. **A build is its sha256.** `Build` has no dirty flag (`build.rs`
   takes `git rev-parse` of `HEAD`), so a rebuild from uncommitted edits
   keeps its label. A member is on a dev pin when the sha256 of its own
   executable equals the pin's; the label is for display. A member is
   on a published pin when it runs that release's build; its bytes were
   checked against the `SHA256SUMS` at install.
3. **When it's pinned: automatically.** The authority's daemon signs a
   new `Op::Build` when it starts on a binary whose sha256 (or release)
   differs from the latest pin (after a `cargo build` and a handoff, or
   an upgrade), and pushes it the way membership changes go. Guard: a
   dev build is not pinned while the control plane's platform differs
   from the authority's (the authority could only pin a build the
   control plane can never run); the daemon logs why and `machine ls`
   shows it. The control plane's platform is what its link's `Hello`
   says, so the check runs at daemon start and again whenever the
   authority's link to the control plane comes up; unknown (no link
   yet) pins nothing until then. A published build's `SHA256SUMS` is
   fetched and verified at signing; offline, the next trigger retries. At signing the authority keeps a copy of the binary under
   `<data>/fleet/.builds/<sha256>` (a dot name, never a machine's fleet
   root), since a later `cargo build` replaces
   the file its `current_exe()` points at.
   **The `Hello` says the bytes.** A peer link's `Hello`
   (`update::hello_of`) also carries a prefix of this binary's sha256
   (16 hex) and whether this end takes dev builds, so both ends compute
   who pushes from the `Hello`s alone, as `control_plane_pushes` does
   today, and no binary is sent to a host that won't take it. The full
   sha256 is checked at install.
4. **Who follows.** Every member but the authority follows the latest
   `Op::Build`, the control plane included, never a peer's `Hello`.
   The authority follows no one. `update::decide` compares this host
   with the pin; `fleet_link::follows` and `update::control_plane_pushes`
   stop deciding by who is the control plane.
   - published pin: download the release for this platform, verify it
     against `SHA256SUMS.sig`
     ([ADR 0047](../../decisions/0047-release-downloads-are-verified-by-signed-checksums.md))
     and the pin's `SHA256SUMS` hash, install, hand off;
   - dev pin: taken only on the pin's platform and with `dev-builds`
     (or trust `full`), as today. With pins automatic, `dev-builds` is
     each host's brake on how far a broken dev build spreads.
5. **Behind.** A host that can't or won't take the pin (another
   platform, no release target, `dev-builds` off) keeps its build and
   still links, on the frame protocol's match, as two non-control-planes
   do today. It is `behind: <why>`, not `unreachable`, on both ends
   (`machine ls`, the dashboard header). So the links on the update path
   must keep working across versions: the frame `VERSION`,
   `sentinel_version`, the proof's hello/sync messages, `Op` serde and
   the `TRANSFER` header stay compatible (an exception to "break wire
   freely" for these only, written down in `fleet.md`).
6. **Where the bytes come from: hop by hop.** Any proven, linked peer
   whose own binary's sha256 equals the pin pushes it (`update::push`,
   generalizing `control_plane_pushes`); the authority pushes its kept
   copy. No routes: a member waiting for the binary serves only that
   transfer (`run_peer`'s `receiving`, `follow::Incoming`), `ROUTE`
   included, so a routed push through waiting members would deadlock.
   A member that took the pin then serves its neighbours.
7. **Check before running.** `update::install_received` checks the
   received file's sha256 against the pin first, and only then runs its
   `fleet-hello` (`check_build`).
8. **Not delegated.** Pinning a build joins `control-plane.md`'s
   never-delegated list: a delegated control plane that could pin
   reopens the second gap.

## Accepted consequences

- Every new binary the authority's daemon starts on is pinned fleet-wide
  (any `target/debug/devsandbox` run hands the daemon off,
  [ADR 0015](../../decisions/0015-newest-binary-wins-version-handoff.md)).
  Members with `dev-builds` on its platform hand off with it, their
  links cycle and remote terminals over them drop; a pinned build whose
  update path is broken strands them until fixed by hand. Installing an
  older build on the authority pins it, a fleet downgrade.
- A compromised authority key is code execution on every member's host,
  outside containers. That was already true through signed configs
  (ADR 0049); the superseding ADR says so.
- A peer can withhold a newer `Op::Build` (freeze); any other direct
  peer's sync delivers it. Not closed further.
- Mixed-platform dev fleets stay behind on dev pins: they run published
  builds, or the build is copied per platform by hand.

## Rollout

1. **The authority follows no one; `devsandbox machine upgrade
   <name>`.** `follows` returns `Same` on the authority as on the
   control plane, which ends both the lockout and the silent downgrade.
   `machine upgrade` copies this binary to a machine over ssh
   (`machines::install::install`, `install::platform_matches`), when the
   remote's build differs, not only its semver. Docs: `fleet.md`
   Versions, `machines.md`.
2. **`Op::Build` and following it.** The op, automatic pinning with
   the platform guard and the kept copy, `decide` against the pin,
   sha256 before running in `install_received`, the frozen update path.
   New ADR superseding 0042; `fleet.md`, `status.md`.
3. **Hop-by-hop push and `behind`.** Any peer on the pin pushes; the
   `behind: <why>` state on both ends in `machine ls` and the dashboard.

## Open questions

- **Log growth.** Automatic dev pins add an entry per daemon start on a
  new binary; the sync is bounded (`proof::SYNC_LIMIT`, 8 MiB). Fine for
  now; compacting superseded `Op::Build` entries may be needed.
- **Rollback net.** Keeping `devsandbox.prev` and reverting when the new
  daemon can't link within some minutes would cover a broken pinned
  build. Not decided.
- **Signing a build made elsewhere** (`devsandbox machine build sign
  <path>`, for a mixed-platform dev fleet): later, if needed.
