# 0053: The authority pins the fleet's build; members follow the pin

Status: accepted (2026-11-17). Supersedes
[0042](0042-fleet-workers-follow-their-hubs-version.md).

## Context

Under 0042 every member followed the control plane's exact build, read
off its link `Hello`, and the control plane followed no one. The
authority, the member whose key signs the membership log (0044), had no
part in it. With a remote control plane and the authority on the laptop
the user develops on, upgrading the laptop first either cut it off (a dev
control plane: `Stop`, redialed and refused again) or silently reverted
the upgrade (a published control plane: `Download` of the older release).
And a member took any binary its control plane pushed: the only check
was the binary's own `fleet-hello`, run before anything else.

The authority can already move and revoke the control plane and make
every member run a config (0049), so taking code from it adds no trust.

## Decision

- **The pin is a membership log entry**, `Op::Build { version, pin }`
  (`fleet::membership`), signed by the authority only, its `version` the
  latest + 1: persisted, applied in order, checked against the authority's
  key as of the entry, delivered by the link proof's sync. A downgrade is
  only a newer entry.
  - `Pin::Dev { semver, git, platform, sha256 }`: a build is its bytes.
  - `Pin::Published { semver, sums_sha256 }`: the release, bound by the
    sha256 of its signed `SHA256SUMS` (0047).
- **The authority pins automatically** (`machines::changes::sign_build`,
  the daemon's `serve-pin` thread): at daemon start and when its link to
  the control plane comes up, whenever its own build differs from the
  pin. A dev build is pinned only where the control plane can run it (its
  platform from its link's `Hello`, or this host is the control plane);
  else it's skipped, saying why. A pinned dev build is kept under
  `<data>/fleet/.builds/<sha256>`, so the authority can serve it after a
  rebuild.
- **The `Hello` says the bytes**: a peer link's `Hello` also carries the
  binary's sha256 prefix and whether this end takes dev builds, so both
  ends decide from the `Hello`s alone.
- **Everyone but the authority follows the pin**, the control plane
  included (`fleet::update::decide`). No pin, nothing to follow. A
  release is downloaded once a linked peer is on it (or it is the
  authority); a dev pin is received, with `dev-builds` (or trust `full`)
  and on its platform, from any linked peer that has it, hop by hop
  (`update::pushes`, its mirror), never over routes. One update at a time
  per host (`update::lock_update`).
- **Checked before it runs**: a received binary's sha256 must be the
  pin's before it is made executable or run (`update::install_received`);
  a download's `SHA256SUMS` must hash to the pin's besides its signature.
- **Behind, not refused**: a host that can't or won't take the pin
  (another platform, no release asset, dev builds not taken) keeps its
  build and its links, shown as `behind: <why>` on both ends.
- **The update path stays compatible across versions**: what following
  rides on (the frame protocol, the sentinel, the proof's hello and sync,
  `Op` serde, the `TRANSFER` header) is the one exception to breaking
  wire formats freely.
- `devsandbox machine upgrade <name>` copies this binary to a machine over
  ssh when its bytes differ, then makes its daemon hand off.
- Pinning builds is never delegated
  ([control plane](../plans/active/control-plane.md)).

0042's forced hello stays: it is how a member hands off to what it
installed.

## Consequences

- Upgrading the authority first never locks it out or downgrades it; the
  fleet converges on its build without a manual copy where platforms
  allow.
- A compromised control plane can no longer push arbitrary code. A
  compromised authority key is code execution on every member's host,
  outside containers, which signed configs already gave it (0049).
- Every new binary the authority's daemon starts on is pinned; members
  taking dev builds hand off with it, and a pinned build whose update
  path is broken strands them until fixed by hand. `dev-builds` is each
  host's brake.
- Hosts behind a pin run another version on their links, relying on the
  update path's compatibility.
- A peer can withhold a newer `Op::Build`; any other direct peer's sync
  delivers it.
