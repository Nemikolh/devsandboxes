> Archived 2026-11-22. Historical, may be wrong; do not use as a reference.
> Current: [credentials/login.md](../../architecture/credentials/login.md),
> [credentials/gh.md](../../architecture/credentials/gh.md),
> [credentials CLI](../../reference/credentials-cli.md),
> [ADR 0058](../../decisions/0058-a-broad-token-is-the-homes-opt-in.md)

# GitHub login

Status: agreed; building. Builds on
[github-and-sentry-access](../active/github-and-sentry-access.md) (the github claim,
`devsandbox login`).

## Goal

Getting a GitHub credential onto the home should take one command, for a
user in an org where they can't install a GitHub App:

- the narrow path (a fine-grained PAT) opens GitHub's creation page
  prefilled from the claim, instead of the user picking permissions by hand;
- every login says which account it logged in as, so a "work" credential
  can't silently be the personal account;
- login pages open in the browser of the credential's profile
  (`profiles.toml`), where that account is signed in;
- a broad opt-in: gh's own device flow (GitHub CLI's OAuth App), giving a
  `gho_` token that reaches every repo the user can. The broker's fence
  still refuses writes outside it; GraphQL queries stay unfenced (a known
  limitation, not waited on).

Out of scope: GitHub App installation tokens (parked), fencing GraphQL
queries.

The github claim's source of truth is the features repo
(github.com/devsandboxes/features); this repo changes its mirror,
`tests/fixtures/features/github/devcontainer-feature.json`. The features
repo gets the same claim change once this lands (by the user).

## Step 1: paste opens the token creation page, prefilled

A paste login may name a page to create the token on; login opens it
before reading the secret.

- Claim: `Login::Paste` (`src/credentials/claim.rs`, `Login`, validated in
  `fn login`) gains an optional `create` object: `url` (https only, no
  query), and `permissions: bool` (append the granted scopes'
  permissions as `<permission>=<level>` query params, the highest level
  per permission across the scopes). Data only, like the rest of the claim.
- Granted scopes: the claim's scopes ∩ the entry's `max` (`--max` doesn't
  exist; an existing entry's `max`, else every scope). Scopes with no
  `permissions` add nothing.
- Fixed params devsandbox adds: `name` = `devsandbox <credential>`
  (GitHub caps it at 40 chars: truncate), `description` naming the
  credential and its profiles. New login flag `--owner <user|org>` →
  `target_name`; no `expires_in` (GitHub's default).
- `obtain`'s `Login::Paste` arm (`src/commands/login.rs`) shows the URL
  and opens it through `flows.open` (the same seam `device.rs` uses; only
  if it passes `device.rs`'s `openable`, move that to `oauth/mod.rs` or
  `open.rs` to share), then reads the secret as today. The shown text says
  to select the repos and, with `--owner`, to check the resource owner
  (GitHub's prefill can show the org but create under the user).
- Fixture: the github claim's paste login gets
  `"create": {"url": "https://github.com/settings/personal-access-tokens/new", "permissions": true}`.
- Tests: URL building is a pure fn (permissions merged to the highest
  level, name truncation, `--owner`, percent-encoding); `obtain` opens it
  (the `Script`/`offline()` seams in `login.rs` tests).
- Docs: `docs/reference/credentials-cli.md` (`login`, `--owner`),
  `docs/architecture/credentials/login.md`, `docs/architecture/credentials/gh.md`,
  the claim spec where `login` methods are documented (grep `hint`).

## Step 2: login checks the token and says whose it is

After `obtain` and the accept check, before `Store::put`
(`login_at`, `src/commands/login.rs`), login calls the claim's `check`
route (`Pinned.check`, e.g. `GET /user`) on the claim's first upstream
with the token, over `flows.http`.

- `Http` (`src/credentials/oauth/mod.rs`) gets a `get(url, bearer)`;
  the injected Http seam stays the only network path.
- 401/403: refuse, store nothing ("the token was refused by <host>").
  Other failures (network, 5xx): warn and store (a home may be offline).
- Claim: optional `checkShows` (a top-level JSON string field name of the
  check's response, e.g. `"login"`): login prints `logged in as <value>`
  (escaped with `open::printable`). Fixture: `"checkShows": "login"`.
- A claim with no `check`: no call (sentry today, unless its claim has one).
- The response's `tokens.expiry` header (`tokens::captured`) sets the
  entry's `expires_at` right away for a pasted token, instead of waiting
  for the broker's first call.
- Tests: a mock upstream (`broker::serve::tests::upstream_server`, as
  `oauth::tests` does) answering 200 with `{"login":"alice"}` + expiry
  header, 401, and 500.
- Docs: `credentials-cli.md`, `login.md`, the claim spec.

## Step 3: login pages open in the profile's browser

`Flows::host` (`src/commands/login.rs`) opens with the system opener;
it should go through `open::open_argv(os, profile, &profiles, url)`
(`src/open.rs`, `load_profiles(&profiles_path())`), the same mapping
container URLs use.

- The profile: the credential's `profiles` (an existing entry's, else
  the `--profile` flags) when it names exactly one; otherwise none
  (system opener). No new flag.
- `Flows.browser` (whether a browser login is possible) follows the same
  argv.
- An unreadable `profiles.toml`: warn, system opener.
- Tests: the profile choice is a pure fn (one, several, none, existing
  entry vs flags); the argv comes from `open_argv`, already tested.
- Docs: `login.md`, `credentials-cli.md`, and
  `docs/architecture/instances/profiles.md` (another opener of the host).

## Step 4: the broad opt-in, gh's device flow

The claim says which broad tokens and logins exist; the home opts in.

- Claim `tokens` gains `broad: [prefix…]` (validated like `accept`,
  disjoint from it): prefixes accepted only from a broad credential.
  `Login::Device` (and `Browser`, for symmetry) gains optional `scope`
  (sent as `scope` in the code/authorize request) and `broad: bool` (the
  method yields a broad token: usable only with `--broad`, and `--broad`
  picks only broad methods).
- Home: `Entry` (`src/credentials/home.rs`) gains `broad: bool`
  (default false, `deny_unknown_fields` stays), set by `login --broad`
  on a new credential; `--broad` on an existing non-broad one, or a
  plain login on a broad one, refuses (a grant change is a file edit,
  like `profiles`). `access ls` shows it.
- `tokens::check_accepted` takes the entry's broadness: broad accepts
  `accept` ∪ `broad`, else `accept` only. The refusal for a `gho_`
  without `--broad` names `--broad` and what it means (every repo you
  can access).
- `--broad` on a TTY asks a confirmation stating that reach; off a TTY it
  needs `--yes`, like the claim approval.
- Device flow (`src/credentials/oauth/device.rs`): send `scope` when the
  claim's login has one. A `gho_` reply has no `expires_in` and no
  `refresh_token`: stored as `Record::OAuth` with neither, which the
  renewer already leaves alone (`refresh.rs` `due`/`renew`).
- Fixture: `tokens.broad: ["gho_"]`, and a second device login
  `{"method":"device","clientId":"178c6fc778ccc68e1d6a","scope":"repo read:org","broad":true,…}`
  (GitHub CLI's public client id; no `workflow` scope).
- Tests: claim validation (disjoint, broad needs a client id or
  `--client-id`); `choose` with and without `--broad`; device flow sends
  `scope` (`oauth::tests::device_provider` records the form; add a `gho_`
  reply without refresh); `check_accepted` both ways; the entry round-trip.
- Docs: `credentials-cli.md` (`--broad`, `broad` in `credentials.toml`),
  `login.md`, `gh.md`, the claim spec. `docs/status.md` known
  limitations: a broad credential's GraphQL reads reach every repo the
  user can; GitHub keeps a limited number of tokens per user, app and
  scope set and revokes the oldest, so `gh auth login`s elsewhere and
  devsandbox's can log each other out.

## Closing

Living docs checked against the code, the `In progress` line dropped,
this plan archived.
