# Mesh services: one `global` service for the whole fleet

Status: agreed direction, nothing built. Answers the fleet plan's P10 and
its open question on services ([fleet](fleet.md#open-questions)).

A `scope = "global"` service is one container per config root *per host*
(`devsandbox-svc-<project>-<name>` on `devsandbox-net-<project>`,
`project_id` the hash of the root's path; `commands::services`,
[services](../../architecture/instances/services.md#scope-and-naming)).
An instance reaches it as `<name>:<port>` because it joins that network
(the `--network-alias` of `service_alias_args`, or `wire_service_dns`'s
`/etc/hosts` on Apple `container`). Nothing crosses a host: two children
of one dispatcher placed on two members see two postgres, and a placed
instance's `global` service is refused under the worker policy anyway
(`global-services`, [worker policy](../../architecture/daemon/fleet.md#worker-policy)).

## Goal

A `global` service of an authority-signed config root
([ADR 0049](../../decisions/0049-the-authority-signs-a-config-every-member-runs.md))
runs once in the fleet, and every instance whose config links it reaches
it as `<name>:<port>` on whatever member it runs, the bytes between
members encrypted because they ride the peer links (or routed streams).
This is devsandbox's kube-proxy: a stable name in every container,
connections carried to wherever the one backend runs.

Out of scope: `isolated` services (they already go where their instance
goes) and global services of host-local roots (unchanged, per host).

## Compared with a Kubernetes Service

| Kubernetes | here |
|---|---|
| Service + ClusterIP, DNS `db.ns.svc` | the service name, resolved in the container to a loopback address its helper listens on |
| kube-proxy on each node (iptables/IPVS: ClusterIP → endpoint pod IPs) | the member's daemon: an in-container listener's connections → the service's home member |
| flat pod network, routed by the CNI | no shared network across hosts: peer links and routed streams |
| pod-to-pod traffic in the clear unless a CNI with WireGuard/IPsec or a mesh's mTLS is opted into | every cross-member byte inside a link (ssh today, [mtls-links](mtls-links.md) next) or an end-to-end routed session ([ADR 0046](../../decisions/0046-routed-streams-are-end-to-end-sessions.md)); no opt-in |
| NetworkPolicy, opt-in | only instances whose config links the service get a listener; the home checks the caller member |
| Endpoints: many replicas | one backend (a stateful service is the case that matters) |

More of the concept map: [kubernetes](../../reference/kubernetes.md).

## Shape

### Identity and home

A mesh service is `(root, service)` with `root` the signed root's name
(`membership::check_root_name`, the `config` op's `root`), never
`project_id`: each member's copy lives at its own
`<data>/devsandbox/mesh/tree/<root>/` (`mesh::tree_path`), so the path
hash differs per member while the root name and the signed bundle don't.

Its **home** is the one member that runs its container:

- **Pinned**: `home = "<member>"` on the `[services.<name>]` table, part
  of the signed bundle, so every member reads the same answer.
- **Placed**: no `home`, the control plane picks one with `fleet::place`
  the first time a referencing instance starts, preferring always-on
  members (the link graph's `always on`, as `Graph::paths` already
  prefers them), and records it. A home is sticky: a postgres owns a
  volume, and moving it is a data move (a later `service move`, not a
  re-placement).

Where a placed home is recorded is the one piece of new desired state;
single writer (the control plane) to start with, see
[mesh-state](../explore/mesh-state.md).

The home creates the container as today (`ensure_service`, its `ports`
published on loopback, `loopback_publish`), on a per-root network named
from the root name rather than `project_id` so a member's restart or a
moved copy finds it again. The worker policy doesn't apply: a signed
root is already outside it (ADR 0049); `key_guard` still runs.

### The local endpoint in each container

Every instance on any member that lists the service, the home included,
reaches it through one path, so a home move or rebuild never rewires a
container.

- **Name**: `<name>` resolves to a loopback address of its own,
  `127.0.<n>.<m>` allocated per service in the instance (all of 127/8 is
  loopback on Linux, Apple `container`'s VMs included): `--add-host` at
  create on docker/podman, the `/etc/hosts` write `wire_service_dns`
  already does on Apple `container`. The service list is hashed config,
  so a change is drift and a `rebuild` anyway.
- **Listener: the helper** (proposed). `devsbd daemon` binds
  `<addr>:<port>` for each of the service's ports, told by the host over
  `RPC` (a `listen.set` method, additive per
  [ADR 0023](../../decisions/0023-helper-protocol-grows-additively.md)).
  Each accepted connection becomes a daemon-opened stream toward the host
  on a new flowing channel `SERVICE`, its first line naming `(service,
  port)`, as `SSH_AGENT` streams are opened today
  ([protocol](../../architecture/helper/protocol.md#channels-open); the
  reserved, unused `HTTP_PROXY` shows the slot was meant for this). The
  host learns which instance asked from the bridge the stream came on,
  which per-instance authorization and accounting need.
- **Alternative: a proxy container** per `(root, service)` on each
  member, on the root's network with `--network-alias <name>`, running
  the static helper as the listener. The container keeps today's DNS
  path and costs one container per member instead of a listener per
  instance, but needs an image (a `scratch` + `devsbd` build per host),
  loses the per-instance identity of a stream, and is one more thing
  `gc` and `rebuild` manage.

### Carrying a connection

The host side reuses the forward engine
([forwards](../../architecture/daemon/forwards.md#the-engine-forward)):
per `(root, service, port)` with a live listener, one `Forward` whose
source is the instances' `SERVICE` streams rather than a TCP listener,
each becoming a `Connect` on its bridge's mux. Its `Opener`:

- **Home elsewhere**: `commands::tunnel::remote_opener` to the home,
  `fleet::route::connect` choosing `fleet-<home>.sock` or
  `route-<home>.sock`, so a home with no direct link is reached over the
  [routed streams](../../architecture/daemon/fleet.md#routed-streams)
  with no new routing. The `TUNNEL` request grows a mesh form, `{"mesh":
  <root>, "service": <name>, "port"}`, beside today's `{root, instance?,
  service?, port}`.
- **Home here**: `local_opener`, the same bridge into the target.

On the home, `fleet::tunnel::serve` answers the mesh form by bridging
into the service: today's injection fallback (`devsbd` installed in the
service container, which needs `/bin/sh`, a root `exec` and a writable
`/run`, `port::plan`) or, where the image can't take it, a bridge through
the proxy container above. The engine's supervisor re-opens a dead
stream as it respawns any bridge, so a link flap or a service rebuild
heals without touching the instances.

### Authorization

- **On the instance's member**: a `SERVICE` stream is served only when
  the instance's merged config lists the service and the service is
  `global` in a signed root that member holds at the log's head
  (`mesh::check_root`); otherwise the stream closes with why, logged.
  The member vouches for its containers, as a kubelet does for its pods.
- **On the home**: the mesh `TUNNEL` form is checked against the stream's
  proven peer (`lent::Peer`: a link's proof, or a routed stream's
  origin, never a relay): a member of the log (an introduced, capped one
  refused), the root signed and declaring the service `global`, the port
  one of the service's. The trust table admits it under `worker` too,
  for this form only: serving a signed root's service is what the
  signature consented to (ADR 0049); today's `{root, …}` form keeps its
  `tunnel::confine` gate.
- Nothing about the instance crosses: the home trusts the member's word
  about which of its instances asked. Finer grants (members allowed per
  service) are an open question.

### Lifetime

References are derived, never stored
([ADR 0010](../../decisions/0010-services-isolated-by-default-refcounts-derived.md)),
but a fleet's are spread over members that sleep:

- The home keeps the service while the head of its root's signed config
  declares it. Laptops asleep with referencing instances, or none
  running, don't remove a stateful service. `gc` on the home removes a
  mesh service only once the signed config drops it (or the root leaves
  the log); `service rm` (new) is the explicit removal.
- Every other member's `gc` treats the per-member pieces (listeners,
  the engine, a proxy container) by its own instances, as today's
  `service_refs`.
- Start and stop follow the home only: `stop` of an instance leaves the
  service running, as a `global` one does today.

### Rebuild

`service rebuild <name>` on a mesh service is a verb on its home
(forwarded with `machine` from anywhere, a routed one included). The home
recreates the container ([service
rebuild](../../architecture/instances/rebuild-and-drift.md#service-rebuild));
connections open across it break, new ones reach the new container
through the healed bridge. No container elsewhere changes. A signed
config version that changes the service's hash is drift on the home.

### Failure modes

| what | what a client in a container sees | surfaced |
|---|---|---|
| home down or asleep | connect accepted, then reset (the engine has no bridge) | `service ls` status `unreachable via <home>`, the engine's `Error` note |
| no link and no path to the home | same | the route failure (`routes::failure`) in the note |
| home's container missing or rebuilding | reset until the bridge reopens | `missing` on the home's row |
| this member's daemon down | connection refused (no listener: the helper's listeners follow the host's bridge) | nothing in the container; `serve.log` |
| home asleep for good | everything above, until the service is re-homed | the control plane's row for the service |

A member with referencing instances keeps its daemon alive while a
listener is set (like a held link), so a service doesn't vanish because
the daemon idled out.

## Rollout

1. **Helper listener.** `listen.set` over `RPC`, the `SERVICE` channel
   and its daemon-opened streams; the host serves them from a test
   target. Helper tests with a loopback target.
2. **Local engine.** The `Forward` source from `SERVICE` streams; the
   home-here opener; `--add-host` / `/etc/hosts` names; a mesh root's
   `global` service on one member goes through it end to end.
3. **Homes.** `home` in the service table (config spec skill updated),
   placed homes recorded by the control plane, `service ls` showing the
   home.
4. **Remote.** The mesh `TUNNEL` form, its gate on the home, the remote
   opener through `route::connect`; docker tests with two in-process
   members (`Options::link_transport`).
5. **Lifetime.** `gc` and `service rm` by the signed config; the
   keep-alive while listeners are set; `service rebuild` forwarded to the
   home.
6. **Surfaces.** `service ls` and the dashboard's Services view across
   members; `devsandbox port` to a mesh service (it already takes
   `service`); docs and an ADR.

## Open questions

- A new `scope = "fleet"`, or `global` meaning fleet-wide for signed
  roots only? The latter fixes P10 with no config change but makes one
  word mean two things by where the root came from.
- Always through the proxy, even on the home? Uniform (a home move never
  rewires) against an extra hop on the home's own instances.
- Helper listener or proxy container, and for the home's side, injection
  or the proxy container? Distroless service images rule injection out.
- Who records a placed home, and what happens when its member is gone
  for good: re-home empty (a cache) automatically, never a stateful one?
  A `stateful = true` flag?
- UDP (DNS-ish services, some game servers): forwards are TCP only
  ([status](../../status.md)).
- Per-service grants on the home (members allowed), or is "any member of
  the log under a signed root" enough?
- Latency-sensitive services (redis) across a laptop's Wi-Fi: warn when
  a home is far from its users, or place the home near them?
