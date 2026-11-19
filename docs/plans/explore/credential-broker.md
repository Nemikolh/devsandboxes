# Credential broker

Status: explored, not agreed. The core the other credential plans build on:
[github-and-sentry-access](github-and-sentry-access.md),
[git-gateway](git-gateway.md), [agent-credentials](agent-credentials.md).
The whole design, with diagrams, is the site's
[Credentials and access](../../../site/src/content/architecture/secrets.mdx)
page (`/architecture/secrets`). Replaces the transport question of
[cli-proxy](cli-proxy.md) with its option 3, generalized.

Builds on: the helper's reserved `HTTP_PROXY` channel
([protocol](../../architecture/helper/protocol.md)), the agent relay's
routing to the newest capable bridge
([ssh-agent](../../architecture/helper/ssh-agent.md)), `AGENT` streams and
`[[agent]]` grants by member key over links and routes
([fleet](../../architecture/daemon/fleet.md)), profiles and
[ADR 0048](../../decisions/0048-a-profile-is-the-users-choice-and-only-its-name-crosses-machines.md)
(only a profile's name crosses machines; what it does is the host's own
file), signed configs
([ADR 0049](../../decisions/0049-the-authority-signs-a-config-every-member-runs.md)).

## Goal

No secret enters a container, on any machine. A tool in a sandbox holds a
*phantom* (`dsbx_<credential>_<random>`): token-shaped, bound to one
instance, useless anywhere else. Its HTTP goes to a loopback listener of
`devsbd`, over the instance's bridge, to a broker in `devsandbox serve`
that checks policy, swaps the phantom for the real credential and makes
the call. The credential's *home* (the member where you logged in) is the
only place it rests and the only one that grants it.

Non-goals here: what a given tool needs (the per-tool plans), network
isolation (a sandbox bypassing the proxy just gets no credentials; making
egress lists binding needs an internal network, not this plan).

## Shape

- **Identity is the bridge.** A stream on an instance's bridge comes from
  that instance; nothing the container sends changes it (as with notify
  records today). Across machines the instance's host signs a claim with
  its member key: `{member, instance_id, name, sandbox, root, profile,
  repos}`. The home checks the signature against the membership log and
  trusts the member for its own instances, as far as it granted that
  member. Locally there's no claim to sign.
- **In the container**: `devsbd daemon` listens on `127.0.0.1:<port>`
  (port in the boot file). Two modes per upstream:
  - *base URL*: plain HTTP from the tool (`SENTRY_URL`,
    `ANTHROPIC_BASE_URL`, git `insteadOf`), no CA; TLS starts at the
    broker.
  - *intercept*: `HTTPS_PROXY`, `CONNECT host:443`, TLS to a leaf the
    broker makes for that host.
  Each connection is `Open(HTTP_PROXY)` with a header (mode, target),
  routed to the newest bridge advertising a new `HTTP_PROXY` cap, held
  up to 1 s like agent streams. Flow control is `Window` credit, as on
  `Connect`.
- **The broker** (`serve`): terminates TLS (intercept), parses the
  HTTP/1.1 request head, decides (below), injects, calls upstream, streams
  the body both ways untouched, strips token-shaped response headers.
  Request bodies are read only when a scope table needs them (GraphQL,
  `git-receive-pack` commands), and only their head.
- **Phantoms**: made at `run`, kept on the `Instance` in `state.toml`,
  dropped at `rm`. Not secret, so they go on the container env (`-e`) and
  the files a claim names (written by devsbd at boot). A phantom arriving
  on another instance's bridge is refused and logged: leak detection for
  free.
- **Lifetimes**: the phantom lives until `rm`. A credential at its home
  doesn't expire for us. A token minted from it is renewed behind the
  phantom (before expiry, or once on a 401) while the request waits; a
  tool never sees an expiry. Delegated tokens (per-tool plans) expire at
  most as the upstream says: the backstop for a member gone before `rm`
  reaches it.
- **`credentials.toml`**, next to `state.toml`, read only on the home:
  per credential `source` (OS keyring; an external store such as
  `op://…` later), `profiles`, `members` (who may broker it, like
  `[[agent]]`), `delegate`, `max` scopes, and rules (remembered answers,
  hand-written ones). Nothing in a config can widen it.
- **Policy** per call, first refusal answers: instance known → credential
  (phantom or claimed host) → granted to this profile and member → asked
  for in the config's `access.<credential>` (request ∩ grant, also
  checked at `run`) → the feature's scope table maps the route to a scope:
  allow, ask or deny, deny wins, unknown routes are scope `other` (ask).
- **`devsandbox login <credential>`** on the home: device flow, browser
  OAuth or paste, per the claim's `login`; stored in the keyring.
  `devsandbox access ls|revoke` lists grants, remembered answers and the
  live minted tokens.
- **The member CA**: an ed25519 key and self-signed CA per member, beside
  `member.key` (and covered by `key_guard`). Name-constrained to the hosts
  some claim intercepts; a new host reissues it, which is drift on
  instances that trust it. Trust goes in at create: the CA into the
  image's system bundle (written as root over the helper, then
  `update-ca-certificates` or the distro's equivalent) and on every exec
  `NODE_EXTRA_CA_CERTS`, `DENO_CERT` (they add) plus `REQUESTS_CA_BUNDLE`
  pointing at a combined bundle (it replaces). Go (gh) reads the system
  bundle on Linux: no custom build. Java and compiled-in roots are the
  feature's problem or base-URL only.
- **Permission requests**: an `ask` holds the request (120 s default) and
  raises a request on the home: `access.requested` notifications,
  `access.pending` / `access.decide` methods, a dashboard modal, a desktop
  popup, `devsandbox access pending|approve|deny` (the
  `machine pending|approve|deny` shape). Answers: once, this instance (until
  `rm`), always (a rule in `credentials.toml`), deny (remembered). Stored
  in `access.json`. Not built on the Inbox, which is frozen and going away
  ([status](../../status.md#planned)).
- **Signed policy**: rules the authority signs and members install like
  configs. A home applies them only if it opts in, and only within its own
  grants.
- **Fleet**: a `CRED` channel on links and routes, opened by the
  instance's host towards the home and answered while the home grants that
  member (read per stream, like `daemon::agent_for`). The home returns the
  upstream response; the token never leaves it. Home unreachable: `503`
  with `Retry-After`, the reason in the body.
- **Audit**: one JSON line per call in `access.log` on the home
  (instance, credential, route, scope, verdict, who decided); per instance
  in the dashboard beside its requests.

## Ideas weighed

| question | option | verdict |
|---|---|---|
| where TLS ends | in the container (devsbd with a CA key) | fails: the key would be in the sandbox |
| | at the instance's host | chosen: one CA per member, the key stays there, local calls stay local |
| | at the home | every handshake crosses the fleet, and the home needs every member's claims |
| how calls reach the broker | `HTTP_PROXY` channel over the bridge | chosen: the bridge is the identity, works on every runtime |
| | container → host `RPC` | a new transport for byte streams the mux already carries |
| | a `tools` service container ([cli-proxy](cli-proxy.md) option 1) | identity is network membership: any container on the network is anyone |
| base-URL routing | path prefix (`/sentry.io/…`) | some tools drop a base URL's path |
| | a loopback port per upstream | chosen unless ports run short: no tool cooperation needed |
| | `Host` header | tools send the loopback host |
| instance identity | from the bridge | chosen: unforgeable from inside, nothing to steal |
| | a client cert per instance | the key would be in the sandbox |
| | by IP on a network | spoofable, breaks with host networking |
| beyond credentials | devsandbox as an OIDC issuer: signed instance tokens for services that accept OIDC (cloud IAM, Vault) | later; the claim is already most of a token |

## Rollout

1. Phantoms on the instance, the `HTTP_PROXY` cap and channel end to end,
   base-URL mode, the broker with static rules from `credentials.toml`,
   profiles gating grants, `access.log`. First user: sentry-cli
   ([github-and-sentry-access](github-and-sentry-access.md)).
2. `devsandbox login`, keyring storage, `access ls|revoke`.
3. The member CA, intercept mode, trust installed at create.
4. Permission requests: API, CLI, dashboard modal; remembered answers.
5. `CRED` over links and routes; claims signed by the instance's host.
6. Signed policy.

## Open questions

- Dependencies: no TLS stack today. `rustls` with `ring` (the
  [mtls-links](../active/mtls-links.md) choice, shared), `rcgen` for
  leaves, `httparse` for request heads, an upstream client (`ureq`, or
  hand-rolled over rustls).
- Host networking: a sandbox on `--network=host` shares the host's
  loopback, so devsbd's listener would be the host's. Refuse brokering
  there, or a unix socket plus a tiny in-container `CONNECT` shim?
- HTTP/2 upstream (gRPC-ish APIs) and WebSockets through intercept mode.
- Keyring off a desktop session (headless boxes): a file store sealed
  to the member key?
- Windows hosts: the broker lives in `serve`, unix-only like the relay.
