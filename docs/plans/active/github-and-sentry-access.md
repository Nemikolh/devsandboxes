# GitHub and Sentry access

Status: agreed; building steps 1-2, steps 3-4 after credential-broker's
fleet steps. Needs [credential-broker](credential-broker.md)
step 1 for sentry (step 1 here), its step 3 for gh (step 2 here), its
step 4 for `ask` scopes (which answer 403 with the reason until
then), its step 5 for remote sandboxes; its git half needs
[git-gateway](git-gateway.md) step 1, the smart-HTTP rule table on the
`github.com` intercept on git's own proxy port (*git* below).
Design and diagrams:
[Credentials and access](../../../site/src/content/architecture/secrets.mdx).

## Goal

`gh` and `sentry-cli` work in every sandbox, on every machine, with no
token in any of them:

- gh reaches only the sandbox's repos (the remotes of its `folder` and
  `folders`); another repo is denied, never asked, and only a config
  change adds one.
- sentry-cli works only in the profiles its credential is granted to
  (`work`, not `personal`).
- The live config drops its shared `gh` and `sentry` mounts
  (`template.base.mounts`).

Both tools ship as features in `github.com/devsandboxes/features`, and the
claim contract they use is the general one any tool's feature can use.

## The claim contract

A feature declares what it needs under
`customizations.devsandbox.credentials` in its `devcontainer-feature.json`;
devcontainer tools ignore unknown customizations, so the feature stays a
plain feature elsewhere. Parsed with the rest of a feature's metadata at
`run`/`rebuild` (`FeatureMetadata` in `src/features.rs`,
[images-and-features](../../architecture/instances/images-and-features.md));
the claim is kept on the `Instance` in `state.toml` beside its
phantoms. One feature per instance claims a given credential
(credential-broker, *`credentials.toml`*).

| field | says |
|---|---|
| `upstreams` | hosts the credential is for, one marked `private` if it's on the LAN (credential-broker, *The broker*) |
| `mode` | `base-url` or `intercept` |
| `inject` | where the token goes: a header and its format, basic auth, a query parameter; two slots where clients differ (Anthropic's `x-api-key` and `Authorization: Bearer`), the one holding the phantom taken |
| `phantom` | env vars, and files from a template (`hosts.yml` is YAML, `.sentryclirc` INI) in the remote user's home; a prefix, if the client reads its mode from it |
| `git` | gitconfig devsandbox writes in the container (ssh forms to https, the credential helper; *git* below) |
| `baseUrl` | for `base-url`: the env var that takes the loopback URL |
| `rewrite` | response fields holding an absolute upstream URL, replaced with the loopback (sentry's chunk-upload `url`, its regions list's `regions[].url`) |
| `login` | how `devsandbox login` gets it: OAuth device flow, browser OAuth, GitHub App manifest, paste |
| `check` | a route the broker calls with no sandbox input to confirm a 401 means a broken credential (`GET /user`, `GET /api/0/`; credential-broker, *Lifetimes*) |
| `mint` | an adapter devsandbox ships (`github-app`, `oauth-refresh`), if any |
| `tokens` | what the secret may be: the token kinds it can act as (`as`, `user` / `app`: *Whose token*), the prefixes `login` accepts (`accept`: `github_pat_`, `ghu_`), the response header reporting a pasted token's expiry (`expiry`) |
| `scopes` | named scopes over routes and GraphQL mutations, with the app permissions each needs: `pr.write = ["POST /repos/{repo}/pulls", "graphql mutation createPullRequest", …]` |
| `never` | routes no ask or rule opens |
| `presence` | routes and ref forms that always ask a human, with the field naming each one's target (credential-broker, *`presence`*) |
| `hold` | how long an `ask` holds the call (credential-broker, *Permission requests*) |
| `refusal` | the upstream's error shape refusals take (credential-broker, *Refusals*) |
| `fence` | how a route or mutation names its repos or project: every repo- or object-naming input (`{repo}` from the path, a body's `owner`/`organization`, each mutation input holding a repository or node id) |
| `ca` | a CA knob devsandbox doesn't set by default, if the tool has one |

Data only: a claim can't run anything on the home or a member.
`credentials` is keyed by the kind, unknown fields are refused, `baseUrl`
points at the first upstream listed, `inject`'s `format` and phantom file
templates take `{token}`, `{phantom}` and (`base-url` only) `{baseUrl}`,
each `rewrite` entry names its route and field paths, `tokens` needs an
`inject` slot and names at least one of its parts (each prefix up to 32
letters, digits, `_`, `-`), and a route is in
one scope or `never`, never both (`credentials::claim`). A route's
`{var}` takes one path segment and `{repo}` two, `owner/repo` (matched as
*Matching* says); a route may end in one `?name=value` the request's query
must hold exactly once with that raw value, other parameters ignored
(`GET /{repo}/info/refs?service=git-receive-pack`), and wins over the same
route without one. `graphql query` stands for every GraphQL query
(never fenced) and `graphql mutation <name>` for one top-level mutation
field; a claim with either lists no HTTP route on `/graphql`. A
mutation's `fence` inputs are paths in its arguments
(`input.repositoryId`, `input.branch.repositoryNameWithOwner`, `[]` for
each element of a list): one named `…NameWithOwner` holds `owner/name`,
any other a global or repository id. A claim whose `fence` names any route is fenced: its
`{repo}` routes and its mutations' inputs are checked against the instance's repo fence.

The home binds the secret to its destination. It pins the whole claim but
its container-side wiring (`phantom`, `baseUrl`, `ca`, `git`: the
container could ignore them anyway) in `credentials.toml`, under the
claim's version: a hash of that pinned part as `FeatureMetadata` reads
it, the same for an OCI or a local feature; a `device` or `browser`
login with no `clientId` (the home names its own app at `login`) is
pinned as having none, so adding or dropping one is a new version. Not
the feature's digest:
`fetch_into` keeps none (its cache is keyed by tag), a local feature has
none, and a gh release that leaves the claim alone must not ask again.
`devsandbox login <credential>` shows and approves the claim of the
feature it names (`--feature ghcr.io/devsandboxes/features/gh:1`), else
of the version pending for that credential; a later one takes
`devsandbox access approve <credential>` on the home or an approver,
which shows the diff (new upstreams, routes per scope, `never` changes)
and leaves the secret alone. The home learns a version's content from
the instance's host, which sends the canonical pinned claim (not only
its hash) the first time it uses a version the home lacks; the home
re-hashes it, refuses the call and keeps it pending, listed in `access
ls`, for `access approve` to diff. The broker uses the home's
copy, for local and remote instances alike; an instance's claim only
selects a credential by name and version. The home keeps every version it
approved and serves each instance the one it ran with, so a new version
never breaks a running instance; a version goes with its last instance or
by `access revoke`. An unapproved version is refused per call, naming
`devsandbox access approve <credential>`, and at `run` when the home
answers (a warning when a remote home is offline). So no feature, whatever its tag resolves to, points a token at
another host, attributes a route to another repo, or redefines a scope to
cover more routes.

### The installer itself

`devsandboxes/features/gh` installs gh itself rather than layering on
`ghcr.io/devcontainers/features/github-cli`: one entry in a config, and
one update path everywhere. As `features/zidane` does, a `gh-update`
puts a gh release in a shared bin dir (a `shared-volumes/` mount) and a
`gh` launcher runs the version it made current, else the one baked at
build, so one update reaches every sandbox without a rebuild. The bin
dir is mounted read-only and updated on the host (agent-credentials,
*Nothing shared writable that runs or steers*). `gh-update` goes to the
latest release; the claim stays the one the feature shipped (its version
pins the claim, not gh's), so a route a newer gh calls that the scope
table can't place is denied until a feature release covers it. A config
drops the official feature, or two gh binaries race on `PATH`.

## gh

- **Reaching the broker**: intercept mode (gh always talks to
  `api.github.com` and `uploads.github.com`, the hosts claimed on the shared proxy port; Go honors `HTTPS_PROXY` and reads the system bundle
  on Linux, so the member CA works with a stock gh). devsbd sends only
  claimed hosts over the bridge; a `CONNECT` to any other host it dials
  itself from the container, through the proxy the container had if any,
  so npm, cargo and curl traffic stays where it was and `serve` down
  stops only the claimed hosts. `HTTPS_PROXY`/`NO_PROXY` and the CA env
  go on the container env beside the phantoms (proxy port fixed at
  create), so VS Code terminals and tasks and a plain `docker exec` reach
  the broker too instead of sending the phantom to GitHub. `GH_TOKEN` is
  the phantom, so `gh auth login` refuses and `gh auth status` reports
  logged in (its GraphQL `viewer{login}`, then `GET /`, credentialed:
  *Routes it can't*); `hosts.yml` gets the phantom for tools that read it. `gh auth
  token` prints the phantom: harmless. gh checks no token itself (any
  shape passes) and never refuses up front: it sends the phantom and
  passes GitHub's or the broker's error on. The feature sets
  `GH_TELEMETRY=false` beside the phantom: gh posts command names and
  flags to `cafe.github.com`, unclaimed, no credential, egress no one
  asked for. `github.com` isn't claimed there
  (*git* below), so release downloads by uv, rustls installers or a
  browser never meet the member CA or the broker.
- **The repo fence**: the remotes of `folder` and `folders` (`github.com`
  URLs in https, `ssh://` and scp form, from `remote.*.url` and `pushurl`
  only, never a `branch.*.remote` URL; an ssh host alias like
  `git@github-work:` resolved with `ssh -G -- <alias>` on the host, the
  alias first checked to be a hostname (`.git/config` wrote it, and `-G`
  runs `Match exec`), and rewritten by
  the instance's `git` config too), read once, never by host git, at the
  first `run` of that folder, and recorded
  per folder in `state.toml` with the folder's other remotes
  ([git-gateway](git-gateway.md), *The folder's remotes*) (keyed by its canonical host path, kept past
  `rm`), snapshotted on each `Instance`. `.git/config` is
  container-writable (the folder is bind-mounted, worktrees share the
  base's `.git`) and `host_git` lets remote URLs through, so the fence is
  never re-read from it: `start`/`rebuild` and later instances reuse the
  record, and a remote added later counts only once the config names it.
  A folder containers mounted before this shipped may already hold such a
  remote, so the first `run` shows the recorded fence, as `access ls`
  does, and `devsandbox access forget <folder> <repo>` drops an entry
  (narrow only, host-local, applied to instances at their next `start`
  or `rebuild`), for a planted remote or a repo moved away. `.gitmodules` never counts. `[sandbox.x.access.github] repos = […]` adds
  more; arrays concatenate through `extends`. A remote instance's record
  lives on its host and travels in the signed claim (the member is trusted
  for it as for the instance's identity); a placed run takes the placing
  instance's. A dispatcher reaches its children's fences too
  ([agent-credentials](../explore/agent-credentials.md)).
- **Matching**: owner and repo compare case-insensitively, with `.git`
  and `.wiki.git` reduced to the repo. Owner, repo and
  `/repositories/{id}` are strict: an encoded character, a dot-segment or
  `//` there is refused, not normalized, as are dot-segments (`..`,
  `%2e%2e`) and `//` anywhere in the path, on every claimed host, before
  anything is injected: a prefix match takes
  `/repos/{allowed}/../{other}` for the allowed repo, and
  `uploads.github.com` resolves `..` (a 301 to the other repo's path). Past the repo `%2F` is decoded
  once and the decoded path matched (gh's `safeurl.JoinPath` sends
  `git/ref/tags%2Fv1` and `contents/.github%2Fworkflows%2Fci.yml`), so a
  branch rule on `git/refs/heads/{branch}` binds `heads%2Fmain`; double
  encoding and dot-segments after decoding are refused. The fence resolves to
  repo ids and node ids when recorded (the home's own `GET
  /repos/{owner}/{repo}`, following a rename's 301; a tool's redirects are
  never followed with the credential), so `/repositories/{id}/…` matches, a renamed repo keeps
  working, and whoever re-registers a renamed account's old login gets
  nothing.
- **Outside the fence** a request goes upstream from the instance's host
  with the phantom stripped and no credential, its response marked
  `devsandbox-fence: outside` (credential-broker, *Policy*): public data
  over REST and git works (cargo and npm git dependencies, submodules,
  `gh api`, release downloads, `gh search repos`), private repos answer
  404 as to a stranger. GitHub serves no anonymous GraphQL (a 403), so
  nothing GraphQL-backed works stripped, public repos included (`gh repo
  view`, `issue`/`pr list`, `gh repo clone`'s lookup, `auth status`):
  those gh commands work out of the fence only because GraphQL queries
  are never fenced (*Routes it can't*). The fence keeps the credential, not the network (the
  broker's non-goal). Stripped REST reads share the host IP's anonymous
  limit (60 an hour, the host's own calls included); a write gets the broker's refusal naming `access.github.repos`,
  not GitHub's 401, which sends tools to `gh auth login`. Smart HTTP to a
  private or missing repo answers 401, which sends git to a credential
  prompt (a hang without a TTY): out of the fence the broker answers that
  401 with its own 403 and the same reason.
- **Routes the broker can attribute**: REST `/repos/{owner}/{repo}/…`,
  `/repositories/{id}/…`, release asset uploads on `uploads.github.com`,
  git smart HTTP, and GraphQL mutations (most of gh's writes).
  `/graphql` is parsed strictly (one operation, no batching, a size cap,
  fragments expanded); each top-level mutation field maps to its scope
  (`createPullRequest` → `pr.write`), and the claim's `fence` names
  every input naming a repo or object (`createPullRequest.repositoryId`
  and `headRepositoryId`, `transferIssue.issueId` and `repositoryId`,
  `createLinkedBranch.issueId` and `repositoryId`,
  `mergePullRequest.pullRequestId`, `addComment.subjectId`,
  `updateRef.refId`, `createCommitOnBranch.branch.repositoryNameWithOwner`;
  variables are JSON, literals sit in the document); all must resolve
  inside the fence, as must a REST body's target (`organization` on a
  fork, `owner` on a template's `generate`). Repository ids match the
  recorded node ids; the home resolves other global ids (`node(id:)`,
  a repository's cached, any other object's re-resolved on each write, as
  a transferred issue keeps its id) with the fence's installation tokens in turn,
  or the user token. A resolved mutation is fenced and asked like a REST
  route, under the installation of the repo it resolves to (a fork's
  `createPullRequest` to the upstream takes the upstream's); one that
  isn't in the table or doesn't resolve is `other`, denied here. A test
  walks GitHub's GraphQL schema, nested input objects included, and fails
  on a mapped mutation input typed `ID`, `*RepositoryId`/`ownerId`,
  `*NameWithOwner` or `owner` + `name` that the `fence` doesn't name. Aliases
  rename only the response key and variables can't name a field, so a
  mutation can't hide. The ref-moving ones (`createCommitOnBranch`,
  `updateRef`, `deleteRef`, merges) are ref writes for the gateway's
  branch rules.
- **Routes it can't**: GraphQL queries (any repo, objects by global id),
  `/search/*`, `/notifications`, `/user/repos`, gists. gh's own queries
  name their repo in variables (`IssueList`, `RepositoryInfo`: `owner`
  and `name`), but `node(id:)`, `search` and `viewer.repositories` don't,
  and a query may ask any of them (open questions). Only a token
  GitHub narrows fences their repo, so their reads (GET, HEAD, GraphQL
  queries) pass under the token's own reach and never `ask` (an `ask`
  scope denies here); their writes (`POST /user/repos`, `POST /gists`)
  are `other`, denied: the broker can't
  name the repo, so an answer could cross the fence. An installation
  token minted for the sandbox's repos makes that reach the fence. A
  read-only allowlist gh needs: the GraphQL `viewer{login}` query and
  `GET /` (`gh auth status` sends the query first and calls the token
  invalid when it fails, then reads the API root's `X-Oauth-Scopes`,
  showing none when that fails),
  `/rate_limit`, `/meta`, and `/user` under a user token (an installation
  token gets 403 there, so `@me` fails under one).
- **`never`**: besides routes that mint a credential, those that plant
  one or a standing access path a sandbox could keep past `rm`: deploy
  keys (`POST /repos/{repo}/keys`), `POST /user/keys` and
  `/user/gpg_keys`, webhooks (`/repos/{repo}/hooks`, `/orgs/{org}/hooks`),
  `PUT /repos/{repo}/collaborators/{user}`, runner registration tokens.
  Listed in the claim, checked by a test.
- **`presence`**: tags and ref deletions, whichever route moves them:
  REST `git/refs`, a release created or `PATCH`ed with `draft: false`,
  GraphQL `createRef`/`updateRef`/`deleteRef`, and the gateway's
  receive-pack commands. The home resolves the target to its ref (a
  release id to its `tag_name`, a mutation's `refId` or `name`), so gh's
  draft-then-publish is one ask.
- **git** takes the intercept on a proxy port of its own, where devsbd
  claims the hosts the `git` field names (`github.com`): the gateway's
  scoped `http.https://github.com/.proxy` points there (git-lfs reads it
  too), and its rule table applies ([git-gateway](git-gateway.md)). A
  `CONNECT github.com` on the shared port is dialed directly, as for any
  unclaimed host. git's fence comes with `features/gh`: a sandbox without
  it keeps today's git and agent. The claim's `git` rewrites only the
  ssh forms (`git@github.com:`, `ssh://git@github.com/`) to
  `https://github.com/`, so remotes stay github.com URLs: gh finds its
  repo in `git remote -v`, which already applies `insteadOf`, and drops
  remotes on hosts it doesn't know, so a loopback URL would fail every gh
  command that takes its repo from the cwd. A request without a phantom
  goes upstream uninjected (credential-broker, *Phantoms*), so git sends
  it after GitHub's 401 through a helper: the `git` field resets
  `credential.https://github.com.helper` (empty) and then sets devsbd's
  (`x-access-token` and the phantom), in command scope
  (`GIT_CONFIG_COUNT`/`KEY_n`/`VALUE_n` on the container env, read last),
  so the user's `store`/`cache` helpers never get the phantom on
  `approve` (`~/.git-credentials` is often a host bind), beside the
  gateway's scoped proxy and CA settings. The helper always answers and
  the broker turns an out-of-fence 401 into a 403, so git never prompts
  for github.com. The helper gh passes on its own argv
  (`gh repo clone`, `gh pr checkout`) hands over the same phantom. Submodules on GitHub take the same path: inside the
  fence with the credential, outside it anonymous.
- **Redirects**: tarball, release asset and artifact downloads 302 to
  `codeload.github.com` / `release-assets.githubusercontent.com` (LFS
  objects come from `github-cloud.githubusercontent.com` or S3: the
  gateway's *LFS*) with a short-lived signed URL. Those hosts are
  unclaimed and `Location` passes through: the signature is in the URL,
  no header to carry.

### Whose token: the bot or you

They differ in who GitHub thinks acted, and in what GitHub itself fences:

| token | acts as | reach GitHub enforces | lives | delegable |
|---|---|---|---|---|
| App installation token | the app (`yourapp[bot]` authors the PR) | the instance's repos and permissions, at mint | 1 h, fixed | yes |
| App user token | you | the app's installed repos you can access | 8 h, refresh token 6 months | no: it isn't narrow |
| fine-grained PAT | you | the repos it selects, one owner | until the expiry you set | no |
| classic PAT, gh's OAuth token | you | every repo you can access | until revoked | not accepted |

Under a user token GraphQL reads any repo in its reach whatever the fence
says, so keep the installation or the PAT down to what sandboxes may
touch; classic tokens (`repo` scope: every private repo) aren't taken.
The home says which kinds a credential offers (`as = ["app", "user"]` in
`credentials.toml`); a config's `access.github.as` picks within it
(`user` by default when offered, else `app`: the babysit dispatcher's
children push and reply as you; `as = "app"` narrows), and a kind the home doesn't offer is refused at `run`:
a user token reaches more, and a config only narrows. An installation
token is minted with `repository_ids` for the fence ∩ that installation's
repos (GitHub refuses the whole mint if one listed repo is outside it)
and `permissions` from the scopes the sandbox is granted or may be asked
(the broker gates the asked ones, so an approved ask meets no 403;
`workflows` only with `git.push.workflows`, git-gateway), so
GitHub enforces them too. GitHub refuses the whole mint (422) if one
named permission isn't granted to the installation (an owner accepted
fewer, an app update still pending acceptance, a new claim version
needing one the app lacks), so the mint asks for that ∩ the
installation's `permissions` (`GET /app/installations/{id}`, cached,
re-read on 422); a call in a scope left out gets the broker's 403
naming the missing permission, `run` and `access ls` report the gap per
installation, and `access approve` names a permission the app lacks,
with its settings URL. A mint never omits `repository_ids` (that reaches
every repo of the installation): an empty intersection mints nothing and
that owner's requests go anonymous. A sandbox whose repos span two
installations (your fork, the org's upstream) gets one token each on
attributable routes; unattributable ones go under the installation of
the `folder`'s `origin`, else of the fence's first repo with one, so from the fork a query about a private
upstream (`gh pr create` looks it up first) fails under `as = "app"`. One rate-limit
bucket per installation, shared by every sandbox and dispatcher poll.

`devsandbox login github --app`: the CLI serves a page that posts the
manifest to GitHub with a random `state`, GitHub redirects to its
loopback listener with a `code` and that `state` (any other is
refused), and the code is exchanged within the hour (`POST
/app-manifests/{code}/conversions`) for the app id and key. The CLI
shows the returned app's `owner`, `slug` and `html_url` and stores it at
home only once confirmed (always for a pasted `--code`), refusing an
owner other than the account or org named: a code from someone else's
manifest would hand their app the user's installs. The manifest requests only the
permissions some scope maps to, never `administration`, `members` or
organization hooks, so a wrong scope→permission entry can't hand them
out. The exchange runs on the home, so the key never leaves it. A
headless home has no browser: on an approver, `login github --app` does
the browser half and sends the single-use `code` over
`credentials.login` (credential-broker, *Who's asking*); on any other
machine `--code-only` prints it, pasted into `devsandbox login github
--app --code <code>` on the home. Then the app is
installed on each owner, with its repos. A private app installs only on
the account that owns it, so for repos spanning owners it's made public
(each org owner installs it) or created under the org. `run` warns of a
fenced repo's owner with no installation ("app not installed on acme"),
only a note when the repo is public (a reference clone in `folders`).

## sentry-cli

- **Reaching the broker**: base-URL mode. `SENTRY_URL` points at the
  loopback port (plain http, no warning), `SENTRY_AUTH_TOKEN` and
  `~/.sentryclirc` hold the phantom; the rc template carries `[defaults]
  url` beside the token, as sentry-cli 3.x pairs URL and token by source
  (`reconcile_auth_url`: a source setting only one drops the other's
  inherited value), so a token-only rc is dropped once `SENTRY_URL` is
  set, and anything that drops `SENTRY_URL` but passes a token
  (`env -i`, a CI wrapper) sends it to `sentry.io` (a 401 for a phantom).
  No CA. sentry-cli takes any token shape (an unknown one warned about by
  3.x only for `--auth-token`, by 2.x from env and rc too) and keeps
  `SENTRY_URL` unless an `sntrys_` token embeds a URL: the phantom is an
  unstructured `sntrys_…` (or a structured one embedding the loopback
  URL), never a copy of the real token's structure, which goes to
  `sentry.io` (3.x) or errors (2.x env). Both redact the header in their
  logs; 2.x logs a `--auth-token` argv in full: harmless.
- **Chunk uploads**: `GET /api/0/organizations/{org}/chunk-upload/`
  answers an absolute region URL (`us.sentry.io`) or a relative one, by
  the server's rule: Sentry's `ChunkUploadEndpoint.get`
  (`src/sentry/api/endpoints/chunk.py`, `requires_region_url`,
  `generate_locality_url`) reads the client's User-Agent and sends
  sentry-cli 2.30 and later the absolute one, and a self-hosted server
  with `system.upload-url-prefix` or
  `hybrid_cloud.disable_relative_upload_urls` sends every version one.
  Every client version posts the chunks to whatever absolute URL it gets,
  token attached. The claim's `rewrite` names that `url`, whatever the
  client version: the broker returns that upstream's loopback port in its
  place and sends those posts to the host the server named; a host
  outside the claim's upstreams is refused. Without it every source map
  and debug-file upload gets a 401. It names `regions[].url` of `GET
  /api/0/users/me/regions/` too, which 2.x `organizations list` follows
  to each region.
- **Scopes**: routes under `/api/0/organizations/{org}/…` and
  `/api/0/projects/{org}/{project}/…`, plus `GET /api/0/` (sentry-cli's
  token check) and `GET /api/0/organizations/`; the fence is the org.
  Projects aren't one: project routes take a numeric id as well as a
  slug, and org-level issue and event routes cross projects. Routes with no org in the
  path (`/api/0/issues/{id}/…`) can't be attributed: the org-bound token
  fences them, and the scope table lists them (`issues.read`), so they
  aren't `other`.
- **Credential**: an internal-integration token: org-bound, permissions
  chosen (releases, issue and event read). An org auth token (`sntrys_`)
  only ever holds `org:ci` (releases, source maps, code mappings): enough
  for uploads, 403 on issues and on the `GET /api/0/organizations/{org}/`
  lookup. A user token spans orgs. `never` lists the routes that mint a
  credential (`…/org-auth-tokens/`, `sentry-apps/{slug}/api-tokens/`,
  project client keys), checked by the same test as gh's. No minting, so never delegated: a
  sandbox on another machine is always brokered from home.
- **Profiles**: one credential per org (`sentry-work`), granted to the
  profiles that may use it. The claim names the kind (`sentry`); an
  instance takes the one granted to its profile, or the one
  `access.sentry.credential` names. One org per instance:
  `SENTRY_AUTH_TOKEN` is one variable.

## Ideas weighed

| question | option | U | N | verdict |
|---|---|---|---|---|
| how gh reaches the token | CLI shim + argv policy (the old CLI-shim plan) | 5 | 3 | no: misses git, SDKs, `gh api` |
| | the broker, intercept mode | 9 | 6 | chosen |
| git to github.com | `insteadOf` to the loopback | 6 | 4 | no: `git remote -v` shows the loopback, gh finds no repo |
| | the intercept, ssh forms rewritten to https | 8 | 6 | chosen |
| GraphQL fence | parse the document | 7 | 5 | chosen for mutations (input ids resolved by the home); queries can't be fenced in general (gh's own name `owner`/`name`: open questions) |
| | a token minted for the repos | 9 | 6 | chosen where `as = "app"` |
| | deny `/graphql` | 3 | 2 | breaks most of gh |
| claim location | devsandbox's own table of known tools | 6 | 2 | every tool a devsandbox release |
| | in the feature's metadata | 9 | 7 | chosen: features ship with their tool, the home approves a copy |
| sentry chunk uploads | rewrite the upload URL the server returns to the loopback | 6 | 5 | chosen: sentry-cli posts to any absolute URL it gets, past `SENTRY_URL` |

## Rollout

1. The claim contract read from feature metadata, the home's copy at
   `login` under the claim's version, `access approve`; phantoms (files from templates), base-URL wiring and
   `rewrite`. `features/sentry` with an internal-integration token.
   A claim applies to an instance only where it can be brokered: locally,
   and on placed or remote instances once credential-broker step 5
   carries `CRED`; until then they keep today's mounts, so the live
   config drops its sentry and gh mounts only after that step.
   Built: all of it; `features/sentry` 1.0.0 is in the features repo and
   checked end to end with sentry-cli 3.8.2 against a mock Sentry
   ([broker](../../architecture/credentials/broker.md#end-to-end)).
   Deferred, each a [status](../../status.md#credentials) limitation:
   calling the claim's `check` route, a claim field for token and
   registration endpoints, stripping token-shaped response headers.
2. `features/gh` in intercept mode with your token (`as = "user"`):
   fine-grained PATs by paste and app user tokens (an existing app's
   device flow, credential-broker step 2), the home's `as` list, a
   warning ahead of a user credential's expiry (`access ls`, a desktop
   notice); `gh-update` and the launcher; the repo fence (recorded remotes, `access.github.repos`, `access forget`,
   ids, strict paths, anonymous outside); GraphQL mutations mapped to
   scopes and attributed by all their input ids; the read-only allowlist; the `never` routes; a
   scope table covering the gh it installs, checked by a test running
   gh's commands against it (`gh auth status` exits 0); git via the
   intercept, the command-scope helper and the gateway's rule table.
   While the gh mount stays, devsbd skips `hosts.yml` there
   (credential-broker, *Phantoms*); `GH_TOKEN` wins anyway.
   Live config: `template.base` swaps
   `ghcr.io/devcontainers/features/github-cli` for `features/gh`;
   `sandbox.devsandboxes` adds `stackblitz/bolt` (its
   dispatcher's repo) to `access.github.repos` and takes `as = "user"`
   (its children push to and reply on your PRs, and `gh pr list --author
   @me` means you). Its repos span `Nemikolh` and `stackblitz`, so that's
   an app user token from a public app a `stackblitz` owner installed (a
   fine-grained PAT has one owner). It runs privileged
   (docker-in-docker), which the broker counts as the host
   (credential-broker, its non-goals): the widest token sits where the
   home's store is in reach on a rootful runtime.
   Built: all of it but the live config; `features/gh` 1.3.1 is in the
   features repo, its claim mirrored by the github test fixture, its
   scope table checked against gh 2.105.0's calls (from its source) and
   GitHub's public GraphQL schema, and gh run end to end through the
   broker against a mock GitHub
   ([gh](../../architecture/credentials/gh.md)); its `device` login
   names no app, the home's comes with `login --client-id`; the feature
   mounts the shared bin dir (read-only in sandboxes whatever the mount
   says) and declares its `gh` update, run on the daemon's schedule or
   by `devsandbox update gh`. Deferred,
   each a [status](../../status.md#credentials) limitation: multi-segment route
   variables (paths are listed per depth), Projects, discussion
   comments by id, and `gh repo fork` (REST body inputs).
3. Parked: most users work in orgs where they can't install an app
   ([status](../../status.md#parked)). `devsandbox login github --app`
   (manifest and install), installation tokens per instance and
   installation (`as = "app"`, `repository_ids` and `permissions`).
4. Parked with step 3: installation tokens delegated to members.

## Open questions

- Out-of-fence public reads share 60 an hour per host IP. The fence's
  installation token can't reach private repos outside it
  (`repository_ids`): lend it to out-of-fence GETs, or keep the
  credential strictly inside the fence?
- gh extensions and `gh copilot`, `gh codespace`: other upstreams, out of
  the claim unless asked. Likewise GitHub's MCP server
  (`api.githubcopilot.com/mcp/`) and Sentry's: refused as `mcp:`
  (agent-credentials) and in no claim here; adding one means a scope
  table per tool, each tool's `owner`/`repo` attributed to the fence.
- Which token takes queries when a fence spans installations (the
  fork-to-upstream lookup above)?
- Fence gh's own GraphQL queries under `as = "user"` by their
  `repository(owner:, name:)` arguments and variables, leaving
  `node(id:)`, `search` and `viewer` to the token's reach? Most gh read
  commands would then stay inside the fence, but an outside query can't
  go anonymous (no anonymous GraphQL), so it would be denied: `gh repo
  view` of a public repo out of the fence fails.
- GitHub Enterprise and self-hosted Sentry: hosts no feature knows, and
  a config can't add upstreams. A home-side `upstreams` override in
  `credentials.toml`, one credential per host?
