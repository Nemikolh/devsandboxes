# Agent credentials

Status: explored, not agreed. zidane first, any coding agent after. Needs
[credential-broker](credential-broker.md) (base-URL and intercept modes,
permission requests, `CRED` over the fleet); its GitHub side is
[github-and-sentry-access](github-and-sentry-access.md). Design and
diagrams: [Credentials and access](../../../site/src/content/architecture/secrets.mdx),
*zidane and other agents*.

## Goal

- zidane is logged in on every machine with nothing in the sandbox: no
  model key, no OAuth tokens in `shared-volumes/zidane-config`
  (`credentials.json`, `mcp-credentials.json`).
- Whatever zidane does with other credentials (gh, git, sentry, MCP
  servers) is bounded by its instance, like anything else in the sandbox.
- When it hits the edge of its scope it can tell, wait, and carry on.

Today the live config shares `zidane-config` (credentials, `config.json`)
across every sandbox of a host through `${sharedVolumes}`; another machine
starts empty and needs its own login. The live login there is Anthropic
subscription OAuth.

## An agent is three things

### A tool with its own login

- **API key**: a static credential, brokered in base-URL mode (the
  provider's base URL env, a phantom key, both on the container env: zidane
  reads `ANTHROPIC_BASE_URL` and `ANTHROPIC_API_KEY`; env only, nothing
  in `credentials.json`). Never delegated: a
  sandbox on another machine reaches the home for every call, so the home
  for model keys is an always-on member, shown in `access ls`; a sandbox
  that can't reach it (a laptop off its network) has no model. The key
  opens workspace-wide routes (batches, files,
  skills) where one instance reads another's data, so the claim's scope
  table allows `POST /v1/messages`, `POST /v1/messages/count_tokens`,
  `GET /v1/models` and `GET /v1/models/{model}` and denies the rest (deny wins over any answer),
  matching the path without its query (the SDK adds `?beta=true`),
  strictly as the `mcp` URL below;
  `login` refuses an admin key. Allowed calls still name workspace
  objects by id (`file_id`, `container`, `skill_id`), so between
  instances of one profile the ids are the only barrier; a key per
  workspace, one credential each granted to its profile (the claim names
  the kind, as Sentry's per org: github-and-sentry-access, *Profiles*),
  keeps profiles apart.
- **Subscription OAuth**: the home keeps the refresh token and the broker swaps
  the phantom bearer for the current access token, refreshed at home. The
  access token isn't narrowed (it's the whole account), so it isn't
  delegated either.
- **The client's own refresh**: the broker answers the provider's token
  endpoint itself for a phantom, with the same phantom and a far expiry,
  so the agent's refresh logic is a no-op and nothing it stores is real.
  The token endpoint is its own host, which a base URL doesn't cover:
  intercept mode. The claim marks it and the registration endpoint, so
  they are never forwarded, phantom or not (credential-broker, *Policy*'s
  third step): every other grant (`authorization_code`) and every
  registration gets a refusal naming `devsandbox login <credential>` on
  the home, as does an upstream 401, so the agent never runs its own
  login in the sandbox. In API-key mode the claim marks them too and
  refuses every grant (broker step 3): a login in the sandbox would write real tokens
  into the config dir. Until then the
  config dir is per instance, so such a login stays in its instance.
- **Phantom shape**: zidane and Claude Code treat `sk-ant-oat…` as
  OAuth, so the claim declares the phantom's prefix per mode
  (credential-broker, *Phantoms*). A client that decodes its access
  token needs more: zidane's ChatGPT login and refresh read
  `chatgpt_account_id` from the JWT payload and fail without it, so an
  opaque phantom can't stand in there (the OpenAI open question).
- **Streaming**: responses (SSE) pass through unbuffered; a hop home adds
  to the first byte only. A stream cut mid-response (link down, home
  asleep, a home daemon's drain) ends with an error event in the
  provider's shape and its transient type (Anthropic's
  `overloaded_error`), never a hang. SDKs don't retry a stream that
  started, so retrying the turn is zidane's own loop (`retryMaxAttempts`),
  metered twice. A
  home upgrade cuts every machine's in-flight turns, and a helper upgrade
  its container's, until the broker keeps open streams across a handoff
  (its open question).
- **Metering**: the claim says where a provider's responses report usage;
  the broker counts per instance and profile against budgets and a model
  allowlist (per profile too, as children are new instances; a remote
  instance's id is its member's word, so at the home only budgets per
  member and profile bind it, the member naming any profile granted to
  it). Over budget
  is an `ask`, not a hard stop, of its own kind: one per instance or
  profile period (a dispatcher's children join it), answered only by a
  new cap, never by "always", a rule or signed policy, so nothing turns
  budgets off.

### A principal acting with everything else

- Its gh, git and sentry calls carry its instance's identity: the repo
  fence, the scopes, the profile's grants. zidane's own GitHub client
  reads `GH_TOKEN` first, so it carries gh's phantom through intercept
  mode like gh does. Under `as = "user"` its GraphQL calls (most of
  them) are bounded by the token's reach, not the fence
  (github-and-sentry-access).
- **Nothing shared writable that runs or steers**: an instance bounded
  by the broker still runs as every instance that executes code it can
  write. zidane's binary, skills and extensions come from its feature or
  read-only mounts (updated on the host); only per-instance state is
  writable. zidane's settings steer it too (`zidane-state/state.json`:
  approval mode, its own sandbox, project extensions, remote hosts), as
  does its MCP tool cache (`mcp-tools-cache.json`, tool descriptions the
  model reads): both per instance, in the zidane feature's next major
  (its mounts live there) as `${devcontainerId}` volumes, which `rm`
  deletes (instance ids are reused, so a kept bind would hand a login or
  steered settings to the next instance of that name), seeded from the
  host's copy when the volume is created, with no `account` selection kept: zidane reads a stored account before
  `ANTHROPIC_API_KEY`. The cache sits in zidane's config dir beside
  `mcp-credentials.json`, which stays shared until step 5, and zidane
  saves both by rename (a single-file bind refuses it, a symlink is
  replaced), so a per-instance config dir needs zidane to take the MCP
  credentials path from an env (like `ZIDANE_CREDENTIALS_PATH`) in that
  same major. `bin/current` is read-only with the binary, so the feature sets
  `ZIDANE_NO_UPDATE=1`.
- **No config root in a sandbox**, even read-only (`run` refuses one:
  credential-broker, *What a sandbox mounts*). Writable, its config
  is the fence, `access.*`, mounts and MCP URLs, and a dispatcher's
  `ensure` reads it as is; readable, it holds every instance's volumes
  (`shared-volumes/zidane/<instance>/sessions.db` transcripts, `gh`,
  `sentry`, `mcp-auth`) and other repos' `.worktrees/`. A sandbox that
  needs the config gets its files (`devsandboxes.toml`, `mcps/`,
  `dockerfiles/`) read-only, one bind each; a single-file bind pins the
  inode, so a host edit saved by rename shows after a restart.
- **Your checkout is outside this**: the first instance binds it rw,
  `.git/` included, so a steered agent can plant what the host then runs
  as you, every secret in reach (`core.fsmonitor`, hooks, `filter.*`,
  `.envrc`, a `.zidane/` extension if the host's zidane has
  `loadProjectExtensions` or `ZIDANE_TRUST_PROJECT_EXTENSIONS=1` on). `host_git` guards devsandbox's own git,
  not yours; git-gateway's `checkout = "clone"` avoids the bind, but a
  clone's `.git` is just as sandbox-written: review its work through
  git-gateway's local remote, not with host git or an editor in the clone.
- **No secret asked in the sandbox**: zidane's `ask_user` has a `secret`
  question (a password field, the answer a file under `$ZIDANE_SECRETS`
  passed to its commands), the natural reaction to a refusal. The
  feature turns zidane's `secrets` setting off (a nudge: the state file
  is writable in the sandbox; the guard is that no secret is in reach),
  refusals name what to do
  instead (`devsandbox login <credential>` on the home, a claim, a
  config grant), and the skill says never to ask for a secret. zidane's
  remote environments are unavailable from a sandbox: they'd carry only
  the phantom off the instance, and their `gitToken` field is another
  `secret` prompt.
- **Dispatcher children** inherit the dispatcher's profile
  ([profiles](../../architecture/instances/profiles.md)) but have their
  own sandbox's fence and `access.*`, and a dispatcher runs commands in
  its children (`exec`, `run-*`,
  [dispatchers](../../architecture/automations/dispatchers.md)). So a
  dispatcher's reach is the union over its `spawn` list: `run` shows it,
  and `spawn = "*"` means every sandbox's. Their GitHub `as` default is
  github-and-sentry-access's open question (`app` needs its minting step).
- **`presence` routes** (pushing a release, deleting a branch, publishing
  a package) ask a human even where the scope allows them, answered only
  once or for this instance on that one target, never by a rule, signed
  policy or a container (credential-broker, *`presence`*). Unattended,
  they wait out the hold and are refused. Git pushes too, as a tag push
  is a release where CI publishes on tags: the gateway holds a push to a
  `presence` ref form ([git-gateway](git-gateway.md)); gh's routes are
  github-and-sentry-access's.
- **MCP servers over HTTP**: MCP fixes discovery, OAuth and JSON-RPC, so
  devsandbox carries one generic `mcp` claim, per server the config
  grants (`access."mcp:linear"`). Its URL (host and path prefix, the
  OAuth `resource`) is bound at `devsandbox login mcp:<server>`, a config
  naming another is refused, and other paths on that host go upstream
  uninjected. Only the home reads it (`credentials.toml`), so it hands
  the binding (host, prefix, the marked endpoints below) over `CRED` at
  `run` to the instance's host, which intercepts, recognizes the route
  and guards those endpoints from its broker file, and folds it into the
  signed `claim` digest; the home refuses a call whose binding isn't its
  login's. A remote home offline at `run` leaves the server unrouted (a
  warning); a re-login that changes the binding reaches an instance at
  its next `rebuild`. The prefix matches whole segments (`/mcp` not
  `/mcp-admin`), and a path is refused, not normalized, unless its
  segments hold only unreserved characters, sub-delims but `;`, `:`,
  `@` and `%XX` of printable ASCII other than `.`, `/`, `\`, `;`, `%`,
  with no empty, `.` or `..` segment: upstream edges resolve
  dot-segments, decode (some twice) and strip `;` parameters (Linear's
  Cloudflare serves `/mcp/%2e%2e/x` as `/x`; `..%3Bx` turns `..`), which would
  carry the token past the prefix. A server fronting a fenced credential (GitHub's
  `api.githubcopilot.com/mcp/`, whose tools take `owner`/`repo`
  arguments; Sentry's) is refused as `mcp:`: it comes only through that
  tool's feature claim, whose scope table attributes each tool's repo or
  project to the fence and marks ref-moving tools `presence`. Intercept mode: the MCP files (`mcps/*.json`) name the real
  URLs and are shared. OAuth done at home (`devsandbox login
  mcp:<server>`), replacing zidane's `mcp-credentials.json` and the
  `mcp-auth` volume. MCP auth starts on a 401, so on `mcp` routes the
  bridge's instance identity is enough: the broker injects with or
  without a bearer (stripping any). The AS's token and registration
  endpoints, from the server's metadata at login (Linear's `/token` and
  `/register`, on the bound host outside the prefix), are marked like a
  provider's, their hosts intercepted. On `mcp` routes the broker strips
  upstream `WWW-Authenticate` and turns a `401` or a `403
  insufficient_scope` (MCP's step-up) into a refusal naming `devsandbox
  login mcp:<server>` with the challenged scope, so step-up happens at
  home. The client never sees a challenge, never
  runs discovery or registration, and stores nothing in a shared file
  (zidane's `mcp-credentials.json`, `mcp-remote`'s `~/.mcp-auth`); a Node
  client such as `mcp-remote` needs Node 22.21+ (credential-broker's
  proxy env), as an older one reaches the server directly and gets the
  401. The broker reads `tools/call` bodies, so a server's
  tools get scopes of their own (allow `search`, ask `create_page`);
  `resources/read` and `prompts/get` are scoped the same way, the other
  methods (`initialize`, listing, `ping`, notifications, client
  responses) pass.
- **Scoped bodies** (`model`, `tools/call`) are parsed strictly
  (credential-broker, *The broker*: duplicate keys and JSON-RPC batches
  refused); a model body, up to Anthropic's 128 MB, is read by a
  streaming scan of top-level keys while it's forwarded. Above the
  claim's declared limit, the provider's own `413`.

### A client of permission requests

The agent is the one most likely to hit an `ask` or a deny, and must not
read it as a crash.

- **Held calls**: an `ask` holds the request (the claim's `hold`, set
  below the client's own timeout: MCP clients give up at 60 s); most
  answers arrive within it, from you or an approver, and the agent never
  notices. A retry joins the pending ask, and a late "once" is a one-shot
  grant for the next matching call (credential-broker, *Permission
  requests*).
- **A structured refusal** when it doesn't, in the upstream's own shape
  (credential-broker, *Refusals*; for MCP's `tools/call` a result the
  model reads, where a `403` is a transport error), as that's all the
  agent sees of it: a one-line message naming `devsbd access wait
  <request>` or what to do instead, plus the object. The same shape
  covers a pending or denied ask, a claim version not approved (`access
  approve`), a missing grant and a broken home credential (`devsandbox
  login`). An out-of-fence read is no refusal: it gets the upstream's
  anonymous answer (a private repo's `404`) and a `devsandbox-fence: outside` header (credential-broker,
  *Policy*), so an agent can tell a fence from a typo.

  ```json
  { "devsandbox": { "verdict": "pending", "request": "r_5f0c9a3e1b7d",
                    "credential": "github", "scope": "issues.write",
                    "retry_after": 30, "why": "issues.write asks you for web" } }
  ```

- **Waiting on purpose**: `devsbd access wait <request>`, a new `control`
  op over `api.sock`, blocks until the answer (the client loops over
  waits capped at `control::MAX_WAIT`), so the agent can park a step and
  come back; `approved` means the call will pass now. It answers only for
  requests its own instance raised (request ids are unguessable), needs
  neither `dispatcher` nor `inbox`, and the instance's host relays it to
  a remote home.
- **Model routes**: a held model call has no model to read its refusal,
  so a pending one is answered in the provider's retryable shape
  (Anthropic's `529 overloaded_error` with `Retry-After`, the object in
  the body): an SDK's retries join the pending ask, and fail the turn
  once they run out.
- **zidane's side**, in its client code, not a skill: recognize the
  object, show it in its own UI, wait (on a model route: pause the turn,
  resume on `devsbd access wait`) or pick another path. A skill teaches
  other agents the same for tool calls.

## Ideas weighed

| idea | from | U | N | verdict |
|---|---|---|---|---|
| broker the model API in base-URL mode | API gateways | 9 | 5 | chosen |
| answer the OAuth refresh endpoint with phantoms | token exchange (RFC 8693), Fly's tokenizer | 8 | 8 | chosen: no agent change for refresh |
| per-tool MCP scopes from JSON-RPC bodies | the gateway's receive-pack parsing | 8 | 8 | chosen once MCP is brokered |
| budgets per instance | cloud billing alerts | 7 | 5 | ask over budget |
| a human for `presence` routes | two-person rule, sudo | 8 | 7 | chosen |
| run the agent outside the sandbox, only tools inside | split-brain agents | 6 | 8 | no: the agent needs the sandbox's files and tools; its credentials are the problem, not its location |
| exchange a JWT the home signs for short-lived model tokens | Anthropic's workload identity federation (zidane reads `ANTHROPIC_IDENTITY_TOKEN_FILE`) | 7 | 6 | later: no static key at home; the home is the issuer (inline JWKS, so Anthropic needn't reach it) and swaps the phantom for the token, never federation in the sandbox |
| give the agent its own identity apart from the instance | SPIFFE per process | 5 | 7 | later: the bridge can't tell processes apart in a container; until then any process there (a postinstall script, a test) spends the agent's grants, bounded by budgets |

## Rollout

Broker steps in brackets.

1. zidane's model credential brokered (API key, base-URL mode) through
   its feature's claim [1: each host its own home, logged in with its own
   key; 5: one home, the other hosts' keys unused and revoked]; the live
   config moves zidane to an API key on env; OpenAI unavailable in
   sandboxes until brokered (the skill says so; until [3] a sign-in there
   is outside the guarantee, its refresh token kept in the instance's
   volume; from [3] zidane's claim also marks `auth.openai.com`'s token
   endpoints, `/oauth/token` and `/api/accounts/oauth/token`, guard only,
   refusing every grant); zidane's config and state dirs per
   instance (no shared `credentials.json`, settings or MCP tool cache;
   `mcp-credentials.json` stays one shared file until step 5, through
   zidane's env for its path, as Linear's and Notion's refresh tokens
   rotate and copies would revoke each other),
   its binary, skills and extensions read-only, `ZIDANE_NO_UPDATE=1`,
   `secret` questions off; `sandbox.devsandboxes` gets the config files
   read-only instead of the `.devsandboxes` folder, so the PR dispatcher
   it runs from there moves first (code in a folder of its own,
   `dispatcher.config.mts` one more read-only bind, `dispatch.log` and
   `.leader` in a volume of their own); that sandbox runs `privileged`
   for docker-in-docker, which credential-broker counts as the host, so
   its zidane stays outside the guarantee; the old OAuth tokens
   revoked and deleted from `shared-volumes` on every host, the
   per-instance `zidane/<instance>/` copies too (`credentials.json`, its
   `.tmp` leftovers, `mcp-credentials.json`: those binds outlive `rm`,
   and a stored account wins over `ANTHROPIC_API_KEY`).
2. Subscription OAuth: login at home,
   phantom refresh, access tokens swapped at home [2, 3].
3. Structured refusals, `devsbd access wait`, zidane handling them [4].
4. Metering, budgets, model allowlist per profile [4].
5. The `mcp` claim, MCP OAuth at home, bearer-less injection, per-tool
   scopes [3]; then `mcp-credentials.json` and the `mcp-auth` volume
   dropped, their tokens revoked.
6. Other agents: one feature per instance claims a credential and a
   sandbox often runs two agents, so the provider claims (`anthropic`,
   …) move into devsandbox as `mcp`'s is, an agent feature naming the
   one it uses with its env and phantom prefix.

## Open questions

- OpenAI, used live through ChatGPT-plan OAuth (`auth.openai.com`,
  `chatgpt.com/backend-api`): OpenAI's "Sign in with ChatGPT" now lets
  open-source clients spend the plan on the public Responses API
  (`api.openai.com/v1`, base URL `OPENAI_BASE_URL`) and steers them off
  `backend-api`; open is whether zidane moves to it and what its access
  token carries, i.e. a phantom shaped like it (a JWT copying the real
  one's non-secret claims) or a client that stops decoding it; then any
  other provider zidane uses.
- Per-instance settings drift from each other: should zidane read its
  settings from a read-only host file apart from its writable state
  (`state.json` mixes both today)?
