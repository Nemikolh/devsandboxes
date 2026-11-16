# Mesh state: how resilient the fleet's state should be

Status: explored, nothing decided. Options and trade-offs only.

Kubernetes keeps all cluster state in etcd: Raft, an odd number of
voting members, writes blocked when a quorum is lost. k3s defaults to
SQLite on a single server, offers embedded etcd for HA, and kine to put
the Kubernetes API on other stores. The question here is what, if any of
that, a fleet needs.

The constraint: no dedicated nodes. Members are the user's machines,
often laptops asleep or off the tailnet; a fleet may have one always-on
member, or none. Raft only helps with at least three always-on voters,
and a fleet's typical shape (one server, two laptops) can't make a
quorum that survives the server.

## State, by kind

| kind | what | today | needs consensus? |
|---|---|---|---|
| **Identity, membership** | who is in, with which key; who is the control plane | the membership log, `membership.json`: a hash chain only the authority signs, verified entry by entry, carried by any member (`fleet::membership`, [ADR 0044](../../decisions/0044-one-fleet-authority-signs-membership.md)) | no: single writer, forks detected (`membership::compare`) |
| **Desired state, signed** | which config version every member runs | `config` ops in the same log, bundles in `mesh/tree/` ([ADR 0049](../../decisions/0049-the-authority-signs-a-config-every-member-runs.md)) | no, same writer |
| **Desired state, unsigned** | placements, where a [mesh service](../active/mesh-services.md) lives | `placements.json` per placing host (`fleet::placements`), a home not recorded yet | single writer if the control plane owns it; contested if any member may write |
| **Per-member settings** | trust, agent grants, allowances, listen address | `member.toml`, each member its own | no, by design (a member's own setting) |
| **Observed** | instance rows, capacity, the link graph | snapshots (`remotes.json` saved on the control plane), capacity reports, signed link reports flooded and held in memory (`fleet::graph`, `fleet::reports`) | no: re-derived from the members themselves; newest signed report wins (`Seen`) |

Most of the state already avoids consensus: either one writer signs and
everyone verifies, or each member is the source of its own facts. What
fails today is liveness of the single writer: with the authority offline,
nothing is added, revoked, rotated or signed, though everything already
signed keeps working. That single point of failure, and moving or
splitting the authority, is [control-plane](../active/control-plane.md)'s
subject.

## Options

1. **Single-writer signed logs (today).** The authority writes, every
   member verifies and carries. Availability of reads is total; writes
   wait for one machine. Recovery from a lost authority is a new
   genesis (a new fleet) unless a successor was pre-authorized. Cheap,
   understood, already built.
2. **Opt-in Raft among always-on members.** Members marked always-on
   (the graph's `always on` already exists) vote; laptops are learners,
   reading the committed log. With three voters a write survives one
   down. The log's entries stay the same signed ops, but the writer is
   the Raft leader, signing with a fleet key the voters share or a
   threshold signature, which is the hard part: a shared key on three
   machines is three ways to lose it. The `openraft` crate fits the
   threading model poorly (it's async; the daemon is threads) and adds
   a storage and snapshot layer. Below three voters it's worse than (1):
   two voters lose writes when either sleeps.
3. **CRDTs or gossip for observed state.** Already how the graph works:
   signed per-member reports, flooded, newest wins. Extending it to
   instance rows and capacity would let any member show the fleet
   without the control plane's federation. Doesn't help desired state,
   where two writers must not both win (two homes for one postgres).
4. **Leases for contested desired state.** A placement or service home
   is held by a lease the control plane grants and the holder renews;
   with the control plane gone the holder keeps it until expiry, then
   it's free. Gives fencing (the fleet plan's step 14 open question)
   without a quorum, at the cost of clocks and a timeout to tune.

## Questions

- Which writes can wait for one machine? Membership and config signing
  probably can (a day without adding a member is fine); placement of a
  `run` while the control plane is asleep probably can't.
- Is a pre-authorized successor (the log names a backup authority whose
  key may sign once the first is silent for N days, or a countersigned
  handover) enough resilience, short of consensus?
- Do any users have three always-on machines? If not, (2) is moot.
- Should desired state the control plane holds (placements, homes) be
  signed into the log like configs, or stay a file only it writes?
