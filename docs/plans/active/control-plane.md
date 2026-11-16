# Control plane: Kubernetes terms, and an authority that isn't a single point of failure

Status: planned. Step 1 (hub → control plane,
[ADR 0051](../../decisions/0051-fleet-terms-follow-kubernetes.md)) is in
progress; the rest is design.

The fleet's roles map onto Kubernetes' ([kubernetes](../../reference/kubernetes.md)):
the control plane federates, places and is the build members follow; the
authority is the cluster CA's analog. The authority is also the fleet's
one hard single point of failure, which this plan removes.

## Problem

One member's ed25519 key, the authority's, signs every
[membership log](../../architecture/daemon/fleet.md#membership-log)
entry: genesis, `add`, `revoke`, `rotate`, `control_plane` and `config`
(`fleet::membership::Op`, [ADR 0044](../../decisions/0044-one-fleet-authority-signs-membership.md),
[ADR 0049](../../decisions/0049-the-authority-signs-a-config-every-member-runs.md)).
The authority is usually the laptop that ran the fleet's first `machine
add`, kept off the always-on control plane on purpose (0044: the most
exposed machine shouldn't hold the root).

1. **Authority asleep**: no member joins or is revoked, no config gets a
   new version, a control plane that is down can't be replaced
   ([status](../../status.md)). Running work is unaffected: members keep
   the latest signed copy.
2. **Authority key lost**: membership freezes for good; the only way out
   is making the fleet again.

Kubernetes copies its CA key to every control-plane node (`kubeadm
--upload-certs`) and orders writes through etcd's quorum. Copying our key
around forks the chain (two signers at one head), and a quorum needs
always-on voters we don't have ([mesh-state](../explore/mesh-state.md)).

## Design: a recovery key and a delegation

Standard PKI's shape: an offline root, an online intermediate.

### Recovery key (fixes 2)

- **Made at genesis.** `Op::Genesis` carries a `recovery` public key,
  a second ed25519 key drawn with the authority's. Its private half is
  shown once on a TTY (to keep off the machine), and saved next to
  `member.key` as `recovery.key` (0600) when there is no TTY. The UX
  (printing, a password manager, moving it off the host) comes later.
- **One power: replace the authority.** `Op::Recover {authority, key,
  recovery}`, signed by the recovery key only, names a new authority
  member and key and a fresh recovery key, ending every delegation. It
  isn't used for anything routine, so its key can stay offline for years.
- `key_guard` covers `recovery.key` as it covers `member.key`; devsandbox
  never loads it into an agent.

### Delegation (fixes 1)

- **The authority delegates to the control plane.** `Op::Delegate {to,
  key, scopes, until}`, signed by the authority: `to` must be the current
  control plane, `key` its member key, `until` a unix time. `devsandbox
  machine delegate [--for 30d] [--scope members,configs]` on the
  authority signs it. The duration is set per call: 30 days by default,
  at most one year (refused above). Renewing means signing a new one;
  `machine undelegate` signs `Op::Undelegate`.
- **Scopes:**
  - `members`: `add` (approving a pending join or introduction) and
    `revoke`.
  - `configs`: `config` entries, i.e. new versions of a signed root,
    so config changes. It's the strongest grant (config is the security
    boundary, [ADR 0002](../../decisions/0002-config-is-the-security-boundary.md):
    a compromised delegate could push a host-path mount everywhere), so
    it's a scope of its own.
  - Never delegated: genesis, `recover`, the authority's own `rotate`,
    `control_plane` moves, `delegate`/`undelegate`.
- **Rules** (`membership::apply`, pure): an entry the delegate signs
  applies when a live delegation names its key, the op is in its scopes,
  and the entry's signed time is before `until`. A `control_plane` move
  away from the delegate, an `undelegate`, a `recover` or an `until` in
  the past ends it.
- **Where changes go.** Proposals (`fleet.propose`,
  `machines::changes::propose`) go to the delegate while a delegation
  is live and in scope, and to the authority as today otherwise; `machine
  pending|approve|deny` work on either. With `configs`, a `run`, `rebuild`
  or `start` typed on the control plane signs the root's next version as
  the authority's do (`changes::sign_config`).

### One writer at a time

The chain stays single-writer in practice: the authority's verbs fetch the
delegate's head before signing (it's the always-on member, usually
reachable). When they can't, both may sign at the same head; the fork is
resolved, not refused: **the authority's branch wins**, and the delegate
re-signs its entries from the losing branch that still apply on top
(`revoke`s first, so a revoke is never lost, only delayed). Every other
fork is refused as today (`membership::compare`).

### What stays a single point

Moving the control plane needs the authority. With both down the fleet
runs on, but nothing about membership or configs changes, which is
Kubernetes' behavior with its control plane down.

## Rollout

1. **Rename hub → control plane** (code, wire, `member.toml`, CLI, docs;
   ADR 0051). No compatibility: fleets are made again.
2. **Recovery key**: `Genesis.recovery`, `Op::Recover`, `recovery.key`
   with `key_guard`, `devsandbox machine recover --key <file> <member>`.
3. **Delegation in the log** (pure): `Op::Delegate`/`Undelegate`, signed
   time on entries, the rules above, tests over `apply`.
4. **Delegate signing**: `machine delegate|undelegate`, proposals and
   approvals on the delegate, `configs` on the control plane's verbs.
5. **Fork resolution**: the authority syncs before signing; authority
   wins; the delegate re-signs.
6. **Docs and ADR** superseding 0044's "one authority" and 0049's
   "authority's own verbs only" for delegated scopes.
7. **Machine → node**: `devsandbox node …`, `nodes.toml`, `nodes.*` API,
   docs; `m/x` addressing unchanged.

## Open questions

- Delegate entry times: a compromised delegate past `until` could
  backdate entries. Members can refuse a delegate entry received after
  `until` by their own clock, but a member joining later verifies history
  by the entries' times alone. Is that acceptable, or should the
  authority countersign a delegation's last head when it ends?
- Does `members` include member key `rotate`? It's countersigned by the
  old key (`by_old`), so a delegate can't swap a key alone.
- Should `machine add --control-plane` offer a delegation at fleet
  creation (one prompt), or stay explicit?
- The recovery key's UX: shown once vs written to disk; multiple
  recovery keys (k-of-n) for teams.
