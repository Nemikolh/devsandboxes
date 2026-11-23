# The credential broker

What `devsandbox serve` does with a connection a sandbox opens to one of
its broker listeners, or a `CONNECT` to a host it intercepts: decide, swap
the phantom for the real credential, call the upstream, and send the
answer back with the credential masked.
Read this before touching the broker, its policy, or what a tool sees when
a call is refused. The design it implements is
[credential-broker](../../plans/active/credential-broker.md) (*Shape*:
"The broker", "Phantoms", "Policy", "Refusals", "Audit") and
[github-and-sentry-access](../../plans/active/github-and-sentry-access.md)
(*The claim contract*, *gh*'s *The repo fence*, *Matching*, *Outside the
fence*, *sentry*, *Sentry's org fence*) and [git-gateway](../../plans/active/git-gateway.md)
(*The gateway*, "The broker"). What isn't built yet:
[status](../../status.md#credentials).

Code: `src/broker/serve.rs` (`Service`, `Respond`, `Files`, `pump`),
`src/broker/policy.rs` (`decide`, `admit_intercept`, `intercept_claim`,
`Sources`, `Outcome`, `Action`, `Injection`, `Phantom`, `inject`,
`strip`, `fence_of`, `fence_for`, `graphql_scopes`, `check_org`,
`OrgList`, `secrets`), `src/broker/presence.rs` (`reads_body`, `http`,
`mutation`, `Named`), `src/broker/github.rs`
(`Fence`, `Live`, `Lookup::release_tag`, `Lookup::ref_name`), `src/broker/orgs.rs` (`place`, `allowed`, `Lookup`,
`Live`, `Cache`, `Https`), `src/broker/graphql.rs` (`decode`, `parse`,
`Operation`, `inputs`), `src/broker/intercept.rs`
(`server_config`, `fold`, `ALPN`), `src/credentials/ca.rs` (`MemberCa`),
`src/broker/route.rs` (`check_path`, `matches`, `Match`, `scope_of`,
`Matched`, `var_value`, `git_service`, `is_write`), `src/broker/rewrite.rs`
(`loopback_url`, `location`, `json_fields`, `org_list`, `MAX_REWRITE`),
`src/broker/refusal.rs` (`Refusal`), `src/broker/audit.rs` (`Audit`,
`Line`), `src/broker/head.rs`, `src/broker/body.rs`, `src/broker/guard.rs`,
`src/broker/redact.rs`, `src/broker/upstream/` (`Upstream`),
`src/serve/host.rs` (`poll`, where the broker is built),
`src/credentials/claim.rs` (`from_canonical`, `Pinned::fenced`),
`src/credentials/fence.rs` (`normalize`, `repo_arg`),
`src/credentials/oauth/refresh.rs` (`Renewer`, `refresh`, `due`,
`MARGIN`, `RECENT`), `src/credentials/home.rs` (`Broken`,
`mark_broken`).

## Where connections come from

Each claimed instance's helper listens on `127.0.0.1:<port>` per upstream
of its claims ([in-container](../helper/in-container.md#broker-listeners))
and carries each connection to the host as an `HTTP_PROXY` stream on its
bridge, with a `ProxyHeader` naming the upstream and the listener port.
`serve::host::poll` builds one `broker::serve::Service` at start
(`Service::from_host`: the user's `state.toml`, the host's native TLS
roots and proxy env, built once) and hands it to `Bridges::spawn_worker`,
so every daemon bridge advertises `caps::HTTP_PROXY` and passes each
stream to `Broker::serve` with its `Caller`, the instance key and
`instance_id` of the bridge that carried it
([bridges](../helper/bridges.md#broker-connections-http_proxy)). The
bridge is the identity: nothing the container sends changes it. A host
with no usable TLS roots still serves the broker; its calls answer `502`
naming why.

An intercepted host comes the same way from the helper's proxy port: a
`CONNECT host:443` to a host its broker file binds is answered `200` by
the helper, and the stream carries `ProxyHeader::Intercept` (the host,
the port) then the tool's raw TLS ([Intercept mode](#intercept-mode)).
The LFS listener's streams carry `ProxyHeader::Lfs` (its port) and are
LFS transfers, not calls: `Service::lfs_request` serves them
([git](git.md#lfs)).

## Per connection

`Service::serve` reads requests until either side closes. Per request
(`Service::request`):

1. `head::read_head`: strict framing; a malformed head gets a refusal
   with `HttpError::status` (400, 431, 505, 417) and the connection
   closes.
2. `guard::refuse_browser` (any `Origin` or `Sec-Fetch-Site`) and
   `guard::check_host` (`Host` must be `127.0.0.1:<listener>`, or in
   intercept mode the `CONNECT` target, port 443 by default): `403`.
3. `state.toml` is read (`State::load_at`); an LFS batch's body is read
   whole (`batch_body`, at most `route::MAX_LFS_BATCH`, after the `100
   Continue` it asked for; [git](git.md#lfs)), and so is a GraphQL
   request's to a claim with GraphQL routes (`graphql_body`,
   [GraphQL](#graphql)) and a presence route's that names its target in
   its body (`presence_body`, [Presence](#presence)), then
   `policy::decide`.
4. A refusal is answered; an uninjected, injected or outside call is
   forwarded (below); one `access.log` line either way.

Keep-alive holds both ways per request: the connection stays open when
the request asked for it and its body was read to its end. A refusal
leaves an unread body behind, so it closes the connection unless the
request had none. `Expect: 100-continue` is answered only once policy let
the call through; a refused one gets the refusal instead, and the body is
never read.

Nothing is cached but the ids the fences resolve ([fence](fence.md#ids),
[The org fence](#the-org-fence)): `state.toml` per request, and for a call carrying the
phantom `credentials.toml`, the file store and the instance's config
(`Config::load` of `Instance::config_dir`, `resolve_sandbox`), so an edit
to a grant, a secret or `access.<kind>` applies to the next call, no
rebuild.

## Policy

`policy::decide` is pure over the instance records, the head and a
`Sources` (the home and the instance's config `access`, read on demand,
only for a call carrying the phantom). First refusal answers:

1. **Instance known**: a `state.toml` record under the caller's key with
   the caller's `instance_id`, carrying claims.
2. **Listener or intercepted host**: base URL: `instance::by_port` finds
   the claim listening on the header's port, its upstream must be the one
   the header names, and the claim must be base-URL mode. Intercept:
   `intercept_claim` finds the instance's intercept-mode claim with the
   `CONNECT` host among its upstreams, port 443 only; listeners play no
   part. That claim is "the claim" below, so a phantom of another claim
   (a base-URL claim's included) sent to it is a `leak` at step 4.
3. **Strict path** (`route::check_path`): a dot-segment (`.`, `..`), an
   encoded dot (`%2e` in any case, anywhere) or an empty segment (`//`)
   anywhere in the path is a `400`, for every call, phantom or not, so no
   path an upstream would resolve differently is ever forwarded.
4. **Phantom**, looked for only in inject slots, by the phantom table
   (never by shape): a header slot's value in its `format` (the text
   before `{token}`, an auth scheme, compares case-insensitively), a basic
   slot's password (any username), a query slot's raw value. Any other
   claim's phantom (this instance's other claim: `other-claim`, another
   instance's: `foreign`) in any slot of this instance's claims is a
   `leak`. A slot sent twice, or both slots of a two-slot claim filled, is
   a `400`. No phantom of the claim in one of its slots: the call goes
   upstream **uninjected**, as it came; a phantom anywhere else (another
   header, the URL, the body) goes as is, never swapped. An uninjected
   call to an upstream the claim marks `private` gets a bare `401` from
   the broker and is never dialed; a git smart-HTTP call's carries
   `WWW-Authenticate: Basic`, since git's curl sends the credential
   helper's answer only to a challenge.
5. **Credential**: `home::resolve` with `access.<kind>.credential` and the
   instance's profile; none names `devsandbox login <kind>`.
6. **Approved version**: the claim's version must be one the credential
   approved (`Entry::approved_claim`), else a refusal naming
   `devsandbox access approve <credential>`. From here on the home's copy
   (`claim::from_canonical`) rules; the instance's only selected it.
7. **Upstream and slot** in that copy.
8. **Route** (`route::scope_of` on the copy, [Route matching](#route-matching)):
   a `never` route is refused always; a route in no scope is `other`:
   asked on a claim without a repo fence, refused on a fenced one (an
   answer couldn't name a repo); a path past `{repo}` that decodes badly
   is a `400`.
   `POST /graphql` to a claim with GraphQL routes is decided by its
   operation instead, its mutation inputs placed in the fence here too
   ([GraphQL](#graphql)), each scope it needs then taking step 11.
9. **Repo fence**, for a fenced claim ([below](#the-repo-fence)): the
   matched route is placed on a repo or a repository id, repo-less, or
   refused; a repo outside the fence goes upstream stripped, or is
   refused for a write; a renamed repo's old name is sent to its current
   one.
10. **Org fence**, for a claim with `orgs` ([below](#the-org-fence)): a
   route holding `orgs.var` must name one of the credential's orgs the
   config allows, by slug or id, else it's refused, reads included.
11. **Scope**: each scope the call needs must be granted (`max`; unset:
   every scope) and requested (`access.<kind>.scopes`; empty: every scope
   of the claim), each refusal naming what to change; `other` takes
   neither, as both name claim scopes. Every refusal comes before any
   ask, so an answer is never followed by one. A scope in
   `access.<kind>.ask` on a repo-less route of a fenced claim is refused
   as never asked. Then, for the asked scopes (in `access.<kind>.ask`,
   or `other`), each by its `asks::AskKey` (instance id, credential,
   scope, and the repo a fenced claim's call names), then for each
   [presence](#presence) target of the call (its key also naming the
   route and target), in this order: a deny kept for it
   (`Sources::remembered`, `access.json`) refuses, checked for every key
   before any is asked; a scope one a rule on the credential allows
   (`Entry::rule_for`; never a presence one), or a *this instance*
   answer kept for it, or `Sources::granted`, goes through; the first
   other is `Action::Ask`: the call is held for a
   [permission request](#permission-requests) and decided again.

The result (`Outcome`) carries the refusal, the injection (credential,
slot, the approved claim, its upstream's `private`), the outside call
(`Action::Outside`: the phantom's slot, the upstream's `private`) or the
ask (`asks::Ask`: its key, the instance and its sandbox, method and route, the claim's
`hold`, else `asks::DEFAULT_HOLD`, 120 s) and the
log context: kind, credential, scope, whose phantom, the instance's
listeners (none in intercept mode), the outside repo and the refusal
shape (the approved copy's once read, else the instance's; git's for any
git smart-HTTP request, `route::git_service`, so git prints the reason).

### Route matching

A route is a method and a path, optionally one `?name=value` query
constraint (`GET /{repo}/info/refs?service=git-receive-pack`): the
request's query must hold that parameter exactly once with that raw
value; other parameters are ignored, and a route without one ignores the
query. `{var}` takes one non-empty segment and `{repo}` two, `owner/repo`;
literal segments compare raw (percent-encoding and case included), so an
encoded form never matches a literal route and lands in `other`.

`{repo}`'s segments are strict, as `fence::normalize` reads them: owner
`[A-Za-z0-9-]`, repo `[A-Za-z0-9._-]`, so an encoded character there
matches nothing; the match carries them lowercased with `.wiki.git` or
`.git` dropped. Past them `%2F` (any case) is decoded once and the
decoded segments matched against the rest of the route (gh sends
`git/ref/tags%2Fv1`); a double encoding (`%25` and two hex digits) or a
dot-segment or `//` after decoding refuses the call (`Match::Refused`).
The request is forwarded exactly as sent, but for a renamed repo's old
name ([The repo fence](#the-repo-fence)).

`never` routes are tried first, then routes with a query constraint, then
the rest, so `info/refs?service=git-receive-pack` is `git.push`'s even if
another scope lists a bare `info/refs`.

### The repo fence

A claim whose `fence` names any route is fenced (`Pinned::fenced`).
`route::scope_of` returns the matched route and what its `{repo}` took;
`attribute` places it:

- the route's `fence` entry names an input other than `{repo}` and a
  `/repositories/{id}` route's `{id}` (a body field): it can't be placed
  yet and is refused;
- a `/repositories/{id}` route whose `fence` names `{id}`: the id (ASCII
  digits, else a `400`) is checked against the fence's pins;
- it took a repo: checked against the instance's fence (`fence_for`:
  `fence_of`'s `Instance::github_repos`, the folders' recorded repos
  ([fence](fence.md)), and the config's `access.<kind>.repos`, each
  `owner/repo` or a GitHub URL of it through `fence::repo_arg`, read per
  call), resolved to ids at the home ([fence](fence.md#ids)): an entry's
  old name or its current one is in, and a call naming the old one is
  sent to the current one (`Injection::path`);
- no `{repo}`: repo-less, on to the scope checks (never asked).

A placed repo (the name the call used, or the entry an id is pinned to)
is the repo of the call's ask key.

In the fence the scope checks follow. Outside it the call gets no
credential and is never asked: a write (`route::is_write`: a method but
`GET`/`HEAD`, except git's upload-pack `POST`, and receive-pack's
`info/refs` too; an LFS batch by its body's `operation`, `upload` the
write) is refused naming `access.<kind>.repos`, at `info/refs`
for a push, before git sends anything; a read is `Action::Outside`, sent
upstream with the phantom's slot removed (`policy::strip`: the header, or
the query parameter), its response marked `devsandbox-fence: outside`
(an upstream's own such field dropped). An upstream `401` to it becomes
the broker's `403` naming `access.<kind>.repos`, so git never falls back
to a credential prompt and gh never to its login.

### The org fence

A claim's `orgs` (`claim::Orgs`) fences one path variable, `var`
(Sentry's `{org}`), to the credential's `orgs` in `credentials.toml`
(`Entry::orgs`), narrowed by the config's `access.<kind>.orgs` when it
names any (`orgs::allowed`, case-insensitive). It's independent of the
repo fence and checked after it, on the matched HTTP route only.

A route holding `var` (`route::var_value`: the segment it took, raw, so
an encoded slug matches nothing) is placed by `policy::check_org` over
`orgs::place`:

- a credential naming no orgs refuses it, naming `orgs` of the
  credential in `credentials.toml`;
- an allowed slug in any case is in;
- all ASCII digits is an org id (Sentry's slugs never are; the `sentry`
  CLI names an org by the id in a DSN): in when it's the id of an
  allowed slug. Ids are learned from `orgs.resolve` with `var` replaced
  by each of the credential's slugs (allowed ones first; a slug that
  isn't letters, digits, `_` and `-` is never sent), called with the
  credential in the claim's slot on the call's own upstream (a region
  answers for its orgs), its answer's top-level `orgs.id` field (a
  string or a number). The id the sandbox named is never looked up
  itself. `orgs::Live` caches each answer per (credential, upstream,
  slug) for `github::TTL`, a failure or a non-`200` for
  `github::FAILED_TTL`, which places nothing; `orgs::Https` is the
  transport over the broker's upstream client;
- else it's refused, reads too (Sentry has no anonymous reads to send
  it to instead): an org of the credential's the config leaves out
  names `access.<kind>.orgs`, any other names `orgs` of the credential.

A route that doesn't hold `var` is unfenced. One listed in `orgs.lists`
hands `serve` the allowed slugs and the fields (`Injection::orgs`) for
its answer ([The call](#the-call)).

### GraphQL

A claim with any `graphql query` or `graphql mutation <name>` route
(`Pinned::graphql`; it may then list no HTTP route on `/graphql`) has
every `POST /graphql` body read before policy (`serve::graphql_body`):
at most `graphql::MAX_BODY` (1 MiB) as sent, else a `413` and the
connection closes; a `Content-Encoding` of `gzip` or `deflate` (zlib) is
decoded within the same bound (`graphql::decode`: past it a `413`, one
that doesn't decode, or any other encoding, a `400`). `graphql::parse`
then reads it strictly, each failure a `400`:

- the body is one JSON object (an array is a batch), no object anywhere
  naming a key twice, its fields only `query` (a string), `variables`
  (an object or `null`) and `operationName` (a string or `null`, naming
  the operation);
- the document is exactly one operation, never a subscription, with
  fragments each defined once, every spread defined, none reaching
  itself; no argument, input field or variable given twice; no nesting
  past 64 levels; strings without `\u{…}` escapes or a lone surrogate.

The bytes as sent go upstream, `Content-Encoding` and all (and are what a
401 replays). `policy::graphql_scopes` decides the operation:

- a **query** takes the `graphql query` route's scope, repo-less: it
  passes under the token's own reach and is never asked, so an `ask`
  scope refuses it;
- a **mutation** takes each top-level field's `graphql mutation <name>`
  scope, fragments (spread or inline, whatever their type condition)
  expanded into the top level and aliases ignored; a field in no scope is
  `other`, in `never` refused. On a fenced claim each `fence` input of
  the field (dotted from its arguments, `[]` for each element of a list,
  a single value taken as a list of one: `input.repositoryId`,
  `input.branch.repositoryNameWithOwner`) is read from the arguments,
  variables replaced by their JSON value or the definition's default
  (`graphql::inputs`): an input named `…NameWithOwner` must be an
  `owner/name` in the fence (an entry's old name is refused: the body
  can't be redirected), any other a global id `Fence::place_node`
  places (a pinned repository's node id with no call, else `node(id:)`
  resolves it), or an integer repository id. One outside the fence
  refuses the call naming `access.<kind>.repos` (a mutation is a write,
  never stripped); one that doesn't resolve is `other`, refused. Each
  repo the inputs place is asked for separately (one `Need` per scope
  and repo). A field whose inputs are all absent is repo-less (never
  asked); a block
  string, an enum or an object where an input is read is a `400`.

Every scope the fields need must pass step 11; the log line's `scope`
lists them, comma-separated.

### Presence

A claim's `presence` route entries (`claim::Presence::Route`) mark calls
a person approves each time, on each target, even when their scope is
allowed or granted: no config, rule or *always* answer opens them. After the scope
checks (step 11), each `presence` route the call is (`route::is`, so a
route matches whichever scope route matched first) or each mutation
field it names (`presence::mutation`) yields its targets:

- a `{var}` target is the segment the path gave it (`route::var_value`,
  past `{repo}` with `%2F` decoded); on a `git/refs` route it is the full
  ref instead, `refs/` and the route's segments past `git/refs/` with each
  `{var}` filled (`presence::path_ref`: `heads/{prefix}/{branch}` reads
  `refs/heads/a/x`);
- `{release_id}` is the release at the call's own path: the home `GET`s
  that path with the credential (`Lookup::release_tag`, a renamed repo's
  current name used) and takes its `tag_name`;
- any other target of an HTTP route is a body field (`ref`, `tag_name`),
  read as `fence` inputs are (`graphql::json_inputs`): the body of such a
  route, and of a release route, is read whole before policy
  (`presence::reads_body`, `serve::presence_body`): at most
  `presence::MAX_BODY` (1 MiB) else a `413` and the connection closes,
  any `Content-Encoding` a `400`, strict JSON (no key twice) else a
  `400`. The bytes as read go upstream;
- a mutation's target is its input (`input.name`, `graphql::inputs`); one
  whose last step ends in `Id` (`input.refId`) is a `Ref`'s node id, the
  home's `node(id:)` names it (`Lookup::ref_name`: `prefix` and `name`,
  `refs/tags/v1`).

A ref target (a `git/refs` route's, a `createRef`/`updateRef`/`deleteRef`
input, a resolved `refId` included) has an operation: create for `POST`
and `createRef`, update for `PATCH` and `updateRef`, delete for `DELETE`
and `deleteRef`. It is asked only when one of the claim's `ref` entries
(`claim::Presence::Ref`) holds it for that operation
(`presence::ref_asked`: `claim::ref_pattern_matches`, a trailing `*`
taking one or more segments; an empty `on`, every operation); else the
call is no presence call and goes on as any other of its scope. gh's
claim names `refs/tags/*` and, on delete, `refs/heads/*`: tags and ref
deletions are asked whichever route moves them, a branch's creation or
update isn't. A ref target named as sent that isn't a full ref
(`refs/…`) is refused, as it could match no entry. Release targets
aren't ref operations.

A target that's absent, empty, not a string or doesn't resolve refuses
the call, `403`, never passes it unasked: "`<METHOD path>` is a presence
call (always asked), and the broker can't name its target (`<target>`:
<why>)". Lookups go out with the credential on `api.github.com` (the
approved claim must list it) and are never cached. A release is presence
only when it publishes, so gh's draft-then-publish is one ask: `POST
…/releases` unless its body's `draft` is `true`, `PATCH
…/releases/{release_id}` only when it's `false` (`presence::draft_exempt`). Each
target is one `AskKey` with `presence` (`asks::PresenceAt`: the call's
`<METHOD> <path>`, or `graphql mutation <name>`, and the target) and the
call's scope and repo, asked after the call's scope asks; a mutation
naming several repos asks per repo.

## The call

`Service::forward` builds the upstream request: `https://<upstream><path>`
(path and query as the tool sent them, a renamed repo's segments
aside), the tool's fields minus those the
upstream client writes itself (`upstream::RESERVED`: `Host`, framing,
`Accept-Encoding`, `Connection`, hop-by-hop, `Expect`) and those the
request's `Connection` names. An injected call gets the secret from the
file store (`Renewer::token`, read per call: the static token, or an
OAuth record's access token, renewed first when due ([Renewal](#renewal));
none stored names `devsandbox login`) and `policy::inject` writes it into
the chosen slot only: the header as its
`format` with the token, `Basic` with the slot's username, or the query
parameter percent-encoded. An outside call has that slot removed instead.
The address check lets private ranges through only for an injected or
outside call whose approved upstream is `private`. `Git-Protocol` (a v2
fetch) passes through like any field the client doesn't reserve. The body
streams from the tool to the upstream as it arrives (`BodyReader` into
`upstream::Body`, its length or chunked as sent), except a renewable
call's (below), which is read whole first when it has a length up to
`MAX_REPLAY` (1 MiB), so a 401 can replay it. One upstream connection per
request.

The answer (`Respond::write`): status and fields as received, minus
hop-by-hop ones and those the response's `Connection` names (and
`Content-Encoding` when the client decoded the body). The injected value
is masked (`policy::secrets`: the token, its query form, and its basic
forms for the claim's basic slots) in the reason, every field value and
the body as it streams (`redact::Redactor`, same length, so a passed
`Content-Length` holds). In base-URL mode a `Location` to
`https://<upstream>[:443]` whose host has a listener on the instance is
rewritten to `http://127.0.0.1:<port>` with path and query kept
(`rewrite::location`); any other passes as is, as every one does in
intercept mode, where the tool already talks to the real host. The body goes back
`Content-Length` when the upstream framed it so, else chunked (to an
HTTP/1.0 client: until close), flushed per read so a stream goes out as
it comes.

`rewrite` (base-URL mode only, as claims allow it): for an injected call
to a route the approved claim lists, the
body is read whole up to `MAX_REWRITE` (16 MiB), masked, and each named
field (`url`, `regions[].url`: dotted names, `[]` for each element)
holding an absolute URL to a bound upstream is replaced by that
upstream's loopback URL (`rewrite::json_fields`), sent with its new
length. A body that isn't JSON or needs no change goes as it came; one
past the bound passes through unrewritten, noted in the log line. No
shipped claim rewrites (the sentry feature's claim intercepts); the
sentry-cli test fixture's base-URL claim
(`tests/fixtures/features/sentry-cli/`) brings its chunk uploads
(`GET …/chunk-upload/` answering an absolute region URL) and regions list
back through the broker this way.
An injected LFS batch's `200` is read the same way, but its actions are
put behind handles before masking (`lfs::rewrite_batch`, [git](git.md#lfs)):
the header they carry is what goes upstream later.

An injected call to an `orgs.lists` route ([The org fence](#the-org-fence))
with a `2xx` answer, in either mode, has its body read the same way,
masked, then cut to the fence (`rewrite::org_list`): a JSON array keeps
the elements one of whose listed top-level fields holds an allowed slug
(any case), or the id of one, resolved as the fence does only when an
element's fields hold all digits and no slug. A body that isn't a JSON
array, or is past `MAX_REWRITE`, is answered with the broker's `502`
(verdict `refused`) instead, since passed whole it could list every org
the account is in. Any other
status passes unfiltered.

An upstream `401` to an injected call never reaches the tool, which would
start its own login. A renewable token not renewed for this call is
renewed (`Renewer::after_401`) and the call replayed with the new one
when its body was held; a body that streamed (chunked, or past
`MAX_REPLAY`) can't be, so the tool gets the broker's `403` saying to send
it again (the token renewed meanwhile). A `401` on a static token, one
just renewed, or the replay is the broker's `403` naming `devsandbox login
<credential>` (`Service::unauthorized`). Before answering it, the broker
calls the claim's `check` route on the same upstream with the token
(`Target::check_401`: a `GET` or `HEAD` with no sandbox input, the slot
filled, no `access.log` line); only a `401` there marks the credential
broken, as the tool shaped every byte of its own call but the slot. A
`401` to an uninjected call passes through. A failed call is answered
with the status `UpstreamError::status` gives (`403` for a refused
address, `502` otherwise, `400` for a malformed request body),
refusal-shaped.

## Intercept mode

`Service::serve` folds the header's host (`intercept::fold`: lowercase,
one trailing dot dropped), then `Service::open_intercept`:

1. **Admitted before any TLS**: `state.toml` is read and
   `policy::admit_intercept` (steps 1 and 2) must find the claim. The
   helper has already answered `200`, so a refusal can't be an HTTP
   answer: the stream is closed, with an `access.log` line (`refused`,
   `status` 0).
2. **The leaf**: `Service::leaf` loads the member CA
   ([ca](ca.md)) at the first intercepted connection
   (`MemberCa::load_or_create`, kept in the `Service` behind an
   `RwLock`, a failed load retried next time) and asks it for the host's
   leaf. A host the CA doesn't cover yet is added first
   (`MemberCa::ensure_hosts` under the write lock), logged to the daemon
   log: containers trust the reissued cert only once it is installed in
   them again.
3. **The handshake**: a rustls server (`intercept::server_config`) whose
   only certificate is that leaf, answered only for a ClientHello whose
   SNI is the host (`OnlyHost`): no SNI or another name gets no
   certificate and the handshake fails. ALPN offered is `http/1.1` only:
   a client offering `h2` and `http/1.1` gets HTTP/1.1, one offering only
   `h2` fails, one offering none is served HTTP/1.1. A handshake that
   fails on TLS grounds leaves a `refused` line naming why; a client that
   just goes away leaves none.

Then the same request loop as base-URL mode runs over the TLS stream
(`Wire::Tls`): `Host` checked against the host, policy with the claim
matched by host, injection, redaction, and the upstream call to
`https://<host><path>` over the host's own TLS, as in base-URL mode.
`Location` passes as is and `rewrite` isn't applied. The connection
closes with a TLS `close_notify`.

## Renewal

An OAuth record with a refresh token is renewable: `Renewer` (one per
`Service`, shared by every connection thread) renews it behind the
phantom with RFC 6749 §6's `refresh_token` grant (`refresh`: `grant_type`,
`refresh_token` and `client_id` posted to the record's `token_url` over
`oauth::Http`, the public-address check on), so the tool never sees the
token change:

- **Ahead of expiry** (`Renewer::token`): within `MARGIN` (10 minutes,
  longer than the upstream client's 5-minute idle bound, so no call
  outlives its token) of `expires_at`. A record not due is read without a
  lock.
- **After a 401** (`Renewer::after_401`), unless this process renewed it
  within `RECENT` (60 s): then the token is new and the 401 is about
  something else. If the stored token is no longer the one that got the
  401 (another call or a login replaced it), that one is replayed with no
  refresh.

One refresh at a time per credential: a per-name mutex in the process,
then the store's file lock (`Store::renew`) held across the call, so a
concurrent `login` or a second daemon in a handoff waits instead of
spending a refresh token GitHub rotates. Under both, the record is re-read
and renewed only if still wanted. The new record (`refresh::renewed`)
replaces the old in one atomic write: the new access token and expiry,
and the new refresh token with its expiry, or the old ones when the
provider sent none.

A renewal that fails answers the call:

| failure | answer | broken |
|---|---|---|
| the provider refused (`invalid_grant`, any error but the transient ones) | `403` naming `devsandbox login <credential>` | yes |
| unreachable, `temporarily_unavailable`, `server_error`, `slow_down`, a reply that isn't a token | `503` with `Retry-After: 30` | no |
| the store can't be read or written | `503` | yes |

`credentials.toml`'s entry carries the mark (`Entry::broken`: `at`, RFC
3339 UTC, and a one-line `reason`), set by `home::mark_broken` only if
unset, so the first reason stays; a successful renewal
(`home::clear_broken`) or a `login` clears it. It refuses no call: it
is for `access ls`.

## Expiry

Every injected call that gets an answer goes through
`Service::track_expiry`. With a pasted secret (`Fresh::pasted`), the
response header the approved claim's `tokens.expiry` names (GitHub's
`github-authentication-token-expiration`) is read
(`tokens::captured`) and, when it differs from the entry's `expires_at`
(`Injection::expires_at`), recorded there (`home::set_expires_at`; a
failed write is logged, the call answered either way). The credential's
end is that, or an OAuth record's refresh token's expiry (`Fresh::ends`,
`tokens::ends_at`). Within 7 days of it the daemon shows a desktop
notice (`devsbd::desktop::notify_desktop`) naming `devsandbox login
<credential>`, at most once per credential per UTC day
(`tokens::Notices`, in memory): the broker sees every call a credential
serves, whichever instance makes it. `access ls` shows the same end and
warns 14 days ahead ([credentials CLI](../../reference/credentials-cli.md#access-ls)).

## Permission requests

An `Action::Ask` holds the call on its connection's thread (each
bridged connection has its own, so only that connection waits) while a
request waits in `asks::Registry`, the daemon's one (`Shared::new`
makes it over `state.toml`'s dir, the broker's data dir; `daemon::run`
hands it through `Host::start` to `Service::from_host`):

- **Ids**: `R-` and 12 Crockford base32 characters, 60 bits from the OS
  random source (`asks::fresh_id`): unguessable to another instance,
  and never a credential name, which has no uppercase letter.
- **Join**: a call whose key matches an unanswered request joins it, a
  retry included, and extends its hold to the joiner's; another repo or
  scope is another request.
- **Cap**: an instance with `MAX_OPEN_SCOPE_ASKS` (3) scope requests
  still in their hold, or `MAX_OPEN_PRESENCE_ASKS` (1) presence ones
  (counted apart), gets a new one of that kind refused at once, `403`
  `pending`, "another request pending for this instance: <id>" naming
  the oldest, no request raised.
- **Answers** (`Registry::answer`, `asks::Answer`): `once`, `instance`
  (this instance, until its `rm`), `always` (a rule), `deny`. A presence
  request refuses `always` (`asks::NotAlways`, left unanswered); its
  `instance` and `deny` cover its route and target only. The
  answer is kept first (`asks::remember`, under the registry's lock):
  `instance` and `deny` as an entry in `access.json`
  (`credentials::answers`, one per key: a later answer replaces it),
  `always` as a rule appended to the credential's `rules` in
  `credentials.toml` (`home::Rule`: the key's scope and repo, the
  instance's sandbox, the request id); a failed write leaves the request
  unanswered.
- **Hold**: `Registry::wait` blocks for the ask's `hold`. Every answer
  but `deny` wakes the waiters through: each decides again (`state.toml`
  reread, the body read before policy reused) with its key answered for
  that call (`asks::Grants`, the `Sources::granted` behind
  `serve::Files`), so a call needing several asks goes through them in
  turn. `deny` refuses `403` `denied`, "denied for this instance
  (request <id>)", naming `devsandbox access revoke <id>`; a kept deny
  refuses the instance's later matching calls the same way at once, so
  an agent stops retrying. A hold ending unanswered refuses
  `403` `pending`, "scope `<scope>`[ on `<repo>`] of credential
  `<credential>` needs permission: request <id> is pending", naming
  `devsandbox access approve <id>`; a presence one, "`<METHOD path>` on
  `<target>` (scope `<scope>`[ on `<repo>`] of credential
  `<credential>`) needs a person's permission each time: request <id> is
  pending".
- **Late answers**: a request no call waits on stays listed
  (`Registry::pending`) for `LATE_GRANT` (5 min) past its hold, then is
  dropped. A `once` then becomes a one-shot grant: the instance's next
  call with that key within `LATE_GRANT` takes it (`Registry::granted`)
  and goes through unheld; the other answers are kept as above.
- **Waiting in the container**: `devsbd access wait <request>
  [--timeout SECS]` (the `access-wait` control op,
  [protocol](../helper/protocol.md#control)) is how an agent refused
  `403` `pending` waits for the answer before it retries. The bridge
  hands it to `asks::serve_wait` with its own instance's id, never
  through `dispatch`: no `dispatcher` or `inbox` allowance is needed,
  and only that instance's requests are found (`Registry::wait_answer`,
  keyed by id and instance id), so another instance's id reads as
  `unknown`, the same as none. The wait is no waiter of the request: a
  `once` answered while no call is held still leaves its one-shot grant
  for the retry. Every answer is kept for `LATE_GRANT` after it is given
  (`Registry::answer`), so a wait started after the answer still reads
  it. The wait lasts up to `MAX_WAIT` (300 s, the default), pinging
  every 30 s, and prints `once`, `instance`, `always` (exit 0), `deny`
  (3), `pending` (4: the timeout passed) or `unknown` (5: no such
  request for this instance, or it expired).
- **Changes**: `Registry::on_change` sets the hook every open, join,
  answer and waiter leaving calls, outside the registry's lock, and a
  request dropped past its late window: by whichever call locks the
  registry next, else by `Registry::prune`, which the host poll
  (`serve::host`) calls every pass. The daemon's hook
  (`daemon::publish_access`) flags `access.changed` for `access`
  subscribers.
- **Popup**: the call that opens a request (`Ticket::opened`, never a
  join) raises a desktop notification (`Service::raise`, through
  `desktop::notify_desktop`), titled `devsandbox: permission request`:
  "<instance> asks <credential> <scope>[ on <repo>][, for <target>]:
  devsandbox access approve <id>", escaped by `open::printable`. Each instance's popups go
  through a `desktop::RateLimit`, as the cap bounds a burst of asks but
  not ones answered one after another.
- **Who answers**: the API, local clients only: `access.pending` lists
  the requests (`api::AccessRequest`: what the sandbox's call chose,
  method, route, repo and a presence request's target, with control
  characters escaped by
  `open::printable`), `access.decide` answers one
  ([API](../../reference/api.md#access)); the CLI's `devsandbox access
  pending`, `access approve <request> [--for …]` and `access deny
  <request>` are clients of them
  ([credentials CLI](../../reference/credentials-cli.md#permission-requests)),
  and so is the dashboard's modal
  ([dashboard](../dashboard/tabs-and-actions.md#permission-requests)).
  `lent::admit` refuses
  `access.*`, `credentials.*` and a `subscribe` to `access` to every
  peer, under either trust.

Requests live in memory: a daemon exiting drops them with their held
calls. Kept answers live in the files: `serve::Files::remembered` reads
`access.json` per call, as the rules are read with `credentials.toml`.
They end with `rm` (`answers::forget_instance`: every answer kept for
the instance) or `devsandbox access revoke <id>` (the answer, or the
rule, the request left;
[credentials CLI](../../reference/credentials-cli.md#access-revoke)).
A hand-written rule (no `id`) may also name a `profile`; a rule's
fields, all optional but `scope`, each must equal the call's (`repo`
case-insensitively).

## Refusals

`refusal::Refusal` renders the claim's `refusal` shape: `json` puts the
one-line reason at the claim's field (`detail` for sentry; a dotted field
nests) beside a `devsandbox` object (`verdict`, `credential`, `scope`,
`request`: the permission request of a `pending` or `denied` verdict,
`do`); `anthropic` is Anthropic's error object; `mcp` a JSON-RPC error;
`git` the reason as a text body. No shape: `json` with `message`. `403`
for policy refusals, held calls' refusals and leaks, `400` for
malformed requests and strict paths, `503` when a file can't be read, `502` for a failed upstream.
Never `WWW-Authenticate`.

## `access.log`

`audit::Audit`: one JSON line per request in `access.log` beside
`state.toml` (0600), moved to `access.log.1` once the next line would
take it past 10 MiB (`MAX_BYTES`). Fields: `time` (RFC 3339 UTC),
`instance`, `instance_id`, `incarnation`, `kind`, `credential`, `method`,
`route` (the path before injection, query dropped), `scope`, `verdict`
(`allow`, `uninjected`, `outside`, `refused`, `pending`, `denied`,
`leak`, `error`), `reason`, `status`, `phantom` (`own`, `other-claim`,
`foreign`), `refreshed: true` when the call renewed the token, and for a
held call `request` (its id) and `decided` (`once`, `instance`,
`always`, `deny`, `timeout`). An intercepted connection closed before any
request (not intercepted, no TLS, a failed handshake) leaves a line
without `method` or `route`, `status` 0. Never a token or a phantom.
Format reference: [credentials CLI](../../reference/credentials-cli.md#accesslog).

## End to end

`commands::run::tests::sentry_through_the_broker_end_to_end_with_docker`
runs the whole path with a real `sentry` (a pinned release in the test
image): the sentry fixture's claim (`tests/fixtures/features/sentry/`,
the claim `features/sentry` ships), `sentry-work` logged in through
`commands::login::login_at` with `--org o` (checked against the mock),
`run` wiring the instance, and a `Service`
with the test CA's roots behind the instance's bridge
(`devsbd::bridge::spawn_brokered`) calling two mocks over TLS:
`sentry.io` (the claim's one private upstream, `Service::with_authority`)
and `us.sentry.io` (`Service::with_private_host`). `sentry issue list`
resolves the org on sentry.io and lists its issues on the region host the
org names; `sentry project delete` is refused at its `never` DELETE,
which reaches neither mock. The org fence: `o` named by its id passes
(the home learning the id from the mock), the account's other org is
refused before either mock sees it, and `sentry org list` shows only `o`
of the two the mock lists. The mocks see only the real token, as
`Bearer`, and `access.log` neither token nor phantom.
`commands::run::tests::gh_through_the_broker_end_to_end_with_docker` does
the same for gh and its repo fence; base-URL mode's listeners, env and
phantom file are `claims_wire_env_files_and_listeners_with_docker`'s,
over the sentry-cli fixture.
