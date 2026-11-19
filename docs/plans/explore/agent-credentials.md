# Agent credentials

Status: explored, not agreed. zidane first, any coding agent after. Needs
[credential-broker](credential-broker.md) (base-URL mode, permission
requests, `CRED` over the fleet); its GitHub side is
[github-and-sentry-access](github-and-sentry-access.md). Design and
diagrams: [Credentials and access](../../../site/src/content/architecture/secrets.mdx),
*zidane and other agents*.

## Goal

- zidane is logged in on every machine with nothing in the sandbox: no
  model key, no OAuth tokens in `shared-volumes/zidane-config`.
- Whatever zidane does with other credentials (gh, git, sentry, MCP
  servers) is bounded by its instance, like anything else in the sandbox.
- When it hits the edge of its scope it can tell, wait, and carry on.

Today the live config shares `zidane-config` (credentials, `config.json`)
across every sandbox of a host through `${sharedVolumes}`; another machine
starts empty and needs its own login.

## An agent is three things

### A tool with its own login

- **API key**: a static credential, brokered in base-URL mode (the
  provider's base URL env, a phantom key). Never delegated: a sandbox on
  another machine reaches the home for every call.
- **Subscription OAuth**: the home keeps the refresh token; the broker swaps
  the phantom bearer for the current access token, refreshed at home. The
  access token isn't narrowed (it's the whole account), so it isn't
  delegated either.
- **The client's own refresh**: the broker answers the provider's token
  endpoint itself for a phantom, with the same phantom and a far expiry,
  so the agent's refresh logic is a no-op and nothing it stores is real.
- **Streaming**: responses (SSE) pass through unbuffered; a hop home adds
  to the first byte only.
- **Metering**: the claim says where a provider's responses report usage;
  the broker counts per instance and profile against budgets and a model
  allowlist. Over budget is an `ask`, not a hard stop.

### A principal acting with everything else

- Its gh, git and sentry calls carry its instance's identity: the repo
  fence, the scopes, the profile's grants. Dispatcher children inherit the
  dispatcher's profile ([profiles](../../architecture/instances/profiles.md)),
  and default to GitHub `as = "app"`, so their PRs come from a bot.
- **`presence` routes** (a scope each feature's table may mark: pushing a
  release, deleting a branch, publishing a package) ask a human even when
  allowed.
- **MCP servers over HTTP**: OAuth done at home (`devsandbox login
  mcp:<server>`), replacing the `mcp-auth` shared volume; the MCP client
  points at the loopback. The broker can read JSON-RPC `tools/call`
  bodies, so a server's tools get scopes of their own (allow
  `search`, ask `create_page`).

### A client of permission requests

The agent is the one most likely to hit an `ask` or a deny, and must not
read it as a crash.

- **Held calls**: an `ask` holds the request (120 s default); most answers
  arrive within it and the agent never notices.
- **A structured refusal** when it doesn't: the `403`/`503` body carries a
  JSON object the agent can parse and act on:

  ```json
  { "devsandbox": { "verdict": "pending", "request": "r_7k2", "credential": "github",
                    "scope": "issues.write", "retry_after": 30,
                    "why": "issues.write asks you for web" } }
  ```

- **Waiting on purpose**: `devsbd access wait <request>` blocks until the
  answer (over `api.sock`), so the agent can park a step and come back.
- **zidane's side**: recognize the object, show it in its own UI, wait or
  pick another path; a skill teaching other agents the same.

## Ideas weighed

| idea | from | U | N | verdict |
|---|---|---|---|---|
| broker the model API in base-URL mode | API gateways | 9 | 5 | chosen |
| answer the OAuth refresh endpoint with phantoms | token exchange (RFC 8693), Fly's tokenizer | 8 | 8 | chosen: no agent change for refresh |
| per-tool MCP scopes from JSON-RPC bodies | the gateway's receive-pack parsing | 8 | 8 | chosen once MCP is brokered |
| budgets per instance | cloud billing alerts | 7 | 5 | ask over budget |
| a human for `presence` routes | two-person rule, sudo | 8 | 7 | chosen |
| run the agent outside the sandbox, only tools inside | split-brain agents | 6 | 8 | no: the agent needs the sandbox's files and tools; its credentials are the problem, not its location |
| give the agent its own identity apart from the instance | SPIFFE per process | 5 | 7 | later: the bridge can't tell processes apart in a container |

## Rollout

1. zidane's model credential brokered (API key, base-URL mode) through
   its feature's claim; live config stops sharing credentials in
   `zidane-config` (config and state stay shared).
2. Subscription OAuth: login at home, phantom refresh, access tokens
   swapped at home.
3. Structured refusals, `devsbd access wait`, zidane handling them.
4. Metering, budgets, model allowlist per profile.
5. MCP OAuth at home, per-tool scopes; the `mcp-auth` volume dropped.
6. Other agents' features declaring the same claims.

## Open questions

- Which providers and auth modes zidane uses, and whether each takes a
  base URL from env or config; an OAuth flow pinned to its host can only
  be intercepted (member CA), not pointed elsewhere.
- How zidane splits `zidane-config` today (credentials vs settings), so
  only the credential part goes.
- One agent session spanning instances (an orchestrator driving
  children): whose identity, the orchestrator's or each child's?
