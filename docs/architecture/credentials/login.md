# OAuth logins

How `devsandbox login` gets a token from a provider when the claim's
`login` lists `device` or `browser`, and what it keeps. Read this before
touching either flow or the token record. The command around them (claim
pick, approval, method choice, `--client-id`) and the store's format:
[credentials CLI](../../reference/credentials-cli.md). The design is
[credential-broker](../../plans/active/credential-broker.md) (*Shape*:
"`devsandbox login <credential>`", "Lifetimes") and
[github-and-sentry-access](../../plans/active/github-and-sentry-access.md)
(*Whose token*: app user tokens by an existing app's device flow).

Code: `src/credentials/oauth/mod.rs` (`Http`, `parse_token_reply`,
`TokenSet`, `OAuthError`, `Ui`, `Clock`, `openable`, `form_encode`,
`pkce_challenge`, `random`), `src/credentials/oauth/device.rs` (`login`,
`parse_authorization`, `Poller`, `poll`), `src/credentials/oauth/browser.rs`
(`login`, `authorize_url`, `callback`, `TIMEOUT`),
`src/credentials/store.rs` (`Record`, `OAuthToken`, `Store::renew`),
`src/commands/login.rs` (`obtain`, `create_url`, `Flows`).

## The endpoints

Every call to a provider goes through `Http`, the broker's upstream client
(`broker::upstream::Upstream::from_host`: the host's roots and proxy env)
with the private override off, so each address dialed passes the public
address check: a claim's `deviceUrl` or `tokenUrl` can't reach the home's
LAN or loopback. Requests are form posts with `Accept: application/json`
(GitHub answers a form otherwise; both are parsed) and a `User-Agent`;
replies past 64 KiB are refused.

`parse_token_reply` reads a token endpoint's answer: an `error` field is
an `OAuthError` whatever the status (GitHub answers errors with a 200), a
token only a 2xx with `access_token`, and a `token_type` other than bearer
is refused. `expires_in`, `refresh_token`, `refresh_token_expires_in`
(GitHub's app user tokens: 8 hours, refresh token 6 months) and `scope`
are kept when present. Errors name the status and the provider's error
code and description (control characters escaped), never the body, which
may hold a token.

## Device flow

RFC 8628. `device::login` posts `client_id` to `deviceUrl`
(`parse_authorization`: `device_code`, `user_code`, `verification_uri`
or Google's `verification_url`, `expires_in`, `interval`), shows the page
and code, and opens the page (`verification_uri_complete` when sent; an
`https` URL without control characters only, `openable`) where a browser
opens. Then
`poll` asks `tokenUrl` with the device code every interval (5 s when the
provider sends none, at least 1 s): `authorization_pending` waits again,
`slow_down` adds 5 s (or takes the provider's `interval` when longer, as
GitHub sends one), `access_denied`, `expired_token` and any other error
end the login, and so does reaching `expires_in` without asking again.
The state between polls is `Poller`, pure; the clock and the sleep are
`Clock`'s, so tests poll without waiting.

## Browser

Authorization code with PKCE `S256` (RFC 7636) and a loopback redirect
(RFC 8252). `browser::login` binds `127.0.0.1:0`, makes a 32-byte random
`state` and verifier (`random`, base64url), and opens `authorize_url`:
the claim's `authorizeUrl` with `response_type=code`, `client_id`,
`redirect_uri=http://127.0.0.1:<port>/callback`, `state`,
`code_challenge` and `code_challenge_method=S256`. Each request to the
listener (`broker::head::read_head`) goes through `callback`: only a `GET`
of `/callback` whose one `state` is this login's (compared in constant
time) counts; anything else (another path, method or state, a parameter
given twice, no code) gets a 4xx and the wait goes on. A matching `error`
ends the login as denied; a code is exchanged at `tokenUrl` with the
`redirect_uri`, `client_id` and `code_verifier`. No callback within
`TIMEOUT` (5 minutes) ends it.

## A paste login's token page

Not OAuth, but the same opener: a paste login whose claim names a
`create` page shows it and opens it through `Flows.open` (when
`openable`) before reading the secret on a terminal. `create_url` builds
it, pure, with `form_encode`: the token's name, description, owner and,
with `permissions`, the granted scopes' permissions at their highest
level ([credentials CLI](../../reference/credentials-cli.md#login)).

## The record

Either flow ends in an `OAuthToken` (`TokenSet::into_record`): the
tokens, `expires_at` and `refresh_expires_at` made absolute in unix
seconds, the scope granted, and what a refresh needs: `token_url` and the
`client_id` used (`--client-id`'s, else the claim's; a claim may name
none, leaving the app to the home, and then `login` requires
`--client-id`). `login` stores it as
`Record::OAuth`, replacing the credential's record whole, and clears the
entry's `broken` mark. The broker renews the token with the refresh token
ahead of expiry and after a 401, storing a rotated refresh token with it
([broker](broker.md#renewal), `oauth::refresh`).
