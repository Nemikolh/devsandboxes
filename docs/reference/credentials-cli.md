# `devsandbox login` and `devsandbox access`

Store a credential's secret on this host, list, approve and revoke the
claim versions it serves, and the two files that keep them. Read when changing these
commands or the home's files; the design they implement is
[credential-broker](../plans/active/credential-broker.md) (*Shape*,
"`credentials.toml`") and
[github-and-sentry-access](../plans/active/github-and-sentry-access.md)
(*The claim contract*). Unix only (the files take `serve::locked_file`'s
write discipline). Local only: both run in-process on the host whose
credentials they are, never on a machine (`--machine` refuses them).

Code: `src/main.rs` (`Command::Login`, `AccessCommand`),
`src/commands/login.rs` (`login_at`, `pick`, `pending_one`, `choose`,
`obtain`, `Flows`, `Term`, `EchoOff`, `approve_or_refuse`), `src/commands/access.rs` (`approve_at`,
`pick_pending`, `list_at`, `uses`, `render`, `fences`, `render_fences`, `revoke_at`, `pick_any`,
`forget_at`), `src/credentials/home.rs` (`Entry`, `Source`, `Version`,
`record_pending`, `update`), `src/credentials/store.rs` (`Store`,
`Record`, `OAuthToken`, `Secret`), `src/credentials/oauth/` (the OAuth
logins: [login](../architecture/credentials/login.md)), `src/credentials/review.rs` (`View`, `diff`, `Diff`),
`src/credentials/mod.rs` (`check_name`), `src/broker/audit.rs` (`Audit`,
`Line`).

```
devsandbox login <credential> [--feature <ref>] [--kind <kind>] [--profile <p>]... [--yes]
                 [--method paste|device|browser] [--client-id <id>]
devsandbox access approve <credential> [--version <v>] [--yes]
devsandbox access ls [<credential>] [--json]
devsandbox access revoke <credential> [--version <v>] [--yes]
devsandbox access forget <folder> <repo>
```

## Names and kinds

A credential has a name (`sentry-work`, `check_name`:
`[a-z0-9][a-z0-9_-]*`, at most 64 bytes) and serves one claim kind
(`sentry`, `claim::check_kind`). A feature's claim and a config's
`access.<kind>` name the kind; `credentials.toml` and
`access.<kind>.credential` name the credential, so one host can hold one
credential per Sentry org, each granted to its own profiles.

## `login`

The claim to approve (`login_at`):

- `--feature <ref>`: the claims in that feature's metadata
  (`features::resolve`; an OCI ref, or a local folder written as a config
  writes one, `./…`, `../…`, `/…`, resolved against the current dir).
  `pick` takes the claim of `--kind`, else the feature's only one, else
  the one whose kind is the credential's name; else it refuses, listing
  the kinds.
- Without it (`pending_one`): the credential's one pending version.
  Several pending: refused, naming `--feature` and `access approve
  --version`. None pending but one approved: no claim to approve (a
  re-login). No such credential, or one with no version at all: refused,
  naming `--feature`.

An existing credential keeps its kind (another kind is refused) and its
grant: `--profile` is refused on it (edit `profiles` in the file). A
claim version already approved for the credential is not asked about
again.

The login method is one of the claim's `login` (`choose`, over the
version being approved, else the latest approved): `--method` names one
the claim lists, else the first the claim lists of, on a terminal,
`device`, `browser`, `paste`, and off one `paste`, `device`, `browser`;
`browser` only where a browser opens (a display and a system opener,
`open::has_display`, `open::opener_argv`), since its redirect lands on
this host's loopback, and a `device` or `browser` login whose claim has
no `clientId` only with `--client-id` (`needs_client_id`; named by
`--method` without it, refused). A claim's own order isn't kept: its version sorts
the list. `github-app-manifest` is refused as not yet, and a claim with
no `login` (no `inject` slot) has no secret to log in. All of this is
checked before anything is asked.

`--client-id` logs in to that OAuth app instead of the claim's (an app
the home owns: a GitHub App you made), and is required when the claim
names none; refused with `paste`. It goes into
the token record, never the claim, so it changes no claim version.

Then, in order: a new version's summary (`View::summary`: kind, mode,
upstreams, each scope with its route count, every `never` route, the
short version) and the question (`approve_or_refuse`: `--yes` approves;
off a terminal without `--yes`, refused); the secret (`obtain`), by:

- `paste`: from stdin (`Stdio::secret`: on a terminal, after the claim's
  paste `hint`, with echo off (`EchoOff`, termios `ECHO` cleared, line
  editing kept); else all of stdin, so `login x < file` works), trimmed,
  empty refused. Stored as a static token.
- `device`: the device flow; it prints the provider's page and code (and
  opens the page where a browser opens) and waits until the code is
  entered and approved, denied, or expired.
- `browser`: the authorization code with PKCE; it prints and opens the
  provider's authorize page and waits up to 5 minutes for the redirect.

A claim whose `tokens.accept` lists prefixes takes only a secret one of
them starts (`tokens::check_accepted`), pasted or an OAuth login's
access token; any other is refused before anything is written, naming
the accepted prefixes and the secret's own (up to its first `_`, when
short). The github claim accepts fine-grained PATs (`github_pat_…`) and
GitHub App user tokens (`ghu_…`), so a classic PAT (`ghp_…`) or gh's own
OAuth token (`gho_…`), which reach every repo you can, is refused.

The record goes into the store first (an approved claim with no secret
serves nothing), replacing any earlier one whole; then the entry (created
with `--profile`s as `profiles`, none: unset) and the approved version
into `credentials.toml`. A re-login touches the record only, but for
clearing `broken` and `expires_at` (the new secret has its own). It
prints `logged in …` with the approved version, or `secret replaced`.

## `access approve`

`approve_at`: the credential's pending version (`pick_pending`: `--version`
as a unique prefix, else the only one; none or several refused), shown as
its diff against the latest approved version (`review::diff`, rendered by
`Diff::render`: upstreams `+`/`-`, routes added and removed per scope,
`(new)` and `(dropped)` scopes, `never` routes `+`/`-`, and `also
changed:` naming every other pinned field that differs, `mode`, `inject`,
`login`, `refusal`, a kept scope's `permissions` among them), or its
summary when none is approved. Same question as `login`. Approval moves
the version from `pending` to the end of `approved`; older approved
versions stay (instances pinned to them keep working). The secret is
untouched.

## `access ls`

`list_at`: every credential in `credentials.toml` (or the one named),
by name, never a secret:

```
sentry-work: sentry, source file
  token     static
  profiles  work
  max       every scope
  approved  3f2a1b4c5d6e (unused), 9c1d0e2f3a4b (latest)
  pending   77ab01cd23ef
  expires   warning: in 9 days (2026-12-01T10:00:00Z): `devsandbox login sentry-work` replaces it
  instances
    web-1  9c1d0e2f3a4b  approved
    api    77ab01cd23ef  not approved
    old    3f2a1b4c5d6e  approved  (may take another credential: cannot read its config: …)
```

- `token`: the store's record kind (`token`): `static`; `oauth` with
  the access token's expiry (`expires` / `expired`, or `no expiry`),
  whether it has a refresh token and until when, and the granted
  `scope`; `none stored` (no record: `devsandbox login`); `unreadable`
  with the store's error, which never holds its content.
- `profiles`, `max` (unset: every profile, every scope), then
  `members`, `approvers`, `delegate`, `as` when set.
- `approved` (in approval order) and `pending` claim versions, by
  `Version::short`. The last approved is `latest`; a version that isn't
  and that no instance here pins is `unused` (`access revoke --version`
  drops it).
- `broken`: why the credential is marked broken, when it is (`broken`).
- `expires`: when the secret stops working, when known
  (`tokens::ends_at`): an OAuth record's refresh token's expiry (its
  access token renews until then; a record without a refresh token has
  none here), else a pasted secret's `expires_at`. `in N days` and the
  date, prefixed `warning:` with a pointer to `devsandbox login` within
  14 days (`tokens::WARN_DAYS`) or once past.
- `instances`: those in `state.toml` with a claim that resolves to it
  (`uses`): the claim's kind, the instance's config's
  `access.<kind>.credential` (its config root and sandbox loaded now) and
  its profile through `home::resolve`, as the broker does per call. Each
  shows the claim version it pinned at `run` and whether that version is
  approved for this credential. An instance whose config doesn't load, or
  whose claim `home::resolve` refuses (two credentials granted), is shown
  under every credential of its kind with the reason, and counts as
  pinning its version.

Without a credential named, the GitHub repo fences follow (`fences`,
[fence](../architecture/credentials/fence.md)): each folder in
`State::folder_remotes`, its recorded `owner/repo`s (or `none`, with why
its remotes couldn't be read), and the instances on it, an instance whose
snapshot (`Instance::github_repos`) still differs from the records marked
`(until its next start)`:

```
GitHub repo fences (each folder's remotes at its first run):
  /home/u/app  org/app, up/app
    instances  app-1, app-2 (until its next start)
```

`--json` prints the same as `{"schema", "data": {"credentials":
[credential…], "fences": [{"folder", "repos", "unread"?, "instances":
[{"instance", "stale"}]}]}}`, each credential
`{"name", "kind", "source", "token": {"type": "static" | "oauth" |
"missing" | "unreadable", …}, "profiles", "max", "members"?,
"approvers"?, "delegate"?, "as"?, "approved": [{"version", "latest",
"unused"}], "pending": […], "broken", "expires"?: {"at", "days",
"expired", "warn"}, "instances": [{"instance", "version", "approved",
"unknown"?}]}` (full version hashes; `oauth` carries `expires_at`,
`refresh_expires_at`, `refreshable`, `scope`; `expires.at` is unix
seconds).

## `access revoke`

`revoke_at`, after showing what goes and asking (`--yes` revokes; off a
terminal without `--yes`, refused):

- Without `--version`: the credential's record leaves the store, then its
  entry leaves `credentials.toml` (the secret first: a failure in
  between leaves an entry that serves nothing). Shown first: the
  instances that lose it (`access ls`'s, unknown ones as perhaps). For
  an OAuth record the token stays valid at the provider until it
  expires: no claim declares a revocation endpoint, so it says so.
- With `--version` (`pick_any`: a unique prefix of an approved or pending
  version): that version leaves the entry. For an approved one it names
  the instances pinning it, which get the not-approved refusal on their
  next call, and which version becomes the latest approved (or that none
  is left).

## `access forget`

`forget_at`: drops `<repo>` (`owner/repo` or a GitHub URL of it,
normalized as the fence is: `fence::repo_arg`) from the GitHub fence
recorded for `<folder>` (canonicalized; a folder since moved or deleted
is named as recorded). Narrow only: nothing re-adds it, since the record
is never re-read; `access.github.repos` in the config adds repos. A
folder with no record, or a repo not in its fence (the fence is listed),
is refused. The instances on that folder, named in the output, keep
their snapshot until their next `start` or `rebuild`.

The `access` commands work on the files directly, in-process; no API
method reaches them.

## `credentials.toml`

Next to `state.toml` (`home::path`, `daemon_config::CREDENTIALS`), read and
written under `credentials.lock` (`home::update` over
`serve::locked_file::update`; a failed update writes nothing). One table
per credential, unknown keys refused:

```toml
[sentry-work]
kind = "sentry"
source = "file"               # the only source; "keyring" and "op://…" refused as not yet
profiles = ["work"]           # unset: every profile
max = ["releases.write"]      # unset: every scope of the claim (the broker's grant check)
members = []                  # these three: parsed, read by nothing
approvers = []
delegate = []
as = ["user"]                 # the token kinds it offers, "user" | "app"; unset: ["user"]
expires_at = "2026-12-01T10:00:00Z"   # a pasted secret's expiry, as the upstream reported it

[[sentry-work.approved]]      # approval order: the last is the latest
version = "3f2a…"             # sha256 of `claim`, as `Claim::version`
claim = '{"kind":"sentry","claim":{…}}'   # `Claim::canonical`

[[sentry-work.pending]]       # `home::record_pending`
version = "…"
claim = '…'

[sentry-work.broken]          # `Entry::broken`, set by the broker, cleared by `login`
at = "2026-11-21T10:00:00Z"   # when first marked, RFC 3339 UTC
reason = "its refresh was refused: invalid_grant (…)"
```

`claim` is the canonical pinned claim (no container-side wiring), the copy
the broker enforces. At load (`Entry::check`, `Version::check`) every
version is re-hashed and must match its `version`, and its claim's kind
must be the entry's: a hand edit to a claim can't approve content nobody
was shown. `record_pending` takes canonical JSON (re-hashed here) for an
existing credential, and skips a version already approved or pending;
`run` calls it for a claim whose credential hasn't approved its version,
then refuses
([images and features](../architecture/instances/images-and-features.md#credential-claims)).
`home::resolve` picks the credential a claim takes: the one
`access.<kind>.credential` names, else the one of that kind whose
`profiles` grant the instance's profile.

`as` applies to a credential whose claim declares token kinds
(`tokens.as`, the github claim's `["app", "user"]`); for any other it is
ignored, and a config's `access.<kind>.as` is refused at `run`, since it
would change nothing. At `run` (`tokens::pick`) the config's
`access.<kind>.as` picks within the credential's `as`, else `user` when
offered, else `app`: a config only narrows, so a kind the credential
doesn't offer, or the claim doesn't declare, refuses the run. `app`
(installation tokens) is refused as not supported yet.

`expires_at` (RFC 3339, UTC) is written by the broker: on an injected
call with a pasted secret, the response's header the claim's
`tokens.expiry` names (GitHub's `github-authentication-token-expiration`,
`2026-12-01 10:00:00 UTC` or with a numeric offset, `tokens::parse_expiry`)
is recorded when it differs (`home::set_expires_at`). Within 7 days of a
credential's end (`tokens::NOTICE_DAYS`) the broker's injected calls
also raise a desktop notice (`devsbd::desktop::notify_desktop`), at most
once per credential per UTC day (`tokens::Notices`, in the daemon's
memory: a restarted daemon may repeat it that day).

## The file store

`credentials-store.toml` beside `member.key`
(`daemon_config::CREDENTIAL_STORE`), one record per credential name
(`Record`, a `[secret.<name>]` table, unknown keys refused), for `source =
"file"`:

```toml
[secret.sentry-work]          # pasted
type = "static"
token = "sntrys_…"

[secret.github]               # an OAuth login (`OAuthToken`)
type = "oauth"
access_token = "ghu_…"
refresh_token = "ghr_…"       # these four optional: as the provider sent them
expires_at = 1795000000       # unix seconds
refresh_expires_at = 1810000000
scope = "…"
token_url = "https://github.com/login/oauth/access_token"
client_id = "Iv1.…"           # the claim's, or `--client-id`'s
```

The broker injects the static token, or the access token, renewed with
the refresh token when due (`oauth::refresh::Renewer`, under the store's
lock: `Store::renew`;
[broker](../architecture/credentials/broker.md#renewal)). `Store::put` / `get` /
`remove` read and write it under `credentials-store.lock`, each write to a
temp file created 0600 and renamed (`serve::locked_file::write_atomic_mode`),
so no secret is ever on disk with wider permissions. A store readable by
group or others is refused (read and write), and a parse error names the
file, never its content. `Secret` has no `Display` and a blank `Debug`.

Both files, their locks, the reserved `access.json` and the member CA's
`member-ca.key` and `member-ca.toml` ([member CA](../architecture/credentials/ca.md)) are refused as bind and build sources in every container,
like `member.key` (`key_guard`, `daemon_config::credential_files`;
[fleet](../architecture/daemon/fleet.md), *Key at rest*).

## `access.log`

The broker's audit log, next to `state.toml` (`broker::audit::Audit`,
0600): one JSON object per line, one line per request a broker listener
carried, rotated to `access.log.1` (replacing it) once the next line would
take the file past 10 MiB. Never a token or a phantom.

```json
{"time":"2026-11-21T10:00:00Z","instance":"web-1","instance_id":"web-1",
 "incarnation":"…","kind":"sentry","credential":"sentry-work","method":"GET",
 "route":"/api/0/organizations/acme/releases/","scope":"releases.read",
 "verdict":"allow","reason":null,"status":200,"phantom":"own"}
```

| field | holds |
|---|---|
| `time` | RFC 3339, UTC, seconds |
| `instance`, `instance_id` | the bridge's instance (state key, persistent id) |
| `incarnation` | the instance's, when its record has one |
| `kind`, `credential`, `scope` | as far as policy got (`null` before) |
| `method`, `route` | the request's, the path before injection, query dropped; empty for an intercepted connection closed before any request |
| `verdict` | `allow` (injected), `uninjected`, `outside` (sent stripped, outside the repo fence), `refused`, `leak`, `error` |
| `reason` | a refusal's or failure's one line; a skipped `rewrite` |
| `status` | what the tool got (the upstream's, or the broker's own); `0` when an intercepted connection was closed unanswered (not intercepted, no TLS, a failed handshake) |
| `phantom` | whose phantom the call carried: `own`, `other-claim`, `foreign`; `null` for none |
| `refreshed` | `true` when the call renewed the credential's token; absent otherwise |

How calls are decided: [broker](../architecture/credentials/broker.md).
