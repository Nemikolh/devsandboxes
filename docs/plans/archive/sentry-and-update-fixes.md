> Archived 2026-11-22. Historical, may be wrong; do not use as a reference.
> Current: [daemon/updates.md](../../architecture/daemon/updates.md),
> [credentials CLI](../../reference/credentials-cli.md),
> [ADR 0060](../../decisions/0060-an-image-runs-the-updates-it-was-built-with.md),
> github.com/devsandboxes/features (`src/sentry`)

# Sentry's new CLI, and two update/run fixes

Status: agreed; building.

## Goal

1. A run refused for an unapproved credential claim leaves nothing behind,
   so "run again" means `devsandbox run` again.
2. A scheduled or manual feature update runs what the image itself was
   built with, never a newer feature's command the image doesn't have.
3. The sentry feature ships Sentry's new CLI (`sentry`, cli.sentry.dev,
   github.com/getsentry/toolkit `packages/cli`) instead of the Rust
   `sentry-cli`, brokered and updated like gh.

## Step 1: roll back a run refused for an unapproved claim

`run` keeps any failed fresh instance as `failed` (`progress::on_failure`,
`src/commands/run/progress.rs`), the claim refusal included, while that
refusal's message says to approve and try again; a second `run` makes
`web-2` next to the dead `web`. The refusal is an expected stop before any
container exists: roll it back like the client's ask path already does
(`src/commands/run/mod.rs`, the `NeedsApproval` arm of `result.map_err`,
`Unapproved::FailOnAsk`). `rebuild` (not fresh) keeps its instance as
today; its message names `devsandbox rebuild <instance>`, not "try again".

- Tests: a refused fresh run leaves no record (the existing
  `rollback_child` pattern in `run/mod.rs` tests), a refused rebuild keeps
  its instance.
- Docs: `docs/reference/credentials-cli.md` (*At `run`*),
  `docs/architecture/instances/images-and-features.md` (credential claims).

## Step 2: updates come from the image's label

Today `devsandbox update` reads `updates` from the feature's current
metadata and runs it in the newest local features image, built from
whatever feature version was current then: a feature release that moves
its updater (gh 1.3.0, zidane 2.1.0) fails in every image built before it.

- At build (`src/commands/run/image.rs`, `features_build`), label the
  features image `devsandbox.updates` with the JSON of every feature's
  `updates` in it, each with its mount's unresolved source
  (`${sharedVolumes}/…`): what the image's own updaters are.
- `src/commands/update.rs` (`declared`, `candidate`, `host_dir`) reads
  that label from each candidate image (a `Backend` method for an image's
  labels, docker/podman `inspect`), re-validates it with `src/updates.rs`'s
  checks, and runs only what it declares. An image without the label, or
  whose label lacks the update: manual `update` refuses naming
  `devsandbox rebuild <sandbox>`; the daemon skips it (no failure, no
  notice).
- Which sandboxes declare an update for the daemon's pass comes from the
  labels too, not the metadata.
- Tests: label JSON round trip (pure), choice over labelled/unlabelled
  images (pure), the docker test builds a labelled image.
- Docs: `docs/architecture/daemon/updates.md`, `docs/reference/update-cli.md`,
  `images-and-features.md`.

## Step 3: the sentry feature ships `sentry` (github.com/devsandboxes/features)

Facts (sentry 0.50.0): a Node SEA binary, glibc ≥ 2.28, linux x64/arm64
only (`sentry-linux-x64[.gz]`, `sentry-linux-arm64[.gz]` on GitHub release
`cli@<ver>` of getsentry/toolkit); sha256s in Sentry's release registry
(`release-registry.services.sentry.io/apps/sentry/<ver|latest>`,
`files.<asset>.checksums.sha256-hex`, `file_urls`). Token from
`SENTRY_AUTH_TOKEN` (`Authorization: Bearer`). Default host sentry.io; org
calls go to the org's region host (`us.sentry.io`, `de.sentry.io`). Node's
fetch: proxy only with `NODE_USE_ENV_PROXY=1`, CA from
`NODE_EXTRA_CA_CERTS`. Telemetry off with `SENTRY_CLI_NO_TELEMETRY=1`,
update banner off with `SENTRY_CLI_NO_UPDATE_CHECK=1`, self-update
`sentry cli upgrade`.

- Install/update: `sentry-update.sh` fetches the `.gz` asset, checks the
  registry's sha256 (of the asset fetched), into `<dir>/<version>/sentry`;
  `--bake` for the image; launcher `/usr/local/bin/sentry` (read-only
  shared dir's `current`, else the image's); `sentry cli upgrade` in a
  sandbox says devsandbox updates it (as zidane's launcher). `sentry-cli`
  and its launcher go. musl images refused with a clear message.
- Claim: **intercept mode** like gh (the CLI talks to the real hosts
  through the proxy port; the broker terminates TLS with the member CA):
  upstreams `sentry.io`, `us.sentry.io`, `de.sentry.io`; phantom in
  `SENTRY_AUTH_TOKEN` (no `.sentryclirc`, no base URL, no rewrites);
  `check: GET /api/0/auth/` (or `/users/me/`, whichever answers a token)
  with `checkShows`; scopes covering the routes the CLI calls (issues and
  events read, issue updates, seer autofix, logs/traces/explore reads,
  projects/teams/releases reads, release writes), destructive project and
  team routes `never`; logins: paste (a user auth token) and the CLI's own
  OAuth device flow (`sentry.io/oauth/device/code/`,
  `/oauth/token/`, its public client id) if the broker's device flow fits.
  Env: `NODE_USE_ENV_PROXY=1` (check devsandbox already sets it for
  intercept claims), `SENTRY_CLI_NO_TELEMETRY=1`,
  `SENTRY_CLI_NO_UPDATE_CHECK=1`. Verify the route list against the CLI
  0.50.0 source, and end to end against a mock as the gh claim was.
- `updates: [{ "name": "sentry", … }]` and the read-only `sentry-bin`
  mount stay. Version 1.2.0 (pre-1.0 rule: `sentry:1` users get it).
- README short, like gh's.

## Step 4: devsandbox follows the new sentry claim

- `tests/fixtures/features/sentry/devcontainer-feature.json` mirrors the
  feature; tests built on the old claim (base-url mode, chunk-upload and
  regions rewrites, the sentry-cli upload end-to-end test) move to what
  the new claim exercises, keeping base-url mode and rewrites covered by
  their own fixtures if no shipped claim uses them anymore.
- Docs naming `sentry-cli` (`docs/architecture/credentials/broker.md`,
  `docs/status.md`, `site/src/content/architecture/secrets.mdx`, code
  comments in `src/credentials/`, `src/devsbd/brokerfile.rs`,
  `devsbd/src/broker.rs`, `src/commands/run/mod.rs`, `src/state.rs`)
  follow.

## Closing

Living docs checked against the code; ADR for "an image runs the updates
it was built with" (refines 0059); plan archived.
