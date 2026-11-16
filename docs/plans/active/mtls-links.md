# mTLS links: peer links without ssh

Status: agreed direction, nothing built. Answers the fleet plan's open
question "Authenticating peers without ssh" ([fleet](fleet.md#open-questions))
and the rest of P11.

Every peer link is an ssh today: the dialer runs `ssh -T -o
BatchMode=yes -- <host> devsandbox fleet-link --peer <me>`
(`fleet::transport::Transport::ssh`, `ssh::target_argv`), and the link's
mux, [key proof](../../architecture/daemon/fleet.md#key-proof) and
channels run over that process's stdio
([transport](../../architecture/daemon/fleet.md#transport),
[peer links](../../architecture/daemon/fleet.md#peer-links);
[ADR 0020](../../decisions/0020-machines-over-plain-ssh-with-protocol-match.md)).
That costs:

- **A shell per dialer.** A member that dials another holds a login
  there, so `worker` trust protects nothing from it: the whole
  [exposure](../../architecture/daemon/fleet.md#exposure) computation
  exists because `dials` means "has a shell".
- **sshd on every dialed member**, a user account, keys in
  `authorized_keys`, `known_hosts` first-use trust, and a login shell's
  noise skipped up to a sentinel (`ssh::skip_lines`).
- **Two authentications.** ssh authenticates the hop, the key proof the
  member; the proof isn't bound to the pipe, which is why a `Transport`
  owes an authenticated far end.
- **Custom crypto for routed streams.** ADR 0046's SIGMA over x25519,
  HKDF and ChaCha20-Poly1305 records (`src/serve/fleet/session.rs`,
  `route::origin_side` / `route::dest_side`): correct by review, but ours
  to maintain.

## Goal

A peer link is one TLS 1.3 connection, both ends authenticated by their
member keys, checked against the membership log. The link logic stays:
whoever can reach dials, the link carries both directions, so a laptop
behind NAT never listens (the shape Kubernetes' Konnectivity gives node
agents dialing out to the control plane, with apiserver traffic tunnelled
back). Routed streams carry an inner TLS session end to end instead of
ours. ssh stays for what it does well: `machine add`'s install and
first contact (decided).

## Shape

### Transport

`fleet::transport::Transport` is already the seam: it owes
confidentiality, integrity and an authenticated far end, and "swapping
ssh changes that module and nothing that dials". It gains a second
implementation, `Transport::tls`, over `rustls`:

- **The dialer** (`fleet::dialer::Dialer::hold`) connects TCP to the
  member's address, runs the TLS client handshake, and hands the link a
  `Duplex` over the TLS stream. `Transport::dial(target, program, args)`
  is ssh-shaped (a command on a host); it becomes `dial(peer)` with the
  ssh form building its argv inside.
- **The listener** is new: the daemon accepts TLS on its listen address
  and runs the dialed end in-process, the role `commands::fleet_link::run_peer`
  plays as ssh's child today (the opener of `fleet::LINK`, streams to
  the daemon as `dialer::Open` does on the other side, no `link.attach`
  hop). `fleet-link --peer` stays for ssh links.
- **No sentinel**: no login shell prints anything; the `Spec`'s `Hello`
  exchange starts at the first byte past the handshake (as `Over::Link`
  already does inside a stream).

The mux, channels (`API`, `TRANSFER`, `PTY`, `TUNNEL`, `AGENT`, `ROUTE`),
keepalive, backoff (`service::backoff`), the link graph's reports and
every caller are unchanged. A link report's `dials` stays, but means
only "this end connects" on a TLS link: no shell comes with it, so
[exposure](../../architecture/daemon/fleet.md#exposure)'s dial rule
applies to ssh links alone.

### Certificates: CA-issued or pinned

The member keys are ed25519 (`fleet::identity`, `member.key`); TLS 1.3
signs its `CertificateVerify` with ed25519 in rustls on either crypto
provider. What a peer presents, two ways:

| | **authority as a CA** | **pinned by the log** (proposed) |
|---|---|---|
| what | the authority issues an X.509 cert per member, `CN`/SAN its member name, signed by the authority's key | each member presents a self-signed cert (or an RFC 7250 raw public key, which rustls supports) of its own member key |
| verifier | standard chain validation to the fleet CA, then a revocation check | custom `ServerCertVerifier` / `ClientCertVerifier`: the presented key must be the one the stored log (or a held introduction) gives the expected name |
| revocation | needs a list anyway: the log's `revoke` | the log's `revoke`, read on every handshake; nothing else |
| rotation | a `rotate` needs a new cert issued, so a member mid-rotation depends on the authority being up | `rotate` in the log is the whole story; the next key is presented once the log lists it (`Identity::signer_for`) |
| expiry | short-lived certs need renewal; a laptop asleep past expiry links with no one until it reaches the authority | none: the log is the lifetime |
| deps | `rcgen` (issuing), `rustls-webpki` path building | only rustls; a DER SubjectPublicKeyInfo of a raw ed25519 key is a fixed prefix |
| fits | standard tooling can inspect a fleet's certs | the authority already is the fleet's CA: the log *is* signed name → key bindings |

The CA form buys interoperability we don't need and adds the one
failure mode a fleet of sleeping laptops can't afford (expiry). The
pinned form is the key proof's model moved into the handshake.

**Membership after the handshake.** The verifier runs before the
membership sync, and today's proof resolves the peer's name only after
merging what the peer carried (a new member's key may be in its log, not
ours yet). So the handshake proves possession of a key and records it;
the link's first rounds (the proof's `hello` and `sync`, kept, inside
TLS) merge the logs; then the recorded key must be the one the merged
log gives the expected name, or the link ends with why, as today. The
proof's `proof` and `confirm` rounds go: TLS bound the key to the
channel, which the proof never could. `proof::Proven` and everything
reading it (`lent::Peer::key`, grants, `link.attach`'s params) don't
change. SNI carries the expected member name; the client checks the
server's key against it the same way.

### Routed streams

Past its header (`route::Header`, kept: relays need it in the clear, and
`Header::check` / `route::came_from` / the TTL stay as they are) a
`ROUTE` stream runs a TLS 1.3 handshake between origin (client) and
destination (server), each side's verifier as on a link, with the
expected names from the header. Relays splice it unread
(`route::splice`), as they carry ADR 0046's records now.

This retires `src/serve/fleet/session.rs` (`Ephemeral`, `Keys::derive`,
`Sealer`, `Opener`, `pump`) and the DH half of `route::origin_side` /
`route::dest_side`, and with them the `x25519-dalek`, `hkdf` and
`chacha20poly1305` dependencies. What changes in behavior: TLS's
`close_notify` closes one direction authenticated, so a routed stream
can half-close like a direct one (ADR 0046 tears both down at any end).
The transcript's binding to the fleet (the genesis hash in
`route::transcript`) moves to the verifier: a key resolves only in this
fleet's log.

A new ADR supersedes 0046's mechanism and 0020's transport.

### Listening

- **Who listens**: a member with `listen` set in `member.toml` (the
  daemon is its only writer: `machine set <m> --listen <addr>` or `serve
  install --listen`). An always-on member listens; a laptop doesn't, and
  dials. A member that listens nowhere is reached only over links it
  dials, as an inbound neighbor is today.
- **Where**: the tailnet address by default (`100.x.y.z:<port>`), never
  `0.0.0.0` unless asked. A Tailscale-style network is the baseline
  assumption: reachability (NAT traversal, stable addresses) is its job,
  not ours; TLS adds member identity on top of the tailnet's device
  identity.
- **How others find it**: the member's signed link report carries its
  listen address (`graph::Report`, under the report's signature), so a
  member learns where to dial from the graph; `machines.toml` entries
  gain `addr` beside the ssh target.
- **Version handoff**: the successor daemon must take the listener over
  ([ADR 0015](../../decisions/0015-newest-binary-wins-version-handoff.md)).
  Today the dialed ends are `fleet-link` processes that outlive a
  handoff; in-process TLS links end with the old daemon and redial (1 s
  backoff). Either accept that, or pass the listening socket to the
  successor at the handoff.

### First contact and install

`machine add` keeps its ssh path: the install check and binary copy
(`machines/install.rs`), the add-time probe (`machines.probe`, `api
--stdio --sentinel`) where the fingerprint is confirmed and the logs are
signed and adopted ([fleet](../../architecture/daemon/fleet.md),
*How a machine joins*). The probe also exchanges listen addresses; once
both ends hold the log, the held link is TLS when the target listens,
else ssh as today. A k3s-style token join with no ssh at all is explored
in [join-tokens](../explore/join-tokens.md).

### Dependencies and size

Nothing TLS is in the tree today: `Cargo.toml` has `ed25519-dalek`,
`x25519-dalek`, `hkdf`, `chacha20poly1305`, `sha2`, `getrandom`;
`Cargo.lock` has no `rustls`, `ring` or `aws-lc-rs`; release downloads
shell out to `curl` (`fleet::update::curl_argv`). Options:

- `rustls` with **`ring`**: small, pure Rust plus assembly, cross-builds
  for every target we ship without a C toolchain beyond `cc`.
- `rustls` with **`aws-lc-rs`** (its default): FIPS option, larger, needs
  cmake (and clang on some targets) to build.
- `rustls-rustcrypto`: a provider over the RustCrypto crates already in
  the tree; least new code, not production-ready.

Proposed: `ring`, `default-features = false`, TLS 1.3 only, one cipher
suite list, measured against the release binary before and after (the
dropped crates offset part of it). The helper (`devsbd/`, no dependencies
by [ADR 0022](../../decisions/0022-helper-has-no-dependencies.md)) is
untouched: TLS is host to host only.

## Rollout

1. **`Transport` reshaped**: `dial(peer)`; ssh behind it, no behavior
   change.
2. **TLS transport and verifier**: rustls with ring, the log-pinned
   verifier, a TLS link between two in-process members in tests
   (`Options::link_transport`); the proof reduced to `hello` + `sync` +
   the post-sync key check on TLS links.
3. **Listener**: `listen` in `member.toml`, the in-process dialed end,
   addresses in link reports, `machines.toml` `addr`; the dialer prefers
   TLS when the peer listens.
4. **Routed streams over inner TLS**; `session.rs` and its crates
   removed; the ADR.
5. **`machine add`** exchanging addresses on the probe; ssh links remain
   for members that don't listen.
6. **Docs**: [fleet](../../architecture/daemon/fleet.md) (*Transport*,
   *Peer links*, *Key proof*, *Routed streams*), machines, the
   [kubernetes](../../reference/kubernetes.md) map's transport section.

## Open questions

- Pinned or CA? The table argues pinned; anything (a browser, `curl`
  against a member's API) that would want a CA chain?
- Raw public keys (RFC 7250) or a self-signed cert wrapping the key? RPK
  is cleaner, less tooling understands it.
- Default port, and whether `listen` should follow the tailnet address
  when it changes.
- Keep ssh links forever for members that can't listen, or require one
  listener per fleet (the control plane, say) and have everyone dial it?
- Listener handoff: socket passing, or links cycle on every upgrade?
- Does a `worker` member that dials nothing and runs no sshd count as not
  exposed at all? Then [exposure](../../architecture/daemon/fleet.md#exposure)
  shrinks to ssh links, and `machine trust` can say so.
- Off-tailnet use (a public address): fine with mTLS alone, or require
  the tailnet?
