# 0045: A routed stream proves its origin to its destination, per stream

Status: accepted

## Context

A member with no link to another reaches it through members that have
one (fleet plan decision 21): a stream spliced hop by hop along a path of
the fleet's link graph. Each hop's peer link proves both ends' keys
(0044), but only for that hop: the destination sees the stream arrive
from its last relay. Served as the relay's, a stream would get the
relay's trust, and any relay could claim to carry another member's. The
destination must know who opened the stream, and the origin must know
it reached the destination and not a relay answering in its place.

## Decision

- **A channel of its own.** A routed stream is a link stream on `ROUTE`
  (`channel::ROUTE`, `caps::ROUTE`; host to host only, the in-container
  helper never sees it), so `Frame::Open` stays one channel byte. It
  starts with a header (`fleet::route::Header`): destination, origin,
  inner channel, path and TTL, bounded (`route::HEADER_LIMIT`).
- **Source routing.** The origin puts the path in the header as member
  names, from its own graph (`fleet::graph::Graph::paths`, in order:
  direct, fewest hops, through always-on members, longest up), TTL
  `graph::MAX_HOPS`. Every hop checks the header (`Header::check`: no
  loop, the TTL covering the hops left, names it knows,
  `route::relay_header`) and that it came from the hop before it
  (`route::came_from`); a relay checks only that its next hop is a
  direct link that's up, then splices the rest unread. The requester
  tries paths in turn and moves on at a refusal, a timeout or a failed
  connect (`fleet::routes::first_open`, each within `routes::ATTEMPT`).
- **Challenge and response, both ways, per stream.** After the header
  the destination sends a fresh nonce; the origin signs the fleet's
  genesis, origin, destination, channel, path and both nonces with its
  member key in its own domain (`route::DOMAIN`, apart from the link
  proof's), and the destination proves itself back the same way. Fresh
  nonces stop replays; the origin refuses a relay posing as the
  destination. Every refusal names the hop that made it.
- **Relaying isn't access.** A relay carries a stream from any proven
  link peer to any member of its log or held introductions, whatever its
  own trust setting; revoked keys and unknown names are refused.
- **The destination serves a routed origin as a direct peer.** The key
  it proved, its fleet root `fleet/<origin>/`, its introduction's cap, no
  agent; the trust table and `fleet.*`'s key checks
  (`lent::admit_fleet`) judge the origin, never the relay.
- **`AGENT` isn't routed.** Routed channels are `API`, `PTY`, `TUNNEL`
  and `TRANSFER`. The agent stays on direct links until a per-member
  agent grant (fleet step 10c): consent per destination member, keyed by
  its key, off by default, never inherited from a relay's own `agent`.

## Consequences

- A member is reachable through others with its own trust applied to
  the member that opened the stream; a `worker` hop relays as readily as
  a `full` one.
- Until fleet step 10c's end-to-end sessions, only the opening is proven:
  after it, a relay sees and can alter the stream's bytes. It can read
  and change whatever the stream carries, and so drive the API calls the
  origin makes on the destination.
- An open stream doesn't fail over: a hop going down ends it, and its
  client opens it again, over the next path that opens.
- Routing over a path is as current as this host's copy of the link
  graph; a stale graph costs failed attempts, not access.
- A routed machine's agent toggle is refused until 10c.
- Membership proposals, removals and `run --place` reach their member
  over any path, superseding 0044's "a proposal reaches the authority
  only over a direct link".

Source: docs/plans/active/fleet.md, step 10b and decisions 53-58.
