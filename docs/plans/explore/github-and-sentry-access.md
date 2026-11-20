# GitHub and Sentry access

Status: explored, not agreed. Needs [credential-broker](credential-broker.md)
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
| `inject` | where the token goes: a header and its format, basic auth, a query parameter |
| `phantom` | env vars, and files from a template (`hosts.yml` is YAML, `.sentryclirc` INI) in the remote user's home; a prefix, if the client reads its mode from it |
| `git` | gitconfig devsandbox writes in the container (ssh forms to https, the credential helper; *git* below) |
| `baseUrl` | for `base-url`: the env var that takes the loopback URL |
| `rewrite` | response fields holding an absolute upstream URL, replaced with the loopback (sentry's chunk-upload `url`) |
| `login` | how `devsandbox login` gets it: OAuth device flow, browser OAuth, GitHub App manifest, paste |
| `check` | a route the broker calls with no sandbox input to confirm a 401 means a broken credential (`GET /user`, `GET /api/0/`; credential-broker, *Lifetimes*) |
| `mint` | an adapter devsandbox ships (`github-app`, `oauth-refresh`), if any |
| `scopes` | named scopes over routes and GraphQL mutations, with the app permissions each needs: `pr.write = ["POST /repos/{repo}/pulls", "graphql mutation createPullRequest", …]` |
| `never` | routes no ask or rule opens |
| `presence` | routes and ref forms that always ask a human, with the field naming each one's target (credential-broker, *`presence`*) |
| `hold` | how long an `ask` holds the call (credential-broker, *Permission requests*) |
| `refusal` | the upstream's error shape refusals take (credential-broker, *Refusals*) |
| `fence` | how a route or mutation names its repos or project: every repo- or object-naming input (`{repo}` from the path, a body's `owner`/`organization`, each mutation input holding a repository or node id) |
| `ca` | a CA knob devsandbox doesn't set by default, if the tool has one |

Data only: a claim can't run anything on the home or a member.

The home binds the secret to its destination. It pins the whole claim but
its container-side wiring (`phantom`, `baseUrl`, `ca`, `git`: the
container could ignore them anyway) in `credentials.toml`, under the
claim's version: a hash of that pinned part as `FeatureMetadata` reads
it, the same for an OCI or a local feature. Not the feature's digest:
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
  logged in; `hosts.yml` gets the phantom for tools that read it. `gh auth
  token` prints the phantom: harmless. `github.com` isn't claimed there
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
  ([agent-credentials](agent-credentials.md)).
- **Matching**: owner and repo compare case-insensitively, with `.git`
  and `.wiki.git` reduced to the repo. Owner, repo and
  `/repositories/{id}` are strict: an encoded character, a dot-segment or
  `//` there is refused, not normalized. Past the repo `%2F` is decoded
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
  `devsandbox-fence: outside` (credential-broker, *Policy*): public repos work (cargo and npm git dependencies,
  release downloads, `gh extension install`), private ones answer 404 as
  to a stranger. The fence keeps the credential, not the network (the
  broker's non-goal). Reads share the host IP's anonymous limit (60 an
  hour); a write gets the broker's refusal naming `access.github.repos`,
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
  `/search/*`, `/notifications`, `/user/repos`, gists. Only a token
  GitHub narrows fences their repo, so their reads (GET, HEAD, GraphQL
  queries) pass under the token's own reach and never `ask` (an `ask`
  scope denies here); their writes (`POST /user/repos`, `POST /gists`)
  are `other`, denied: the broker can't
  name the repo, so an answer could cross the fence. An installation
  token minted for the sandbox's repos makes that reach the fence. A
  read-only allowlist gh needs: `GET /` (the API root, whose
  `X-Oauth-Scopes` `gh auth status` reads, failing on anything but 200),
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
  `codeload.github.com` / `objects.githubusercontent.com` with a
  short-lived signed URL. Those hosts are unclaimed and `Location` passes
  through: the signature is in the URL, no header to carry.

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
  loopback port, `SENTRY_AUTH_TOKEN` and `~/.sentryclirc` hold the phantom.
  No CA. sentry-cli reads the env token unchecked (it warns about an
  unknown format only for `--auth-token`), keeps `SENTRY_URL` (only an
  `sntrys_` token's embedded URL overrides it), and leaves the phantom
  unredacted in its logs: harmless.
- **Chunk uploads**: sentry-cli 2.30 and later gets an absolute region URL
  from `GET /api/0/organizations/{org}/chunk-upload/` (Sentry's
  `ChunkUploadEndpoint`, `requires_region_url`) and posts the chunks there
  directly. The claim's `rewrite` names that `url`: the broker returns
  that upstream's loopback port in its place and sends those posts to the
  host the server named (`us.sentry.io`); a host outside the claim's
  upstreams is refused. Without it every source map and debug-file upload
  gets a 401.
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
| GraphQL fence | parse the document | 7 | 5 | chosen for mutations (input ids resolved by the home); queries can't be fenced |
| | a token minted for the repos | 9 | 6 | chosen where `as = "app"` |
| | deny `/graphql` | 3 | 2 | breaks most of gh |
| claim location | devsandbox's own table of known tools | 6 | 2 | every tool a devsandbox release |
| | in the feature's metadata | 9 | 7 | chosen: features ship with their tool, the home approves a copy |
| sentry chunk uploads | rewrite the upload URL the server returns to the loopback | 6 | 5 | chosen: sentry-cli ≥ 2.30 posts past `SENTRY_URL` |

## Rollout

1. The claim contract read from feature metadata, the home's copy at
   `login` under the claim's version, `access approve`; phantoms (files from templates), base-URL wiring and
   `rewrite`. `features/sentry` with an internal-integration token.
   A claim applies to an instance only where it can be brokered: locally,
   and on placed or remote instances once credential-broker step 5
   carries `CRED`; until then they keep today's mounts, so the live
   config drops its sentry and gh mounts only after that step.
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
3. `devsandbox login github --app` (manifest and install), installation
   tokens per instance and installation (`as = "app"`, `repository_ids`
   and `permissions`).
4. Installation tokens delegated to members.

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
- GitHub Enterprise and self-hosted Sentry: hosts no feature knows, and
  a config can't add upstreams. A home-side `upstreams` override in
  `credentials.toml`, one credential per host?
