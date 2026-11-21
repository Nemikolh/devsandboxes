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
(*The claim contract*, *sentry-cli*). What isn't built yet:
[status](../../status.md#credentials).

Code: `src/broker/serve.rs` (`Service`, `Respond`, `Files`, `pump`),
`src/broker/policy.rs` (`decide`, `admit_intercept`, `intercept_claim`,
`Sources`, `Outcome`, `Action`, `Injection`, `Phantom`, `inject`,
`secrets`), `src/broker/intercept.rs` (`server_config`, `fold`, `ALPN`),
`src/credentials/ca.rs` (`MemberCa`), `src/broker/route.rs`
(`check_path`, `matches`, `scope_of`), `src/broker/rewrite.rs`
(`loopback_url`, `location`, `json_fields`, `MAX_REWRITE`),
`src/broker/refusal.rs` (`Refusal`), `src/broker/audit.rs` (`Audit`,
`Line`), `src/broker/head.rs`, `src/broker/body.rs`, `src/broker/guard.rs`,
`src/broker/redact.rs`, `src/broker/upstream/` (`Upstream`),
`src/serve/host.rs` (`poll`, where the broker is built),
`src/credentials/claim.rs` (`from_canonical`),
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

## Per connection

`Service::serve` reads requests until either side closes. Per request
(`Service::request`):

1. `head::read_head`: strict framing; a malformed head gets a refusal
   with `HttpError::status` (400, 431, 505, 417) and the connection
   closes.
2. `guard::refuse_browser` (any `Origin` or `Sec-Fetch-Site`) and
   `guard::check_host` (`Host` must be `127.0.0.1:<listener>`, or in
   intercept mode the `CONNECT` target, port 443 by default): `403`.
3. `state.toml` is read (`State::load_at`), then `policy::decide`.
4. A refusal is answered; an uninjected or injected call is forwarded
   (below); one `access.log` line either way.

Keep-alive holds both ways per request: the connection stays open when
the request asked for it and its body was read to its end. A refusal
leaves an unread body behind, so it closes the connection unless the
request had none. `Expect: 100-continue` is answered only once policy let
the call through; a refused one gets the refusal instead, and the body is
never read.

Nothing is cached: `state.toml` per request, and for a call carrying the
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
   the broker and is never dialed.
5. **Credential**: `home::resolve` with `access.<kind>.credential` and the
   instance's profile; none names `devsandbox login <kind>`.
6. **Approved version**: the claim's version must be one the credential
   approved (`Entry::approved_claim`), else a refusal naming
   `devsandbox access approve <credential>`. From here on the home's copy
   (`claim::from_canonical`) rules; the instance's only selected it.
7. **Upstream and slot** in that copy.
8. **Scope** (`route::scope_of` on the copy): method and path, query
   ignored; `{var}` takes one non-empty segment; segments compare raw
   (percent-encoding and case included), so an encoded form never matches
   a literal route and lands in `other`. A `never` route is refused
   always. A route in no scope is `other`, refused as an ask. The scope
   must be granted (`max`; unset: every scope) and not in
   `access.<kind>.ask` (refused: asks need permission requests), and
   requested (`access.<kind>.scopes`; empty: every scope of the claim),
   each refusal naming what to change.

The result (`Outcome`) carries the refusal or the injection (credential,
slot, the approved claim, its upstream's `private`) and the log context:
kind, credential, scope, whose phantom, the instance's listeners (none in
intercept mode) and the refusal shape (the approved copy's once read,
else the instance's).

## The call

`Service::forward` builds the upstream request: `https://<upstream><path>`
(path and query as the tool sent them), the tool's fields minus those the
upstream client writes itself (`upstream::RESERVED`: `Host`, framing,
`Accept-Encoding`, `Connection`, hop-by-hop, `Expect`) and those the
request's `Connection` names. An injected call gets the secret from the
file store (`Renewer::token`, read per call: the static token, or an
OAuth record's access token, renewed first when due ([Renewal](#renewal));
none stored names `devsandbox login`) and `policy::inject` writes it into
the chosen slot only: the header as its
`format` with the token, `Basic` with the slot's username, or the query
parameter percent-encoded. The address check lets private ranges through
only for an injected call whose approved upstream is `private`. The body
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
past the bound passes through unrewritten, noted in the log line. This is
what makes sentry-cli's chunk uploads (`GET …/chunk-upload/` answering an
absolute region URL) and its regions list come back through the broker.

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

## Refusals

`refusal::Refusal` renders the claim's `refusal` shape: `json` puts the
one-line reason at the claim's field (`detail` for sentry; a dotted field
nests) beside a `devsandbox` object (`verdict`, `credential`, `scope`,
`do`); `anthropic` is Anthropic's error object; `mcp` a JSON-RPC error;
`git` the reason as a text body. No shape: `json` with `message`. `403`
for policy refusals and leaks, `400` for malformed requests and strict
paths, `503` when a file can't be read, `502` for a failed upstream.
Never `WWW-Authenticate`.

## `access.log`

`audit::Audit`: one JSON line per request in `access.log` beside
`state.toml` (0600), moved to `access.log.1` once the next line would
take it past 10 MiB (`MAX_BYTES`). Fields: `time` (RFC 3339 UTC),
`instance`, `instance_id`, `incarnation`, `kind`, `credential`, `method`,
`route` (the path before injection, query dropped), `scope`, `verdict`
(`allow`, `uninjected`, `refused`, `leak`, `error`), `reason`, `status`,
`phantom` (`own`, `other-claim`, `foreign`), and `refreshed: true` when
the call renewed the token. An intercepted connection closed before any
request (not intercepted, no TLS, a failed handshake) leaves a line
without `method` or `route`, `status` 0. Never a token or a phantom.
Format reference: [credentials CLI](../../reference/credentials-cli.md#accesslog).

## End to end

`commands::run::tests::sentry_cli_brokered_end_to_end_with_docker` runs
the whole path with a real sentry-cli (a pinned release in the test
image): the sentry fixture's claim (`tests/fixtures/features/sentry/`,
the claim `features/sentry` ships) with its one upstream at a private
`localhost`, `sentry-work` logged in through `commands::login::login_at`,
`run` wiring the instance, and a `Service` with the test CA's roots
behind the instance's bridge (`devsbd::bridge::spawn_brokered`) calling
a mock Sentry over TLS (`Service::with_authority`). `releases new` and a
source-map upload (`GET …/chunk-upload/` answering an absolute URL, which
`rewrite` points back at the loopback, then the chunk post and the
assemble calls) succeed; the mock sees only the real token, as `Bearer`,
and `access.log` an `allow` line per call with neither token nor phantom.
