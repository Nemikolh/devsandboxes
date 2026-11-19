# Credential broker

Status: explored, not agreed. The core the other credential plans build on:
[github-and-sentry-access](github-and-sentry-access.md),
[git-gateway](git-gateway.md), [agent-credentials](agent-credentials.md).
The whole design, with diagrams, is the site's
[Credentials and access](../../../site/src/content/architecture/secrets.mdx)
page (`/architecture/secrets`). Supersedes the parked CLI-shim plan
(`cli-proxy`, deleted), whose HTTP-over-the-helper option this
generalizes.

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
only place it rests and the only one that grants it. Log in on an
always-on member (a box, not a laptop) for credentials remote sandboxes
use: a static one is brokered only while its home is reachable.

Non-goals here: what a given tool needs (the per-tool plans), network
isolation (a sandbox bypassing the proxy just gets no credentials; making
egress lists binding needs an internal network, not this plan), and
sandboxes that are the host: one with the runtime socket bound or running
`privileged` can read its host's files and use any instance's bridge.
`run` warns when such a sandbox has claims, and `access ls` marks it.

## Shape

- **Identity is the bridge.** A stream on an instance's bridge comes from
  that instance; nothing the container sends changes it (as with notify
  records today). Across machines the instance's host signs a claim with
  its member key: `{member, instance_id, name, sandbox, root, profile,
  repos, claim}` (`claim`: the digest of the instance's claim for that
  credential; plus `placer`, the member key of the peer that placed it,
  if one did), per `CRED` stream over the proven link. The home checks the
  signature against the membership log and trusts the member for its own
  instances, as far as it granted that member: the home enforces the
  grant, `max`, the profile's name, the bound claim and every route's
  scope; the rest (`instance_id`, `name`, `sandbox`, `root`, `profile`,
  `repos`, `placer`) is the member's word, so what the home remembers for
  an instance (answers, denials, rules) is keyed by the member key with
  it, never by a bare instance id or sandbox name. Locally there's no
  claim to sign.
- **The repo fence** (`repos`) is the host's read of the folders' remotes
  (`host_git`; `url.*` stays refused, so no `insteadOf` rewrites it) at a
  folder's first `run`, recorded per folder in `state.toml` (kept past
  `rm`) and snapshotted on each `Instance`; never re-read at `start`,
  `rebuild`, autostart or a later `run`: the checkout's `.git/config` is
  writable from the container. A remote added since counts only once the
  config names it (the one way to widen a fence).
- **In the container**: `devsbd daemon` listens on `127.0.0.1`, a port
  per upstream of each claim the instance carries (approval is checked
  per call, so a later `access approve` needs no recreate) plus one proxy
  port, from a high range below the ephemeral one (`ip_local_port_range`,
  32768 up by default) so dev servers and loopback clients never hold
  them (ports fixed at `run`; `onAutoForward: ignore` in the VS Code name config's
  `portsAttributes`; refused by `forwards.*`). The host writes the listeners and phantom files
  into a broker file of the helper's (not the boot file, which exists only
  for `autostart = "runtime"`), installed with the helper before any
  lifecycle command; every daemon start (install, `devsbd boot`, a
  bridge) reads it, so `HTTPS_PROXY` never points at a closed port. Two
  modes per upstream:
  - *base URL*: plain HTTP from the tool (`SENTRY_URL`,
    `ANTHROPIC_BASE_URL`, git `insteadOf`), no CA; TLS starts at the
    broker.
  - *intercept*: `HTTPS_PROXY`, `CONNECT host:443`, TLS to a leaf the
    broker makes for that host. Only bound upstreams go to the broker
    (the `CONNECT` host case-folded, trailing dot dropped, before
    matching);
    devsbd dials any other `CONNECT` target itself, from the container
    (its DNS, its networks, through the proxy the container had if
    any: the config's `containerEnv` or the image's ENV,
    `facts::image_facts`, recorded in the broker file; `run` warns when
    `remoteEnv` sets a proxy variable, which would bypass the route).
    Both cases of `HTTPS_PROXY` / `NO_PROXY` (loopback and the
    services' aliases) and `NODE_USE_ENV_PROXY=1` (Node 22.21+ / 24;
    older Node: base URL only) go on the container env with the phantoms
    and the CA variables: a process that holds a phantom has the route
    too, VS Code terminals and the container's own processes included.
  Each connection is `Open(HTTP_PROXY)` with a header (mode, target),
  a hint the broker checks against the instance's bound claims and the
  phantom, routed to the newest bridge advertising a new `HTTP_PROXY`
  cap, held up to 1 s like agent streams. Flow control is `Window`
  credit, as on `Connect`.
- **The broker** (`serve`): terminates TLS (intercept; ALPN `http/1.1`
  only), parses **every** request on a connection with strict framing
  (ambiguous `Content-Length` / `Transfer-Encoding` refused; `Upgrade`
  refused until designed; `Expect: 100-continue` answered after policy),
  decides (below), injects, calls upstream with redirects off
  (`Location` passed back, rewritten to the loopback in base-URL mode
  when its host is a bound upstream, as is otherwise) and
  `Accept-Encoding: identity`, streams the body both ways, strips
  token-shaped response headers and redacts the injected value wherever
  it comes back (an exact-match pass over headers and body, for the value
  and its basic-auth base64, carrying the last token-length bytes over
  between chunks). An upstream address that resolves to loopback,
  link-local or private/ULA ranges is refused at connect, checked on the
  address dialed (so DNS rebinding gets nothing), unless the bound claim
  marks that upstream private (a self-hosted Sentry on the LAN): no
  feature turns the broker into a way into its host's network. Request
  bodies are read only when a scope table needs them (GraphQL's parsed document, `git-receive-pack`
  commands); a `Content-Encoding` one is decoded within the body bound,
  and one that doesn't decode or parse is denied. Requests with an `Origin` or `Sec-Fetch-Site` are refused,
  and so is a `Host` other than the listener (base URL) or other than the
  `CONNECT` target and SNI (intercept): a browser in the sandbox can't
  drive the broker. Its own upstream calls use the host's proxy env.
- **Phantoms**: made at `run`, kept on the `Instance` in `state.toml`,
  dropped at `rm`. Not secret, so they go on the container env (`-e`) and
  the files a claim names (written by devsbd from its broker file, never onto a mount
  or volume target nor over a file that isn't a phantom: `run` warns and
  skips such a file, as a shared tool login mounted rw would be
  overwritten and leak-flagged by every other instance). Every injected
  request must carry the instance's phantom, in both modes, and the real
  value replaces it only in the bound claim's `inject` slot: a phantom
  anywhere else (URL, other headers, a body) goes upstream as is, never
  swapped, so no posted text stores the token. Without one it goes
  upstream uninjected (git gets a 401 first, so its credential helper
  hands over the phantom). A phantom arriving on another
  instance's bridge is refused and logged: leak detection for free.
- **Lifetimes**: the phantom lives until `rm`. A credential at its home
  doesn't expire for us. A token minted from it is renewed behind the
  phantom ahead of expiry, with a margin longer than a long request, so a
  tool never sees an expiry. A 401 anyway renews and replays the request
  only when its body is held (none, or under a bound); a streamed body
  past that can't be replayed, and the 401 reaches the tool with the
  reason in the body. Delegated tokens (per-tool plans) expire at most as
  the upstream says, the backstop for a member gone before `rm` reaches
  it, and are refreshed ahead of expiry the same way. The home's own
  credential failing (refresh rejected, an upstream 401 on a freshly
  renewed or static one, the store unreadable) is no 401 to the tool,
  which would send it to the tool's own login: a `403` broker refusal
  naming `devsandbox login <credential>` and the home, the credential
  marked broken in `access ls` and its `approvers` notified until a
  login.
- **`credentials.toml`**, next to `state.toml`, read only on the home:
  per credential `source` (a file store beside `member.key`, 0600, the
  default on Linux, where a headless box or a user unit started before a
  desktop session has no unlocked keyring; the OS keyring on macOS; an
  external store such as `op://…` later), the approved claims (each
  version's `upstreams`, `inject`, `rewrite`, scope table, `never` routes
  and private upstreams, keyed by the digest of that canonicalized claim,
  not the feature artifact's, as approved at `login`; an instance pins the
  one it ran with, so approving a new version never breaks a running
  instance, and a version goes with its last instance or by `access
  revoke`), `profiles`, `members` (who may broker it, like
  `[[agent]]`), `approvers` (member keys that may answer its asks),
  `delegate`, `max` scopes, and rules (remembered answers, hand-written
  ones). A `members` entry gives that member the whole grant unless it
  carries its own `max`. Nothing in a config can widen it. One feature
  per instance claims a given credential: a second is refused at `run`.
  `key_guard` matches files by name, so it lists this file, the file
  store, the CA key and `access.json` beside the member key's.
- **Policy** per call, first refusal answers, evaluated by whoever
  injects (the home for a remote instance), for every request, injected
  or not: instance known → the target host is an upstream of one of the
  instance's claims and that claim is an approved version (else a
  refusal naming `devsandbox access approve <credential>`; checked at
  `run` too, refused there when the home answers, a warning when a remote
  home is offline) → credential (by phantom; none: upstream uninjected,
  the steps below skipped) → the target host is an upstream of that
  credential's bound claim (a phantom sent to another claim's upstream,
  on its listener or the shared proxy port, is refused and logged, as a
  foreign one is) → granted to this profile and member →
  asked for in the
  config's `access.<credential>` (request ∩ grant, also checked at `run`;
  a remote home offline then is a warning, the per-call check stands) →
  the fence, for credentials that have one: the scope table attributes
  the route to a repo (normalized: case-folded, `.git` stripped, `..` and
  encoded separators refused, `/repositories/{id}` resolved) or lists it
  as repo-less; a route it can't place, unknown ones included, is
  denied, and a repo outside the fence gets no credential, never asked:
  the request goes upstream with the phantom stripped, from the
  instance's host, as to a stranger (public repos work, under the
  upstream's anonymous rate limit, 60 requests an hour per IP on
  GitHub's REST API); its response carries `devsandbox-fence: outside`,
  and an upstream `401` becomes the broker's refusal naming
  `access.<credential>.repos`, so no tool is sent to its own login → the
  bound scope table maps the route to a scope: allow, ask or deny, deny
  wins; for a credential without a fence, unknown routes are scope
  `other` (ask). Routes marked `never` (ones that mint
  credentials, returned in the body): no ask or rule opens them.
- **Who's asking**: `access.*` and `credentials.*` are local clients'
  only, `denied` to peers under either trust (as `member.*` is, in
  `lent::admit`), except from a credential's `approvers`: the home pushes
  them `access.requested` over their links and admits their
  `access.pending` / `access.decide` and `credentials.login` (the secret
  over the proven link) for that credential, so a laptop answers for a
  headless home. A placed instance's grant is its host's ∩ its
  `placer`'s (by key, never by its `fleet/<peer>/` root, whose name is
  the host's own), so a host can't borrow a wider grant by naming
  another placer; a `full`-trust peer is the user otherwise. A profile change re-checks
  grants.
- **`devsandbox login <credential>`** on the home or an approver: device
  flow, browser OAuth or paste, per the claim's `login`; stored in the
  home's store, the claim bound beside it.
  `devsandbox access ls|revoke` lists grants, remembered answers and the
  live minted tokens.
- **The member CA**: an ECDSA P-256 key and self-signed CA per member
  (ed25519 stays the member identity: browsers and other verifiers
  refuse Ed25519 certificates), beside `member.key`. Name-constrained to
  the hosts some claim intercepts; a new host reissues the cert over the
  same key and re-installs it in place over the helper, no drift. Leaves
  are backdated for clock skew. Trust goes in at create: the CA into the
  container's system bundle (written as root over the helper, then
  `update-ca-certificates` or the distro's equivalent, from its anchors
  dir so later regenerations keep it), and on the container env
  `NODE_EXTRA_CA_CERTS`, `DENO_CERT` (they add) plus `REQUESTS_CA_BUNDLE`,
  `AWS_CA_BUNDLE` (they replace certifi's roots) pointing at the system
  bundle file itself, never a snapshot, so a corporate CA added later
  still counts. An image with no system bundle gets one written from a
  vendored Mozilla set (`webpki-roots`) plus the CA, never the CA alone.
  No `SSL_CERT_FILE`: Go (gh) and OpenSSL read the system bundle on
  Linux, no custom build. Java ignores `HTTPS_PROXY`, so it and
  compiled-in roots are base-URL only.
- **Permission requests**: an `ask` holds the request (the claim's
  `hold`, 120 s default, as tools' timeouts differ) and
  raises a request on the home: `access.requested` notifications,
  `access.pending` / `access.decide` methods, a dashboard modal, a desktop
  popup, `devsandbox access pending|approve|deny` (the
  `machine pending|approve|deny` shape). Answers: once, this instance (until
  `rm`), always (a rule in `credentials.toml`), deny (remembered; its
  refusal says so and names `access revoke`, so an agent stops retrying). Stored
  in `access.json`; a remote instance's "this instance" answers age out
  after 30 days unused, as the home doesn't see its `rm`. Not built on the Inbox, which is frozen and going away
  ([status](../../status.md#planned)).
- **Signed policy**: rules the authority signs and members install like
  configs. A home applies them only if it opts in, and only within its own
  grants.
- **Fleet**: a `CRED` channel on links and routes, opened by the
  instance's host towards the home and answered while the home grants that
  member (read per stream, like `daemon::agent_for`). A credential's
  home is the authority's word, never the home's own: `login` on a
  member proposes it (`fleet.propose`, as membership changes are,
  `serve::machines::changes`) and the authority-signed membership log
  names one member key per credential, followed through rotations and
  ended by a revocation. So a member can't make itself a home (it would
  read the calls and forge the answers) and there are never two. The
  home's signed link report (`graph::Report`) only says which members it
  grants, as for agents; a report naming a credential the log gives
  another member is ignored and logged, and a local copy that isn't the
  log's home goes unused, marked in `access ls`. Outside a fleet the home
  is the host. The instance's host ends TLS and
  sends the parsed head with the signed claim, and the body once the home
  accepts the call or asks for it (a scope table reading it; the host
  keeps that copy, bounded); an out-of-fence verdict has the host send
  the request upstream stripped itself. The home runs policy, injects,
  calls upstream, redacts and returns the response; the host rewrites `Location` and `rewrite`
  fields to the instance's loopback. The token never leaves the home. Home unreachable: the
  instance's host holds the request (bounded) while it redials, then
  `503` with `Retry-After`, the reason in the body.
- **Audit**: one JSON line per call in `access.log` on the home
  (instance, credential, route before injection, scope, verdict, who
  decided); per instance in the dashboard beside its requests.
- **The daemon's life**: a running instance with a claim is a
  `live_instances` holder (`host::holds_host`): the broker can't idle out
  under it.

## Ideas weighed

| question | option | verdict |
|---|---|---|
| where TLS ends | in the container (devsbd with a CA key) | fails: the key would be in the sandbox |
| | at the instance's host | chosen: one CA per member, the key stays there, local calls stay local |
| | at the home | every handshake crosses the fleet, and the home needs every member's claims |
| how calls reach the broker | `HTTP_PROXY` channel over the bridge | chosen: the bridge is the identity, works on every runtime |
| | container → host `RPC` | a new transport for byte streams the mux already carries |
| | a `tools` service container (the old CLI-shim plan) | identity is network membership: any container on the network is anyone |
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
   `devsandbox login` by paste into the store with the claim bound,
   the helper's broker file, profiles gating grants,
   `access.log`; `ask` is deny until step 4. First user: sentry-cli
   ([github-and-sentry-access](github-and-sentry-access.md)).
2. `devsandbox login`'s device flow and browser OAuth, `access ls|revoke`.
3. The member CA, intercept mode, trust installed at create.
4. Permission requests: API, CLI, dashboard modal; remembered answers;
   `approvers` (a headless home's asks answered from a laptop).
5. `CRED` over links and routes; claims signed by the instance's host;
   homes in the membership log.
6. Signed policy.

## Open questions

- Dependencies: no TLS stack today. `rustls` with `ring` (the
  [mtls-links](../active/mtls-links.md) choice, shared), `rcgen` for
  leaves, `httparse` for request heads, an upstream client (`ureq` with
  redirects off, or hand-rolled over rustls).
- Handoff and restart: a drain drops the bridges within `DRAIN_TIMEOUT`
  (5 s) so a successor never overlaps them, which cuts open broker
  streams (a model's SSE, a long push, a held ask). Let a draining daemon
  keep its bridges for open broker streams (bounded) while new ones go to
  the successor's, newest bridge first, and re-attach pending asks from
  `access.json`? In the container, an upgrade's helper takeover (`Quit`,
  up to 3 s) drops every proxied connection, claimed or not, and refuses
  new ones meanwhile: hand the listeners to the successor over
  `devsbd.ctl` (`SCM_RIGHTS`) and let the old daemon finish open
  connections, bounded, or run the proxy as its own process?
- Host networking: a sandbox on `--network=host` shares the host's
  loopback, so devsbd's listener would be the host's. Refuse brokering
  there, or a unix socket plus a tiny in-container `CONNECT` shim?
- HTTP/2 upstream (gRPC-ish APIs) and WebSockets through intercept mode.
- The file store at rest: plain 0600 like `member.key`, or sealed with
  `systemd-creds` / a TPM where there is one?
- Windows hosts: the broker lives in `serve`, unix-only like the relay.
