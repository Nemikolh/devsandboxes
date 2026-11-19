# Agent credentials

Status: explored, not agreed. zidane first, any coding agent after. Needs
[credential-broker](credential-broker.md) (base-URL mode, permission
requests, `CRED` over the fleet); its GitHub side is
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
  matching the path without its query (the SDK adds `?beta=true`);
  `login` refuses an admin key. Allowed calls still name workspace
  objects by id (`file_id`, `container`, `skill_id`), so between
  instances of one profile the ids are the only barrier; a workspace per
  profile keeps profiles apart.
- **Subscription OAuth**: the home keeps the refresh token and the broker swaps
  the phantom bearer for the current access token, refreshed at home. The
  access token isn't narrowed (it's the whole account), so it isn't
  delegated either.
- **The client's own refresh**: the broker answers the provider's token
  endpoint itself for a phantom, with the same phantom and a far expiry,
  so the agent's refresh logic is a no-op and nothing it stores is real.
  The token endpoint is its own host, which a base URL doesn't cover:
  intercept mode. There the broker refuses every other grant
  (`authorization_code`, dynamic client registration) with a refusal
  naming `devsandbox login <credential>` on the home, and turns an
  upstream 401 into the same refusal, so the agent never runs its own
  login in the sandbox. In API-key mode the claim intercepts the token
  endpoint too and refuses every grant (broker step 3): a login in the
  sandbox would write real tokens into the config dir. Until then the
  config dir is per instance, so such a login stays in its instance.
- **Phantom shape**: a client that picks its auth mode from the key's
  prefix (zidane and Claude Code treat `sk-ant-oat…` as OAuth) gets a
  phantom with the prefix the claim declares; the broker knows phantoms by
  its table, not by their prefix.
- **Streaming**: responses (SSE) pass through unbuffered; a hop home adds
  to the first byte only. A stream cut mid-response (link down, home
  asleep, a home daemon's drain) ends with an error event in the
  provider's shape and its transient type (Anthropic's
  `overloaded_error`), never a hang. SDKs don't retry a stream that
  started, so retrying the turn is zidane's own loop (`retryMaxAttempts`),
  metered twice. A
  home upgrade cuts every machine's in-flight turns until the broker's
  drain keeps open streams (its open question).
- **Metering**: the claim says where a provider's responses report usage;
  the broker counts per instance and profile against budgets and a model
  allowlist (per profile too, as children are new instances). Over budget
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
  model reads): both per instance, seeded at create from the host's
  copy by the zidane feature's next major (its mounts live there), with
  no `account` selection kept: zidane reads a stored account before
  `ANTHROPIC_API_KEY`. `bin/current` is read-only with the binary, so the feature sets
  `ZIDANE_NO_UPDATE=1`.
- **No config root in a sandbox**, even read-only. Writable, its config
  is the fence, `access.*`, mounts and MCP URLs, and a dispatcher's
  `ensure` reads it as is; readable, it holds every instance's volumes
  (`shared-volumes/zidane/<instance>/sessions.db` transcripts, `gh`,
  `sentry`, `mcp-auth`) and other repos' `.worktrees/`. A sandbox that
  needs the config gets its files (`devsandboxes.toml`, `mcps/`,
  `dockerfiles/`) read-only, one bind each; a single-file bind pins the
  inode, so a host edit saved by rename shows after a restart.
- **No secret asked in the sandbox**: zidane's `ask_user` has a `secret`
  question (a password field, the answer a file under `$ZIDANE_SECRETS`
  passed to its commands), the natural reaction to a refusal. The
  feature turns zidane's `secrets` setting off (a nudge: the state file
  is writable in the sandbox; the guard is that no secret is in reach),
  refusals name what to do
  instead (`devsandbox login <credential>` on the home, a claim, a
  config grant), and the skill says never to ask for a secret.
- **Dispatcher children** inherit the dispatcher's profile
  ([profiles](../../architecture/instances/profiles.md)) but have their
  own sandbox's fence and `access.*`, and a dispatcher runs commands in
  its children (`exec`, `run-*`,
  [dispatchers](../../architecture/automations/dispatchers.md)). So a
  dispatcher's reach is the union over its `spawn` list: `run` shows it,
  and `spawn = "*"` means every sandbox's. Their GitHub `as` default is
  github-and-sentry-access's open question (`app` needs its minting step).
- **`presence` routes** (pushing a release, deleting a branch, publishing
  a package) are a property of a route in a feature's scope table, not a
  scope a config grants: always asked, answered only once or for this
  instance on that exact route (one tag, one branch), never by a
  remembered rule or signed policy, never from a container. Unattended,
  they wait out the hold and are refused. Git pushes too (a tag push is a
  release where CI publishes on tags): the claim's `presence` names ref
  forms (`refs/tags/**`, deletions), which the gateway matches in
  receive-pack's parsed commands, holding the push before forwarding
  and answering `ng` in `report-status` when the hold runs out
  ([git-gateway](git-gateway.md)). A `presence` ask and its answer key
  on the target too, the ref the claim resolves it to (a pushed ref, a
  release's tag, a ref mutation's ref; release and node ids resolved by
  the home, so gh's draft-then-publish is one ask): another target is
  another ask.
- **MCP servers over HTTP**: MCP fixes discovery, OAuth and JSON-RPC, so
  devsandbox carries one generic `mcp` claim, per server URL the config
  grants (host and path prefix: other paths on that host go upstream
  uninjected). A server fronting a fenced credential (GitHub's
  `api.githubcopilot.com/mcp/`, whose tools take `owner`/`repo`
  arguments; Sentry's) is refused as `mcp:`: it comes only through that
  tool's feature claim, whose scope table attributes each tool's repo or
  project to the fence and marks ref-moving tools `presence`. Intercept mode: the MCP files (`mcps/*.json`) name the real
  URLs and are shared. OAuth done at home (`devsandbox login
  mcp:<server>`), replacing zidane's `mcp-credentials.json` and the
  `mcp-auth` volume. MCP auth starts on a 401, so on `mcp` routes the
  bridge's instance identity is enough: the broker injects with or
  without a bearer (stripping any), the client never sees a 401, never
  runs discovery or registration, and stores nothing in a shared file
  (zidane's `mcp-credentials.json`, `mcp-remote`'s `~/.mcp-auth`). The
  broker reads `tools/call` bodies, so a server's
  tools get scopes of their own (allow `search`, ask `create_page`).
- **Scoped bodies** (`model`, `tools/call`) are parsed strictly:
  duplicate keys, JSON-RPC batches and encoded request bodies refused on
  those routes. Model bodies run to the upstream's limit (Anthropic's
  128 MB), so `model` is read by a streaming scan of top-level keys while
  the body is forwarded, a violation aborting the upstream request before
  its body ends; above the claim's declared limit, the provider's own
  `413`.

### A client of permission requests

The agent is the one most likely to hit an `ask` or a deny, and must not
read it as a crash.

- **Held calls**: an `ask` holds the request (the claim's `hold`, set
  below the client's own timeout: MCP clients give up at 60 s); most
  answers arrive within it and the agent never notices. Asks dedupe per
  instance, credential and scope (for `presence`, method, route and
  target); a retry of a pending request joins it. An answer after the
  hold has no call left to release, so "once" becomes a one-shot grant
  for that instance's next matching call (same scope; for `presence`,
  same method, route and target) within a few minutes.
- **A structured refusal** when it doesn't, in the upstream's own error
  shape (the claim declares it: GitHub's `message`, Anthropic's `error`,
  git's `text/plain`; for MCP's `tools/call` a `200` result with
  `isError: true`, which the model reads, where a `403` is a transport
  error; never `WWW-Authenticate`, which starts the client's auth flow),
  as that's all the agent sees
  of it: a one-line message naming `devsbd access wait <request>`, plus
  the object. `403` for pending and deny (no client auto-retries it),
  `503` with `Retry-After` only for a home that's unreachable. An
  out-of-fence repo is no refusal (it goes upstream as a stranger and
  gets the upstream's `404`), but the response carries a
  `devsandbox-fence: outside` header so an agent can tell a fence from a
  typo.

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
   sandboxes until brokered (the skill says so; a sign-in there stays in
   its instance until the guard covers `auth.openai.com` at [3]); zidane's config and state dirs per
   instance (no shared `credentials.json`, settings or MCP tool cache;
   `mcp-credentials.json` stays one shared file until step 5, as
   Linear's and Notion's refresh tokens rotate and copies would revoke
   each other),
   its binary, skills and extensions read-only, `ZIDANE_NO_UPDATE=1`,
   `secret` questions off; `sandbox.devsandboxes` gets the config files
   read-only instead of the `.devsandboxes` folder; the old OAuth tokens
   revoked and deleted from `shared-volumes` on every host.
2. Subscription OAuth: login at home,
   phantom refresh, access tokens swapped at home [2, 3].
3. Structured refusals, `devsbd access wait`, zidane handling them [4].
4. Metering, budgets, model allowlist per profile [4].
5. The `mcp` claim, MCP OAuth at home, bearer-less injection, per-tool
   scopes [3]; then `mcp-credentials.json` and the `mcp-auth` volume
   dropped, their tokens revoked.
6. Other agents' features declaring the same claims.

## Open questions

- OpenAI, used live through ChatGPT-plan OAuth (`auth.openai.com`,
  `chatgpt.com/backend-api`): base URL (`OPENAI_BASE_URL`) or intercept; then
  any other provider zidane uses.
- Per-instance settings drift from each other: should zidane read its
  settings from a read-only host file apart from its writable state
  (`state.json` mixes both today)?
