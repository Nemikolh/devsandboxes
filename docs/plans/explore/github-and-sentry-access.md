# GitHub and Sentry access

Status: explored, not agreed. Needs [credential-broker](credential-broker.md)
steps 1-3 (and its step 5 for remote sandboxes); its git half needs the
pass-through of [git-gateway](git-gateway.md) step 1. Design and diagrams:
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
plain feature elsewhere. Read with the rest of a feature's metadata at
`run`/`rebuild` (`feature_container_opts`,
[images-and-features](../../architecture/instances/images-and-features.md)).

| field | says |
|---|---|
| `upstreams` | hosts the credential is for |
| `mode` | `base-url` or `intercept` |
| `inject` | where the token goes: a header and its format, basic auth, a query parameter |
| `phantom` | env vars and files that get the phantom (`GH_TOKEN`, `~/.config/gh/hosts.yml`) |
| `baseUrl` | for `base-url`: the env var that takes the loopback URL |
| `login` | how `devsandbox login` gets it: OAuth device flow, browser OAuth, GitHub App manifest, paste |
| `mint` | an adapter devsandbox ships (`github-app`, `oauth-refresh`), if any |
| `scopes` | named scopes over routes: `pr.write = ["POST /repos/{repo}/pulls", …]` |
| `fence` | how a route names its repo or project (`{repo}` from the path, a GraphQL variable) |
| `ca` | a CA knob devsandbox doesn't set by default, if the tool has one |

Data only: a claim can't run anything on the home or a member.
Merged per instance; two features claiming one credential name must agree
or `run` refuses.

### A feature beside the installer, or the installer itself

`ghcr.io/devcontainers/features/github-cli` already installs gh. Either
`devsandboxes/features/gh` installs gh itself (one feature, version under
our control) or it only adds the claim and the wiring on top of the
official one (`installsAfter`; `dependsOn` isn't applied by devsandbox).
Leaning towards installing it: one entry in a config, and the claim's
scope table tracks the gh it installs.

## gh

- **Reaching the broker**: intercept mode (gh always talks to
  `api.github.com`; Go honors `HTTPS_PROXY` and reads the system bundle
  on Linux, so the member CA works with a stock gh). `GH_TOKEN` is the
  phantom, so `gh auth login` refuses and `gh auth status` reports logged
  in; `hosts.yml` gets the phantom for tools that read it. `gh auth token`
  prints the phantom: harmless.
- **The repo fence**: the linked repos are every remote of `folder` and
  `folders` (`github.com` URLs in https, `ssh://` and scp form), read on
  the host at `run`/`start` (`host_git`), so a remote added inside the
  container doesn't widen it. `[sandbox.x.access.github] repos = […]`
  adds more; arrays concatenate through `extends`.
- **Routes the broker can attribute**: REST `/repos/{owner}/{repo}/…`,
  release asset uploads on `uploads.github.com`, git smart HTTP.
- **Routes it can't**: `/search/*`, `/graphql` (one endpoint, any repo),
  `/notifications`, `/user/repos`, gists. With a token minted for the
  sandbox's repos GitHub fences these itself; with a user token they're
  scope `other` (ask) except a read-only allowlist gh needs (`/user`,
  `/rate_limit`, `/meta`).
- **git through gh** (`gh repo clone`, `gh pr checkout`) configures
  `gh auth git-credential`, which would hand git the phantom for a direct
  push to github.com. The feature sets git's `insteadOf` to the gateway
  instead ([git-gateway](git-gateway.md)) and gh's credential helper off.

### Whose token: the bot or you

GitHub offers three, and they differ in who GitHub thinks acted:

| token | acts as | narrowed per instance | lives | delegable |
|---|---|---|---|---|
| App installation token | the app (`yourapp[bot]` authors the PR) | yes: repos and permissions at mint | 1 h, fixed | yes |
| App user token | you, within the app's installed repos and permissions | no, only by the broker's rules | 8 h, refresh token 6 months | no: it isn't narrow |
| OAuth / PAT (gh's own login) | you, everything the token can | no | until revoked | no |

Per sandbox, not global: `as = "app"` for unattended agents (their PRs
visibly come from a bot, and the fence is GitHub's), `as = "user"` where
you want your own name. Installation tokens are per owner: a sandbox whose
repos span two owners (your fork, the org's upstream) gets one per owner,
picked by route. `devsandbox login github --app` runs the manifest flow
(a prefilled "create app" page; GitHub redirects back with the app id and
key, stored at home).

## sentry-cli

- **Reaching the broker**: base-URL mode. `SENTRY_URL` points at the
  loopback port, `SENTRY_AUTH_TOKEN` and `~/.sentryclirc` hold the phantom.
  No CA.
- **Scopes**: routes under `/api/0/organizations/{org}/…` and
  `/api/0/projects/{org}/{project}/…`; the fence is the org (and
  projects, if listed in `access.sentry-…`).
- **Credential**: an org auth token (`sntrys_`, org-bound, limited
  permissions) rather than a user token. No minting, so never delegated:
  a sandbox on another machine is always brokered from home.
- **Profiles**: one credential per org (`sentry-work`), granted to the
  profiles that may use it.

## Ideas weighed

| question | option | U | N | verdict |
|---|---|---|---|---|
| how gh reaches the token | CLI shim + argv policy ([cli-proxy](cli-proxy.md)) | 5 | 3 | no: misses git, SDKs, `gh api` |
| | the broker, intercept mode | 9 | 6 | chosen |
| GraphQL fence | parse operations and variables | 5 | 5 | a second fence at most |
| | a token minted for the repos | 9 | 6 | chosen where `as = "app"` |
| | deny `/graphql` | 3 | 2 | breaks most of gh |
| claim location | devsandbox's own table of known tools | 6 | 2 | every tool a devsandbox release |
| | in the feature's metadata | 9 | 7 | chosen: features ship with their tool |
| sentry chunk uploads | rewrite the upload URL the server returns to the loopback | 6 | 5 | if needed (open question) |

## Rollout

1. The claim contract read from feature metadata; phantoms and base-URL
   wiring from a claim. `features/sentry`; live config drops the sentry
   mount.
2. `features/gh` in intercept mode with a user token (`as = "user"`), the
   repo fence from remotes, `access.github.repos`, the read-only
   allowlist; git via the gateway's pass-through. Live config drops the gh
   mount.
3. `devsandbox login github --app`, installation tokens per instance and
   owner, `as = "app"`, delegation to members.

## Open questions

- Does sentry-cli follow absolute URLs the server returns (chunk upload
  endpoints) straight to sentry.io, past `SENTRY_URL`? To verify; the fix
  is a response rewrite for those routes.
- The default for `as`: `app` for dispatcher children (unattended),
  `user` otherwise?
- gh extensions and `gh copilot`, `gh codespace`: other upstreams, out of
  the claim unless asked.
- GitHub Enterprise hosts: the claim's `upstreams` per host, one
  credential each.
