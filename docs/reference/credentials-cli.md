# `devsandbox login` and `devsandbox access`

Store a credential's secret on this host, list, approve and revoke the
claim versions it serves, and the two files that keep them. Read when changing these
commands or the home's files; the design they implement is
[credential-broker](../plans/active/credential-broker.md) (*Shape*,
"`credentials.toml`") and
[github-and-sentry-access](../plans/active/github-and-sentry-access.md)
(*The claim contract*). Unix only (the files take `serve::locked_file`'s
write discipline). Local only: both run on the host whose credentials
they are, never on a machine (`--machine` refuses them); in-process, but
for permission requests, which are the local daemon's (*Permission
requests*).

Code: `src/main.rs` (`Command::Login`, `AccessCommand`),
`src/commands/login.rs` (`login_at`, `pick`, `pending_one`, `choose`,
`obtain`, `create_url`, `check_request`, `read_check`, `check_token`, `Flows`, `Term`, `Stdio`, `styled`, `EchoOff`, `approve_or_refuse`, `approved`), `src/commands/access.rs` (`approve_at`, `ask_approve`, `review_pending`, `confirm_revoke`,
`pick_pending`, `approve_route`, `deny_route`, `pending_text`, `decide_with`, `list_at`, `uses`, `render`, `fences`, `render_fences`, `revoke_at`, `pick_any`,
`revoke_answer_at`, `forget_at`), `src/credentials/home.rs` (`Entry`, `Rule`, `Source`, `Version`,
`record_pending`, `update`), `src/credentials/answers.rs` (`Answers`,
`Remembered`, `lookup`, `forget_instance`), `src/credentials/store.rs` (`Store`,
`Record`, `OAuthToken`, `Secret`), `src/credentials/oauth/` (the OAuth
logins: [login](../architecture/credentials/login.md)), `src/credentials/review.rs` (`View`, `View::fold`, `diff`, `Diff`, `Diff::fold`),
`src/credentials/mod.rs` (`check_name`), `src/broker/audit.rs` (`Audit`,
`Line`).

```
devsandbox login <credential> [--feature <ref>] [--kind <kind>] [--profile <p>]... [--yes]
                 [--method paste|device|browser] [--client-id <id>] [--owner <user|org>] [--broad]
                 [--org <slug>]...
devsandbox access approve <credential> [--version <v>] [--yes]
devsandbox access approve <request> [--for once|instance|always]
devsandbox access deny <request>
devsandbox access pending [--json]
devsandbox access ls [<credential>] [--json]
devsandbox access revoke <credential> [--version <v>] [--yes]
devsandbox access revoke <request>
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
grant: `--profile` is refused on it (edit `profiles` in the file), and so
is `--broad` on one that isn't broad, or a login without `--broad` on
one that is (edit `broad` in the file), and `--org` (edit `orgs` in the
file). A
claim version already approved for the credential is not asked about
again.

`--broad` makes a broad credential: one whose token may be of the
claim's `tokens.broad` prefixes (the github claim's `gho_`, gh's own
OAuth app's), which reach every repo the account can access. The broker
still refuses writes outside an instance's repo fence, but GraphQL reads
aren't fenced. Refused when the claim declares no `tokens.broad`.

`--org <slug>` (repeatable) names an org a new credential reaches, for a
claim with an org fence (its `orgs` field): stored as the entry's
`orgs`. Each is one segment of letters, digits, `_` and `-`, not all
digits (that's an id, and the fence names orgs by slug), deduped
case-insensitively, the first spelling kept (`org_slugs`). Refused when
the claim (the version being approved, else the latest approved) has no
`orgs`. A login of such a claim leaving the credential with no `orgs`
is stored anyway, with a note that its org routes are refused until
`orgs` is set (`no_orgs_note`); `access approve` (and `run`'s question)
shows the same note when the version it approves brings the fence.

The login method is one of the claim's `login` (`choose`, over the
version being approved, else the latest approved): `--method` names one
the claim lists, else the first the claim lists of, on a terminal,
`device`, `browser`, `paste`, and off one `paste`, `device`, `browser`;
`browser` only where a browser opens (a display and an opener,
`open::has_display`, `open::open_argv`), since its redirect lands on
this host's loopback, and a `device` or `browser` login whose claim has
no `clientId` only with `--client-id` (`needs_client_id`; named by
`--method` without it, refused). A claim's own order isn't kept: its version sorts
the list. A claim's `device` or `browser` login marked `broad` yields a
broad token: only `--broad` uses it, and `--broad` takes no other
method but `paste` (so `gh auth token | devsandbox login gh --broad
--method paste --yes` works); a claim may list a method twice, broad
and not. A login's `scope` (OAuth scopes, space-separated) is sent as
the device code request's or the authorize page's `scope`. `github-app-manifest` is refused as not yet, and a claim with
no `login` (no `inject` slot) has no secret to log in. All of this is
checked before anything is asked.

`--client-id` logs in to that OAuth app instead of the claim's (an app
the home owns: a GitHub App you made), and is required when the claim
names none; refused with `paste`. It goes into
the token record, never the claim, so it changes no claim version.

`--owner` names the user or org a paste login's token page prefills as
the token's resource owner (`target_name`); refused unless the chosen
login is a paste with `create`.

A claim version is approved after its review, the version as a tree
(`View::fold`): ``claim `<kind>` (<mode>)`` with its short version, the
upstreams (inline when one, else listed), `scopes` with each scope's route count, its routes under it,
and `never` (red) with every route no ask or rule opens. `scopes` and
`never` start folded, showing their first two lines and `… N more`. On
a terminal the question is a pick (docs/architecture/cli-prompts.md):
Approve (`a`) or Refuse (`r`), Refuse preselected; Esc or Ctrl-C
refuses too. Tab or ↑ past the first answer moves into the tree, where
→ opens a node and ← closes it. Answered, the tree stays in scrollback
folded to one line per section, then `? <question> › <answer>`. When it
can't draw in place (stderr not a terminal, `TERM=dumb`), the tree
prints unfolded but for each scope's routes (its count shows) and the
answer is read as a number or hotkey (empty: Refuse).
`--yes` approves unasked and off a terminal without it the login is
refused; both print the tree first, the same way.

On stderr, a line starting `warning:` or `note:` has that word in
yellow or cyan, the token page's URL is bold and `approved claim
version …` green (`styled`), when stderr is a terminal and `NO_COLOR`
is unset; the final line on stdout is green under `src/render.rs`'s
rule.

Then, in order: a new version's review and the question
(`approve_or_refuse`); for a new credential with `--broad`, a warning of
what it reaches and the same kind of question, with no review (a re-login
of a broad one isn't asked: its reach was agreed when it was made); the
secret (`obtain`), by:

- `paste`: from stdin (`Stdio::secret`: on a terminal, after the claim's
  paste `hint` as `? <hint>:` (styled as a question), with echo off
  (`EchoOff`, termios `ECHO` cleared, line
  editing kept); else all of stdin, so `login x < file` works), trimmed,
  empty refused. Stored as a static token. When the claim's paste login
  has a `create` page (not with `--broad`: the page makes a narrow token) (`{"url": …, "permissions": bool}`; https, no query
  or fragment) and stdin is a terminal, it first shows that page and opens
  it (`oauth::openable`, through `Flows.open`), prefilled by `create_url`:
  `name` `devsandbox <credential>` (cut to GitHub's 40 characters),
  `description` naming the credential and its profiles (the existing
  entry's, else the `--profile`s; none: every profile), `target_name`
  from `--owner`, and with `permissions` each permission of the granted
  scopes (the claim's within the entry's `max`; a new credential: all) as
  `<permission>=<level>`, at the highest level any of them asks. No
  `expires_in`: GitHub's default. The text asks to select the
  repositories and, with `--owner`, to check the resource owner (GitHub's
  page can show the org yet create the token under the user).
- `device`: the device flow; it prints the provider's page and code (and
  opens the page where a browser opens) and waits until the code is
  entered and approved, denied, or expired.
- `browser`: the authorization code with PKCE; it prints and opens the
  provider's authorize page and waits up to 5 minutes for the redirect.

Every page opens in the browser of the credential's profile when it has
exactly one (the existing entry's `profiles`, else the `--profile`s):
that profile's `open` command in `profiles.toml`
([profiles](../architecture/instances/profiles.md#on-the-opening-host)),
else the system opener. Several or every profile, a profile the file
doesn't map, or an unreadable file (a warning): the system opener
([login](../architecture/credentials/login.md#where-pages-open)).

A claim whose `tokens.accept` lists prefixes takes only a secret one of
them starts (`tokens::check_accepted`), pasted or an OAuth login's
access token, and a broad credential one of `tokens.broad` too; any
other is refused before anything is written, naming the accepted
prefixes and the secret's own (up to its first `_`, when short). A
secret of a `tokens.broad` prefix is refused from a credential that
isn't broad, even when `accept` is empty, naming `--broad`. The github
claim accepts fine-grained PATs (`github_pat_…`) and GitHub App user
tokens (`ghu_…`), and gh's own OAuth token (`gho_…`) from a broad
credential only; a classic PAT (`ghp_…`) is refused.

The claim's `tokens` and `login` fields `login` reads (validated by
`claim::parse`; a change is a new version, shown by `access approve`):

- `tokens.accept`, `tokens.broad`: up to 8 prefixes each (letters,
  digits, `_`, `-`, at most 32), sorted; no prefix of one list starts
  or is started by one of the other.
- `login[]`: `{"method": "paste", "hint"?, "create"?}`,
  `{"method": "device", "clientId"?, "deviceUrl", "tokenUrl", "scope"?,
  "broad"?}`, `{"method": "browser", "clientId"?, "authorizeUrl",
  "tokenUrl", "scope"?, "broad"?}`, `{"method": "github-app-manifest"}`.
  A method once, or twice as a broad and a narrow one; a `broad` login
  needs `tokens.broad`; `scope` is RFC 6749 scope tokens separated by
  single spaces.

An accepted secret is then checked (`check_token`) when the claim has a
`check` route: that `GET`/`HEAD` on the claim's first upstream (as
the claim lists them: their order is pinned), the secret in the claim's first `inject` slot, as the
broker calls it (`check_request`), over `Flows.http` (the private
address override only for a `private` upstream). The answer
(`read_check`):

- 401: refused, `the token was refused by <host> (401)`, nothing
  written. Only a 401, as the broker's own check reads it: a 403 may be
  a rate limit or an org policy, not a bad token.
- 2xx: the claim's `checkShows` field of the JSON body (a top-level
  string, e.g. GitHub's `login`) is whose the token is; the claim's
  `tokens.expiry` header (`tokens::captured`) is a pasted secret's
  `expires_at` (an OAuth record keeps its own expiry).
- Anything else, or no answer (offline, a 403, a 5xx): a warning, the secret
  stored unchecked.

Then each `--org` is checked with the secret (`check_org`): the claim's
`orgs.resolve` for its slug (`orgs::resolve_path`), sent as the check is
(`credentialed`: first upstream, first slot, `Flows.http`), whether or
not the claim has a `check`. The answer (`read_org`):

- 2xx naming an id in the claim's `orgs.id` field (`orgs::parse_id`):
  reachable.
- 403 or 404: `org <slug> isn't reachable with this token (<status>)`,
  nothing written.
- 401: refused, as the check's 401.
- Anything else (no answer, a 5xx, a 2xx naming no id): a warning, the
  org stored unchecked.

The record goes into the store first (an approved claim with no secret
serves nothing), replacing any earlier one whole; then the entry (created
with `--profile`s as `profiles`, none: unset, and `--org`s as `orgs`) and the approved version
into `credentials.toml`. A re-login touches the record only, but for
clearing `broken` and setting `expires_at` to the check's (else unset).
It prints `logged in …` with `as <whose>` when the check showed it
(escaped, `open::printable`), then the approved version, or `secret
replaced`.

## `access approve`

`approve_at`: the credential's pending version (`pick_pending`: `--version`
as a unique prefix, else the only one; none or several refused), shown
(`review_pending`) as its diff against the latest approved version
(`review::diff`, as a tree by `Diff::fold`: upstreams `+` green / `-`
red, routes added and removed per scope, `(new)` and `(dropped)` scopes
folded and edited ones open, `never` routes `+`/`-`, open, and `also
changed:` naming every other pinned field that differs, `mode`, `inject`,
`login`, `checkShows`, `refusal`, `tokens`, a kept scope's `permissions` and a login's `create`
page or `scope`, `login.<method>.create` and `login.<method>.scope`
(a broad login's method read `<method>.broad`), among them), or its
review as `login` shows it when none is approved. Same question as
`login`; the final line is green on a terminal. Approval moves
the version from `pending` to the end of `approved`; older approved
versions stay (instances pinned to them keep working). The secret is
untouched.

### At `run`

A `run` whose feature brings a claim version its credential hasn't
approved records it pending, then on a TTY shows the same review and
asks "Approve this claim for `<credential>`?" (`ask_approve`, the same
pick): Approve approves it as above and the run goes on (through the
daemon: the run is rolled back and resent, docs/architecture/daemon/verbs.md,
*The CLI side*); Refuse or a cancel refuses the run, naming `devsandbox access approve
<credential>`, then try again. Off a TTY, with `--json` or `-d`, from the
dashboard, and for `rebuild` or a machine's run, it refuses without
asking. A refused `run` is rolled back, so trying again is another `run`
under the same name; a refused `rebuild` keeps its instance as `failed`,
and its refusal names `devsandbox rebuild <instance>` instead.

## Permission requests

A call an `ask` holds raises a request in the daemon
([broker](../architecture/credentials/broker.md#permission-requests));
these three are clients of its `access.*` methods
([API](api.md#access)), connecting as `devsandbox machine` does (the
lazy start included).

`access pending` (`pending_text`) lists the unanswered requests, oldest
first, or `no pending requests`:

```
ID              INSTANCE  CREDENTIAL  SCOPE        REPO  CALL                   AGE  HELD
R-7Q2M9XK4TB0D  web-1     gh          pr.write     o/r   POST /repos/o/r/pulls  now  yes
R-3JX8W1PZ6N2C  api       sentry      issues.read  -     GET /api/0/issues/     2m   no
```

`REPO` is `-` for a scope with no repo fence; a presence request's
`CALL` ends with its target, `DELETE /repos/o/r/git/refs/tags/v1
(presence: refs/tags/v1)`; `HELD` says whether a call
waits on it now (`yes (N calls)` for several); `no`: its hold ended, and
only a late answer is left. `--json` prints `{"schema", "data":
{"requests": [request…]}}`, each request as the API's.

`access approve <request>` and `access deny <request>` (`decide_with`):
a request id (`asks::is_request_id`) as the positional makes `approve`
answer that request (`approve_route`): `--for` once (the default),
`instance` or `always`, as `access.decide` takes them; `--version` with a
request is refused, and so is `--for` with a credential; `--yes` changes
nothing for a request, which isn't asked about (the user named it). `deny`
takes a request id only (`deny_route`), refusing anything else with a
pointer to `access revoke <credential> --version`. Both first show the
request as listed, then answer it; `--for always` on a presence request
is refused there, unsent ("request R-… is a presence request: answer it
`--for once` or `--for instance`, never always"):

```
request R-7Q2M9XK4TB0D: `web-1` (sandbox web) asks `gh` pr.write on o/r for POST /repos/o/r/pulls
approved R-7Q2M9XK4TB0D for this instance: `web-1` `gh` pr.write on o/r, until its `rm` (`devsandbox access revoke R-7Q2M9XK4TB0D` drops it)
```

The other answers print "approved R-… once: `web-1` `gh` pr.write on
o/r" (with no call held: "(no call waits on it: the instance's next
matching call within 5 minutes takes it)" appended), "approved R-…
always: a rule on `gh` lets sandbox web's pr.write on o/r through
(`devsandbox access revoke R-…` drops it)", "denied R-…: `web-1` `gh`
pr.write on o/r, kept until `devsandbox access revoke R-…`". A presence
request's scope reads `git.push on o/r for refs/tags/v1`. A request
not listed, or answered between the listing and the answer
(`not-found`): "no pending request `R-…` (answered already, or
expired)".

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
  rules
    releases.write, sandbox web  (R-7Q2M9XK4TB0D)
    issues.read, profile work  (by hand)
  answers
    web-1  other  this instance  R-3JX8W1PZ6N2C  2026-11-23T10:00:00Z
    api    other  deny  R-K5D0V9HQ2M7S  2026-11-23T10:05:00Z
```

- `token`: the store's record kind (`token`): `static`; `oauth` with
  the access token's expiry (`expires` / `expired`, or `no expiry`),
  whether it has a refresh token and until when, and the granted
  `scope`; `none stored` (no record: `devsandbox login`); `unreadable`
  with the store's error, which never holds its content.
- `profiles`, `max` (unset: every profile, every scope), then
  `members`, `approvers`, `delegate`, `as` when set, and `broad` for a
  broad credential.
- `orgs`: the credential's orgs when set; `none (org routes refused)`
  when unset and its latest approved claim has an org fence.
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
- `rules`: the credential's `rules`, each its scope (`on <repo>` when
  set), the fields it sets and the request that wrote it, or `by hand`.
- `answers`: the answers `access.json` keeps on it: instance (its name
  when answered), scope (`on <repo>`, `for <target>` for a presence
  answer, escaped), `this instance` or `deny`, the request id and when.

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
"approvers"?, "delegate"?, "as"?, "broad", "orgs", "org_fence", "approved": [{"version", "latest",
"unused"}], "pending": […], "broken", "expires"?: {"at", "days",
"expired", "warn"}, "instances": [{"instance", "version", "approved",
"unknown"?}], "rules": [{"scope", "repo"?, "sandbox"?, "profile"?,
"id"?}], "answers": [<an `access.json` entry>]}` (full version hashes; `oauth` carries `expires_at`,
`refresh_expires_at`, `refreshable`, `scope`; `expires.at` is unix
seconds; `org_fence`: the latest approved claim has an `orgs` field).

## `access revoke`

`revoke_at`, after showing what goes and asking (`confirm_revoke`: Revoke
(`r`), or Drop (`d`) for a version, against Keep (`k`), preselected; a
cancel keeps; `--yes` revokes; off a terminal without `--yes`, refused):

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
- A request id (`R-` and 12 Crockford base32 characters,
  `asks::is_request_id`; never a credential name, which has no
  uppercase letter), `revoke_answer_at`: drops the answer that request
  left, its entry in `access.json`, else its rule in
  `credentials.toml`, without asking (its calls are just asked again):
  "dropped the deny answer of request R-… (`gh` pr.write on o/r for
  instance `web-1`): its calls are asked again", "dropped the rule of
  request R-… on `gh` (pr.write on o/r, sandbox web): its calls are
  asked again". None left: "no rule or kept answer from request R-… on
  this host". `--version` with a request id is refused.

## `access forget`

`forget_at`: drops `<repo>` (`owner/repo` or a GitHub URL of it,
normalized as the fence is: `fence::repo_arg`) from the GitHub fence
recorded for `<folder>` (canonicalized; a folder since moved or deleted
is named as recorded). Narrow only: nothing re-adds it, since the record
is never re-read; `access.github.repos` in the config adds repos. A
folder with no record, or a repo not in its fence (the fence is listed),
is refused. The instances on that folder, named in the output, keep
their snapshot until their next `start` or `rebuild`.

The `access` commands but the request ones work on the files directly,
in-process; no API method reaches them.

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
broad = true                  # takes the claim's `tokens.broad` too (`login --broad`); unset: false
orgs = ["acme"]               # the orgs, by slug, a claim with an `orgs` field may reach; unset: none
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

[[sentry-work.rules]]         # an asked scope allowed without asking: an *always* answer
scope = "releases.write"
repo = "o/r"                  # these four optional; each set one must equal the call's
sandbox = "web"               # the instance's sandbox
id = "R-7Q2M9XK4TB0D"         # the request it answered, for `access revoke`

[[sentry-work.rules]]         # hand-written: no `id`
scope = "issues.read"
profile = "work"              # the instance's profile
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

`broad` is set by the `login --broad` that creates the credential and
only read by `login` (which token it takes, and that the flag matches);
the broker serves whatever the store holds, so turning it off by hand
takes effect at the next login.

`orgs` applies to a credential whose claim has an `orgs` field (an org
fence over one path variable, sentry's `{org}`): the orgs, by slug, its
routes may name, an org matched by its slug (case-insensitively) or its
id. A config's `access.<kind>.orgs` narrows it, never widens it. Unset:
no route holding the variable is reachable. Set by `login --org`, else
by hand, as the rest of the grant (`Entry::orgs`).

`rules` (`Entry::rules`, `Rule::matches`) let an asked scope through
without a permission request: a rule matches a call of its `scope` when
each field it sets equals the call's, `repo` (`owner/name`, the repo a
fenced claim's call names) case-insensitively, `sandbox` the instance's
sandbox name, `profile` its profile. An *always* answer appends one with
the request's scope, repo, sandbox and id
([broker](../architecture/credentials/broker.md#permission-requests));
an `id` must be a request id, a `profile` a profile name. A deny kept in
`access.json` beats a rule. No rule answers a presence call
([broker](../architecture/credentials/broker.md#presence)).

`expires_at` (RFC 3339, UTC) is written by `login`, from its token
check, and by the broker: on an injected call with a pasted secret, the response's header the claim's
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
refresh_token = "ghr_…"       # these four optional: as the provider sent them (a `gho_`: none but `scope`)
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

## `access.json`

Next to `state.toml` (`answers::path`, `daemon_config::ACCESS`), read
and written under `access.lock` (`answers::update` over
`serve::locked_file::update`): the *this instance* and deny answers of
permission requests, one per instance, credential, scope, repo and, for
a presence request, route and target (a later answer replaces it). Written by the daemon's `asks::Registry`, by
`rm` (`answers::forget_instance` drops the instance's) and by `access
revoke <request>`; read by the broker per call.

```json
{
  "version": 1,
  "answers": [
    {
      "id": "R-3JX8W1PZ6N2C",
      "instance_id": "…",
      "instance": "web-1",
      "credential": "github-work",
      "scope": "pr.write",
      "repo": "o/r",
      "answer": "instance",
      "at": "2026-11-23T10:00:00Z"
    }
  ]
}
```

`answer` is `instance` or `deny`; `repo` only for a fenced claim's
scope; `presence` only for a presence request's, `{"route": "DELETE
/repos/o/r/git/refs/tags/v1", "target": "refs/tags/v1"}` (`asks::PresenceAt`);
`instance` is the name when answered, for display; `at` RFC 3339 UTC. Another `version` fails the file, as do unknown keys.

Both credential files, their locks, `access.json` and the member CA's
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
| `verdict` | `allow` (injected), `uninjected`, `outside` (sent stripped, outside the repo fence), `refused`, `pending` (held, unanswered, or past the instance's cap of open requests), `denied`, `leak`, `error` |
| `reason` | a refusal's or failure's one line; a skipped `rewrite` |
| `status` | what the tool got (the upstream's, or the broker's own); `0` when an intercepted connection was closed unanswered (not intercepted, no TLS, a failed handshake) |
| `phantom` | whose phantom the call carried: `own`, `other-claim`, `foreign`; `null` for none |
| `refreshed` | `true` when the call renewed the credential's token; absent otherwise |
| `request` | the permission request (`R-…`) the call was held on, or the one a refusal names (past the cap, a kept deny); absent otherwise |
| `decided` | how the call's last hold ended: `once`, `instance`, `always`, `deny`, `timeout`; absent for a call never held |

How calls are decided: [broker](../architecture/credentials/broker.md).
