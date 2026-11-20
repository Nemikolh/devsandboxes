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
*phantom* (`dsbx_<credential>_<random>`, or a prefix the claim declares:
*Phantoms*): token-shaped, bound to one instance, useless anywhere else. Its HTTP goes to a loopback listener of
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
`privileged` can read its host's files and use any instance's bridge;
one with the host's display or session sockets bound (`/tmp/.X11-unix`,
Wayland, the D-Bus session bus, `$XDG_RUNTIME_DIR`, which holds
`serve`'s socket) can drive the desktop and answer any instance's asks,
`presence` included, which holds only while none runs where asks are
answered. `run` warns when such a sandbox runs on a host that brokers
any credential or is one's approver, and `access ls` marks it.
Two side doors are the other plans': ssh through the agent relay
(git-gateway filters its keys under a git-host claim) and VS Code's
attach, which forwards the host's git credentials and agent (outside
the guarantee; `devsandbox vscode` warns, git-gateway).

## Shape

- **Identity is the bridge.** A stream on an instance's bridge comes from
  that instance; nothing the container sends changes it (as with notify
  records today). Across machines the instance's host signs a claim with
  its member key: `{member, instance_id, incarnation, name, sandbox, root,
  profile, repos, claim, access}` (`incarnation`: random, minted at `run`
  and dropped at `rm`, as `unique_instance_id` hands a removed
  instance's id to the next `run` of that name; `claim`: the digest of the instance's claim for that
  credential; `access`: its config's resolved `access.<credential>`,
  read from the host's config at signing as the local check reads it, so
  an edit reaches both alike; plus `placer`, the member key of the peer that placed it,
  if one did), per `CRED` stream over the proven link. The home checks the
  signature against the membership log and trusts the member for its own
  instances, as far as it granted that member: the home enforces the
  grant, `max`, the profile's name, the bound claim and every route's
  scope (undelegated calls: *Delegation*); the rest (`instance_id`, `incarnation`, `name`, `sandbox`,
  `root`, `profile`, `repos`, `access`, `placer`) is the member's word (the home
  intersects `access` with the grant; its `repos`, which widens the
  fence, is as member-asserted as `repos`), so what the home remembers for
  an instance (answers, denials) is keyed by the member key with
  its `incarnation`, never by an instance id or sandbox name. Locally there's no
  claim to sign.
- **The repo fence** (`repos`) is the host's read of the folders' remotes
  (raw, no `insteadOf` applied, in a one-off container, never by host
  git: git-gateway, *The folder's remotes*) at a folder's first `run`, recorded per folder in `state.toml` (kept past
  `rm`) and snapshotted on each `Instance`; never re-read at `start`,
  `rebuild`, autostart or a later `run`: the checkout's `.git/config` is
  writable from the container. A remote added since counts only once the
  config names it (the one way to widen a fence); `devsandbox access
  forget <folder> <repo>` narrows it (github-and-sentry-access).
- **What a sandbox mounts**: `run` refuses a bind, read-only too, whose
  source covers a config root, a whole `shared-volumes/` dir, another
  instance's volume or worktree, or what `key_guard` guards, and warns of
  a shared writable bind other instances run code from (`cargo`,
  `pnpm-store`, or a checkout another sandbox owns, through a `folders`
  worktree's companion `.git` mount or a `worktree = "never"` bind: hooks,
  `core.fsmonitor`, build scripts), marked in `access ls` when the
  sandboxes' fences or grants differ: an instance bounded by the broker
  still runs as any instance that writes what it executes (git-gateway's
  clones drop the companion mount). It refuses `runArgs` that join
  another instance's namespaces or volumes (`--network`/`--pid`/`--ipc
  container:`, `--volumes-from`, podman `--pod`) when either has claims,
  as `worker_policy` does for worker trust. A sandbox that needs the config
  gets its files read-only, one bind each (agent-credentials). The config
  refusal holds without claims (an edit could hand a sibling or the next
  `rebuild` claims), so a config inside its own `folder` (`folder = "."`)
  stops running: move it beside the repo.
- **In the container**: `devsbd daemon` listens on `127.0.0.1`, a port
  per upstream of each claim the instance carries (approval is checked
  per call, so a later `access approve` needs no recreate) plus the proxy
  ports (one shared, git's own: github-and-sentry-access), from a high range below the ephemeral one (`ip_local_port_range`,
  32768 up by default) so dev servers and loopback clients never hold
  them (ports fixed at `run`, a rebuild adding a new claim version's; `onAutoForward: ignore` in the VS Code name config's
  `portsAttributes`; refused by `forwards.*`). The host writes the listeners and phantom files
  into a broker file of the helper's (not the boot file, which exists only
  for `autostart = "runtime"`), installed with the helper before any
  lifecycle command; every daemon start (install, `devsbd boot`, a
  bridge) reads it, so lifecycle commands never meet a closed
  `HTTPS_PROXY`; what the entrypoints start does, until `devsbd boot`
  runs after them (until `materialize` installs the helper on first
  create): a one-shot fetch there fails. Two modes per upstream:
  - *base URL*: plain HTTP from the tool (`SENTRY_URL`,
    `ANTHROPIC_BASE_URL`), no CA; TLS starts at the broker.
  - *intercept*: `HTTPS_PROXY`, `CONNECT host:443`, TLS to a leaf the
    broker makes for that host. Only bound upstreams go to the broker
    (the hosts a claim's `git` field names only on git's own proxy port,
    github-and-sentry-access; the `CONNECT` host case-folded, trailing dot dropped, before
    matching);
    devsbd dials any other `CONNECT` target itself, from the container
    (its DNS, its networks, through the proxy the container had if
    any: the config's `containerEnv` or the image's ENV,
    `facts::image_facts`, recorded in the broker file; `run` warns when
    `remoteEnv` sets a proxy variable, which would bypass the route).
    Both cases of `HTTPS_PROXY` / `NO_PROXY` (loopback, the
    services' aliases and the original `NO_PROXY` minus bound upstreams) and `NODE_USE_ENV_PROXY=1` (Node 22.21+ / 24;
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
  `Accept-Encoding: identity` (an encoded response anyway is decoded as
  it streams, re-framed chunked), streams the body both ways, strips
  token-shaped response headers and redacts the injected value wherever
  it comes back (an exact-match pass over headers and body, for the value
  and its basic-auth base64, a same-length mask so passed-through
  framing holds, carrying over between chunks only a tail that is a
  prefix of the value, so an SSE event isn't held for the next). An upstream address that resolves to loopback,
  link-local or private/ULA ranges is refused at connect, checked on the
  address dialed (so DNS rebinding gets nothing), unless the call is injected and its
  approved bound claim marks that upstream private (a self-hosted Sentry
  on the LAN; a phantom-less call to it gets a bare `401` from the
  broker, never dialed, so git still reaches its credential helper): no
  feature turns the broker into a way into its host's network. Request
  bodies are read only when a scope table needs them (GraphQL's parsed
  document, `git-receive-pack` commands, a model call's `model`, MCP's
  `tools/call`), and parsed strictly: a `Content-Encoding` one decoded
  within the body bound, duplicate keys and batches refused, one that
  doesn't decode or parse denied; a body past the bound (model calls run
  to 128 MB) is scanned as it streams upstream, a violation aborting the
  upstream request before its body ends. Requests with an `Origin` or `Sec-Fetch-Site` are refused,
  and so is a `Host` other than the listener (base URL) or other than the
  `CONNECT` target and SNI (intercept): a browser in the sandbox can't
  drive the broker. Its upstream calls use the host's proxy env (the
  proxy exempt from the address check; the target resolved and checked
  on the host, `CONNECT` to the checked address, SNI by name) and trust
  the host's native store (corporate TLS inspection).
- **Phantoms**: made at `run`, kept on the `Instance` in `state.toml`,
  dropped at `rm`. Not secret, so they go on the container env (`-e`) and
  the files a claim names (written by devsbd from its broker file, never onto a mount
  or volume target nor over a file that isn't a phantom: `run` warns and
  skips such a file, as a shared tool login mounted rw would be
  overwritten and leak-flagged by every other instance). The broker
  knows phantoms by its table, never by their shape, so a claim may
  declare a prefix for a client that picks its auth mode from the key's
  (zidane reads `sk-ant-oat…` as OAuth). Every injected
  request must carry the instance's phantom, in both modes (one
  exception: devsandbox's own `mcp` claim, where the bridge's identity is
  enough, agent-credentials), and the real
  value replaces it only in the bound claim's `inject` slot (a request
  carrying that header twice is refused): a phantom
  anywhere else (URL, other headers, a body) goes upstream as is, never
  swapped, so no posted text stores the token. Without one it goes
  upstream uninjected (git gets GitHub's 401 first, so the credential
  helper the claim's `git` sets hands over the phantom, git-gateway). A phantom arriving on another
  instance's bridge is refused and logged: leak detection for free.
- **Lifetimes**: the phantom lives until `rm`. A credential at its home
  doesn't expire for us. A token minted from it is renewed behind the
  phantom ahead of expiry, with a margin longer than a long request, so a
  tool never sees an expiry. A 401 anyway renews and replays the request
  only when its body is held (none, or under a bound); a streamed body
  past that can't be replayed, and the 401 reaches the tool with the
  reason in the body. Delegated tokens (*Delegation*) expire at most as
  the upstream says, the backstop for a member gone before `rm` reaches
  it, and are refreshed ahead of expiry the same way. An upstream 401 on
  a freshly renewed or static credential is no 401 to the tool, which
  would send it to the tool's own login: a `403` broker refusal naming
  `devsandbox login <credential>` and the home. The sandbox shapes every
  byte but the slot, so such a 401 marks nothing by itself: the
  credential is marked broken in `access ls` and its `approvers` notified
  (once, until a login) only on a refused refresh, an unreadable store,
  or a 401 on the claim's `check` route the broker calls itself. Broken
  refuses no call. A claim may intercept its provider's token and
  registration endpoints, or another provider's it only guards (no
  `inject` slot) (*Policy*'s third step; agent-credentials).
- **`credentials.toml`**, next to `state.toml`, read only on the home:
  per credential `source` (a file store beside `member.key`, 0600, the
  default on Linux, where a headless box or a user unit started before a
  desktop session has no unlocked keyring; the OS keyring on macOS; an
  external store such as `op://…` later), the approved claim versions
  (the claim minus its container-side wiring, keyed by a hash of that
  part, not the feature artifact's digest; an instance pins the one it
  ran with, so approving a new version never breaks a running instance;
  a version goes with its last instance or by `access revoke`; what's
  pinned and how a version gets approved: github-and-sentry-access,
  *The claim contract*), `profiles`, `members` (who may broker it, like
  `[[agent]]`), `approvers` (member keys that may answer its asks),
  `delegate`, `max` scopes, `as` (the token kinds it offers, for a
  credential that has several), and rules (remembered answers,
  hand-written ones). A `members` entry gives that member the whole grant unless it
  carries its own `max`. Nothing in a config can widen it. A claim and a
  config's `access.<credential>` name the credential's kind (`sentry`);
  an instance takes the home's entry (`sentry-work`) granted to its
  profile (github-and-sentry-access, *Profiles*). One feature
  per instance claims a given credential: a second is refused at `run`.
  `key_guard` matches files by name, so it lists this file, the file
  store, the CA key and `access.json` beside the member key's.
- **Policy** per call, first refusal answers: instance known → the
  target host is an upstream of one of the instance's claims → a
  token or registration endpoint a bound claim (or an `mcp` binding)
  marks is never forwarded, phantom or not: a phantom's refresh gets the
  same phantom and a far expiry, anything else a refusal naming
  `devsandbox login <credential>` →
  credential (by phantom; none, `mcp`'s bearer-less injection aside: the
  instance's host sends it upstream uninjected itself, the steps below
  skipped, so a home that is down breaks only calls that carry a
  credential); the rest evaluated by whoever injects (the home for a
  remote instance, unless *Delegation*) → that credential's bound claim is an approved
  version (else a refusal naming `devsandbox access approve
  <credential>` and the home; checked at `run` too, refused there when the home
  answers, a warning when a remote home is offline) → the target host is
  an upstream of that bound claim (a phantom sent to another claim's upstream,
  on its listener or the shared proxy port, is refused and logged, as a
  foreign one is) → granted to this profile and member →
  asked for in the
  config's `access.<credential>` (the claim's `access` for a remote
  instance; request ∩ grant, also checked at `run`;
  a remote home offline then is a warning, the per-call check stands) →
  the fence, for credentials that have one: the scope table attributes
  the route to a repo (case-folded, `.git` stripped, the query string
  ignored; strict in the repo part, `%2F` past it decoded once:
  github-and-sentry-access, *Matching*; `/repositories/{id}` and node
  ids resolved by the home) or lists it as repo-less (never asked: an
  `ask` scope denies there, as an answer couldn't name a repo); a route
  it can't place, unknown ones included, is
  denied, and a repo outside the fence gets no credential, never asked:
  the request goes upstream with the phantom stripped, from the
  instance's host, as to a stranger (public repos work, under the
  upstream's anonymous rate limit, 60 requests an hour per IP on
  GitHub's REST API); its response carries `devsandbox-fence: outside`,
  and an upstream `401` becomes the broker's refusal naming
  `access.<credential>.repos`, so no tool is sent to its own login → the
  bound scope table maps the route to a scope: allow, ask or deny, deny
  wins; for a credential without a fence, unknown routes are scope
  `other` (ask). Routes marked `never` (ones that mint a credential or
  plant one or a standing access path: tokens, deploy keys, hooks): no
  ask or rule opens them.
- **`presence`**: the claim marks routes and ref forms that need a
  human even when the scope allows them (a release, a tag push, a branch
  deletion), each with the field naming its target (a pushed ref, a
  release's `tag_name`, a ref mutation's ref; ids resolved to the ref by
  the home). Not a scope, so no config grants it: always asked, answered
  only once or for this instance on that one target, never by a rule,
  signed policy or a container (a delegate's own code, under
  *Delegation*); unattended, it waits out the hold and is refused. The routes are each tool's plan
  ([github-and-sentry-access](github-and-sentry-access.md),
  [git-gateway](git-gateway.md)).
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
  `devsandbox access ls|revoke` lists grants, remembered answers, pending
  claim versions and the live minted tokens; `access approve` and `access
  forget` are github-and-sentry-access's.
- **The member CA**: an ECDSA P-256 key and self-signed CA per member
  (ed25519 stays the member identity: browsers and other verifiers
  refuse Ed25519 certificates), beside `member.key`. Name-constrained to
  the hosts some claim intercepts; a new host reissues the cert over the
  same key and re-installs it in place over the helper (a stopped
  instance at its next `start`), no drift. Leaves
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
  Linux, no custom build. git gets `http.https://github.com/.sslCAInfo`
  pointing at the same file, over a dotfile's or nix's own bundle path
  (git-gateway). Java ignores `HTTPS_PROXY`, so it and
  compiled-in roots are base-URL only.
- **Permission requests**: an `ask` holds the request (the claim's
  `hold`, 120 s default, as tools' timeouts differ) and
  raises a request on the home: `access.requested` notifications,
  `access.pending` / `access.decide` methods, a dashboard modal, a desktop
  popup, `devsandbox access pending|approve|deny <request>` (the
  `machine pending|approve|deny` shape; `access approve <credential>`,
  naming no request, approves a pending claim version instead). Answers: once, this instance (until
  `rm`), always (a rule in `credentials.toml` matching credential, scope,
  the repo for repo-attributed scopes and `sandbox`, plus the member key
  for a remote instance; hand-written ones may match `profile`, `sandbox`
  or member), deny (remembered; its
  refusal says so and names `access revoke`, so an agent stops retrying);
  `presence` and budget asks ([agent-credentials](agent-credentials.md))
  take fewer. Asks dedupe per instance, credential, scope and, for
  repo-attributed scopes, repo (`presence`:
  method, route and target), a retry of a pending call joins it, and an
  answer after the hold becomes a one-shot grant for the instance's next
  matching call within minutes. In the container, `devsbd access wait
  <request>` (a `control` op exempt from the dispatcher check and held
  like `events-follow`, its own instance's requests only, ids
  unguessable, relayed to a remote home over `CRED`) blocks until the
  answer. Targets and routes, sandbox-shaped, are shown to approvers
  with control characters escaped. Stored
  in `access.json`; a remote instance's "this instance" answers age out
  after 30 days unused, as the home doesn't see its `rm`. Open asks are
  capped per instance (one `presence`, a few scope asks): a call past
  the cap is refused at once ("another request pending"), no popup, so
  volume can't wear an approver down. Not built on the Inbox, which is frozen and going away
  ([status](../../status.md#planned)).
- **Refusals** take the upstream's own error shape, which the claim
  declares (`refusal`: GitHub's `message`, Anthropic's `error`, git's
  `report-status`; for MCP's `tools/call` a `200` result with
  `isError`), with a one-line reason and a `devsandbox` object (verdict,
  request, credential, scope, what to do): `403` for pending, denied,
  not approved, not granted, an out-of-fence write and a broken home
  credential; `503` with `Retry-After` only for an unreachable home;
  never `WWW-Authenticate`, which starts a client's own login. A pending
  call on a model route gets the provider's retryable shape instead
  (Anthropic's `529`), and a stream cut mid-response ends with an error
  event of the provider's transient type (agent-credentials).
- **Signed policy**: rules the authority signs and members install like
  configs. A home applies them only if it opts in, and only within its own
  grants.
- **Fleet**: a `CRED` channel on links and routes, opened by the
  instance's host towards the home and answered while the home grants that
  member (read per stream, like `daemon::agent_for`). A credential's
  home is the authority's word, never the home's own: `login` on a
  member the log doesn't already name as home (there it just replaces
  the secret; an approver's `credentials.login` counts as the home's)
  proposes it (`fleet.propose`, as membership changes are,
  `serve::machines::changes`, a new `Change` kind) and the authority-signed membership log
  names one member key per credential, followed through rotations and
  ended by a revocation. `login` stores the secret first, then prints the
  `devsandbox machine approve <id>` to run on the authority (the
  authority's own login signs directly; an unreachable authority is
  reported and a re-run proposes again); meanwhile the host brokers its
  own instances only while the log names no home for that credential,
  and other members' calls get a refusal naming the pending approval and
  the authority. So a member can't make itself a home (it would
  read the calls and forge the answers) and there are never two. The
  home's signed link report (`graph::Report`) only says which members it
  grants, as for agents; a report naming a credential the log gives
  another member is ignored and logged, and a local copy that isn't the
  log's home goes unused, marked in `access ls`. Outside a fleet the home
  is the host. The instance's host ends TLS and,
  for a call carrying a phantom or on an `mcp` route, sends the parsed head with the signed claim (and the claim's canonical
  content the first time the home lacks that version, which it re-hashes
  and keeps pending for `access approve`), and the body once the home
  accepts the call or asks for it (a scope table reading it; the host
  keeps that copy, bounded); an out-of-fence verdict has the host send
  the request upstream stripped itself. The home runs policy, injects,
  calls upstream, redacts and returns the response; the host rewrites `Location` and `rewrite`
  fields to the instance's loopback. A static credential never leaves the
  home. Home unreachable: the
  instance's host holds the request (bounded) while it redials, then
  `503` with `Retry-After`, the reason in the body.
- **Delegation** (decision 3: credentials the home narrows by minting,
  GitHub App installation tokens first) to members its `delegate` lists:
  the instance's host sends `CRED` `mint` with the signed claim; the home
  runs every check up to the fence (grant, `delegate` and `members`
  read per mint, approved version, profile, `access`) and mints for that
  instance alone (keyed by member key and `incarnation`), narrowed to
  fence ∩ granted repos and the permissions of its *allowed* scopes, at
  most the upstream's lifetime. The delegate injects it, running the bound
  scope table and the fence itself; a call that would ask, needs
  `presence` or names a repo by id goes to the home as an undelegated
  call, so asks, rules and `access.json` stay there. Permissions are
  coarser than routes (GitHub's `contents: write` for a push also tags,
  deletes refs and publishes releases), so `delegate = [m]` trusts *m*'s
  daemon to route `presence` home, as fully as the home trusts itself;
  `access ls` marks a delegated credential whose claim has `presence`
  routes. Audit lines go to the home over `CRED`. The delegate deletes an
  instance's token upstream (`DELETE /installation/token`) at its `rm`,
  the home every outstanding one at `access revoke`; expiry covers a
  delegate gone dark.
- **Audit**: one JSON line per call in `access.log` on the home, rotated
  by size
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
   `access.log`, `run`'s bind, `runArgs` and host-class checks, broker
   ports refused by `forwards.*`; `ask` is deny and `presence` passes until step 4 (git-gateway
   step 2 the same). First user: sentry-cli
   ([github-and-sentry-access](github-and-sentry-access.md)).
2. `devsandbox login`'s device flow and browser OAuth, `access ls|revoke`.
3. The member CA, intercept mode, trust installed at create.
4. Permission requests: API, CLI, dashboard modal; remembered answers;
   `approvers` (a headless home's asks answered from a laptop).
5. `CRED` over links and routes; claims signed by the instance's host;
   homes in the membership log. Until then each host is its own
   credentials' home; once the log names one, the other hosts' copies go
   unused (marked in `access ls`) until revoked.
6. Signed policy.
7. Delegation (github-and-sentry-access step 4 its first user).

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
- Decision 4's "denied" is read in all four plans as "no credential":
  an out-of-fence request goes upstream anonymously (public repos work,
  writes and git's 401 get a refusal). Confirm, or refuse such requests
  outright?
- In-sandbox edits of a config: with no config root mounted, a sandbox
  that edits the live config needs a host-side confirm of the change, or
  ADR 0049 signing?
- Host networking: a sandbox on `--network=host` shares the host's
  loopback, so devsbd's listener would be the host's. Refuse brokering
  there, or a unix socket plus a tiny in-container `CONNECT` shim?
- HTTP/2 upstream (gRPC-ish APIs) and WebSockets through intercept mode.
- The file store at rest: plain 0600 like `member.key`, or sealed with
  `systemd-creds` / a TPM where there is one?
- One home per credential name fleet-wide: two members can't keep
  separate accounts for one tool (a personal Sentry on the laptop, a work
  one on the box). Let `login` pick the credential name the claim binds
  to (also github-and-sentry-access's GHES question)?
- Windows hosts: the broker lives in `serve`, unix-only like the relay.
