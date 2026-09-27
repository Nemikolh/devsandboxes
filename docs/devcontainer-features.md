# Plan: devcontainer `features` support

Addresses finding #6 in `docs/config-toml-findings.md` (lines 88–94): `goj`,
`wasm-typescript` and others originally used devcontainer features
(go, rust, node, common-utils/zsh, git); today those must be baked into images
or `postCreateCommand`.

## Design

Native Rust implementation, no dependency on the devcontainers CLI. A feature
is an OCI artifact (e.g. `ghcr.io/devcontainers/features/node:1`): a tar layer
containing `devcontainer-feature.json` + `install.sh`. Support pipeline:

1. **Parse** `features` as a typed map `ref -> options` in `SandboxProperties`
   (currently `Option<Value>`, flagged as ignored at `src/config.rs:67,103`).
2. **Fetch** each feature from its OCI registry by shelling out to `curl`
   (matching the project's shell-out-to-docker/git philosophy; no new crates):
   anonymous pull token → manifest (`application/vnd.oci.image.manifest.v1+json`)
   → layer blob → extract with `tar` into a per-user cache
   (`$XDG_CACHE_HOME/devsandbox/features/<registry>/<path>/<tag>/`, mirroring
   `State::path` style at `src/state.rs:63`). A cached extraction is reused
   as-is (offline-friendly; mutable tags like `:1` won't auto-refresh — v1
   limitation, clear the cache dir to force).
3. **Order** installs by `installsAfter` from feature metadata (topological,
   alphabetical tie-break, refs matched sans tag). `overrideFeatureInstallOrder`
   stays unimplemented/ignored.
4. **Build** a derived image when `features` is set: generate a Dockerfile in a
   temp build context, mirroring the official CLI's layout
   (`containerFeaturesConfiguration.ts:205-363`, `containerFeatures.ts:227-360`):
   `FROM <base>` + `USER root`; per feature (in install order) `ENV` lines from
   its `containerEnv` (escape `"` and `\` only — `# Plan: devcontainer `features` support

Addresses finding #6 in `docs/config-toml-findings.md` (lines 88–94): `goj`,
`wasm-typescript` and others originally used devcontainer features
(go, rust, node, common-utils/zsh, git); today those must be baked into images
or `postCreateCommand`.

## Design

Native Rust implementation, no dependency on the devcontainers CLI. A feature
is an OCI artifact (e.g. `ghcr.io/devcontainers/features/node:1`): a tar layer
containing `devcontainer-feature.json` + `install.sh`. Support pipeline:

1. **Parse** `features` as a typed map `ref -> options` in `SandboxProperties`
   (currently `Option<Value>`, flagged as ignored at `src/config.rs:67,103`).
2. **Fetch** each feature from its OCI registry by shelling out to `curl`
   (matching the project's shell-out-to-docker/git philosophy; no new crates):
   anonymous pull token → manifest (`application/vnd.oci.image.manifest.v1+json`)
   → layer blob → extract with `tar` into a per-user cache
   (`$XDG_CACHE_HOME/devsandbox/features/<registry>/<path>/<tag>/`, mirroring
   `State::path` style at `src/state.rs:63`). A cached extraction is reused
   as-is (offline-friendly; mutable tags like `:1` won't auto-refresh — v1
   limitation, clear the cache dir to force).
3. **Order** installs by `installsAfter` from feature metadata (topological,
   alphabetical tie-break, refs matched sans tag). `overrideFeatureInstallOrder`
   stays unimplemented/ignored.
 is left for build-time
   expansion, e.g. `PATH=/usr/local/go/bin:${PATH}`), then `COPY` its dir and
   `RUN` a generated `devcontainer-features-install.sh` wrapper that `set -a`
   sources `devcontainer-features.builtin.env` (`_CONTAINER_USER`,
   `_REMOTE_USER` from config, default `root`; `_*_HOME` appended in-container
   via `getent passwd <user> | cut -d: -f6`) and the feature's
   `devcontainer-features.env` (options as `NAME="value"`, name mapped like the
   CLI's `getSafeId`: non-word → `_`, leading digits/underscores stripped,
   uppercased; defaults from metadata merged under user values; string/bool
   shorthand → the `version` option) before running `install.sh`. Restore
   `USER <containerUser>` at the end when set. Base = `image` as-is, or the
   existing sandbox build (`image_for`, `src/commands/run.rs:471`). Derived
   tag: `devsandbox-img-<sandbox>-feat`.

### v1 scope cuts (documented, not silent)

- OCI refs only; local-path / https-tarball features error clearly.
- Feature metadata `capAdd`, `entrypoint`, `init` ignored (the runner uses
  `sleep infinity`; the four target features need none of these).
  `containerEnv`, options, `mounts` and `privileged` are honored (`privileged:
  true` on any enabled feature runs the container `--privileged`; skipped with
  a warning on Apple `container`, which has no such flag).
- No image-config introspection for the default user (the CLI inspects the
  image and uses its `User` — `containerFeatures.ts:385-390`): `_REMOTE_USER`
  falls back to `root` when `remoteUser`/`containerUser` are unset.
- `dependsOn` and `roundPriority`/`overrideFeatureInstallOrder` (the CLI's
  full ordering in `containerFeaturesOrder.ts`) not resolved; `installsAfter`
  only.
- Plain build context, no BuildKit build contexts / temp content image (the
  CLI's two strategies in `containerFeatures.ts:237-263`).

## Step 1 — typed `features` in config

`src/config.rs`: replace `features: Option<Value>` with
`Option<BTreeMap<String, FeatureOptions>>` where `FeatureOptions` is untagged
`Table | String | Bool` (devcontainer shorthand: a bare string means the
`version` option, `true` means no options). Remove `features` from the
`ignored()` macro list (`src/config.rs:103`). Tests: parse all three option
forms, template merge of `features` tables, `ignored()` no longer flags it.

Constraints: keep `deny_unknown_fields` behavior; do not touch other fields.

## Step 2 — `src/features.rs`: fetch + metadata

New module (registered in `src/main.rs`):

- `FeatureRef::parse("ghcr.io/devcontainers/features/node:1")` →
  `{ registry, path, id, tag }` (tag defaults to `latest`; refuse refs without
  a registry host / local paths with a clear error).
- Fetch via `curl` (`-fsSL`), following the official auth flow
  (`httpOCIRegistry.ts:33-37,551`): request the manifest anonymously; on a 401
  parse the `WWW-Authenticate: Bearer realm=…,service=…,scope=…` challenge and
  `GET <realm>?service=…&scope=…` for a token (registry-generic: works for
  ghcr.io and docker.io alike). Manifest Accept:
  `application/vnd.oci.image.manifest.v1+json`; pick the layer with mediaType
  `application/vnd.devcontainers.layer.v1+tar`
  (`containerCollectionsOCI.ts:14,534`) → blob → pipe into `tar -x` under the
  cache dir. Skip everything when the cache dir already holds
  `devcontainer-feature.json`.
- `FeatureMetadata` (serde, tolerant/`default`): `id`, `version`, `options`
  (map with `default` values), `containerEnv`, `installsAfter`, `entrypoint`.
- `install_order(&[ResolvedFeature]) -> Vec<...>`: topo sort per
  `installsAfter`, cycle → error.

Tests: ref parsing (with/without tag, invalid), install-order sort, option
env-name mapping, metadata deserialization from a JSON fixture string; plus a
**runtime-gated network test** that fetches `common-utils` from ghcr.io — it
skips (with a message) when `curl` can't reach the registry, and runs for real
on CI runners.

## Step 3 — derived image build in `run`

`src/commands/run.rs`:

- `image_for` grows a features path: base image or base build first, then
  fetch features, generate the Dockerfile + build context (feature dirs copied
  in), `backend().run_checked(["build", "-t", "<tag>-feat", ...])`.
- Dockerfile generation is a pure function (base image + ordered features +
  users) returning the Dockerfile string — unit-tested without docker:
  option env serialization (bool/string, defaults), `${containerEnv:PATH}`
  translation, per-feature `RUN` blocks, quoting.

Constraints: build context lives under the features cache
(`.../build-<sandbox>/`), recreated each run (docker layer cache makes rebuilds
cheap). No behavior change for sandboxes without `features`.

Adds `tests/features_docker.rs`: **docker-gated integration test** that builds
a derived image from a tiny base with a synthetic pre-extracted feature (no
network needed), asserting the install script ran and `containerEnv` landed.
The test probes for a usable runtime (`docker info`) at start and skips with a
message when absent — so it's a no-op in this sandbox but exercises the real
build on CI.

## Step 4 — example config + findings doc

- `data/config.toml`: `wasm-typescript` gains the
  `go` feature (comment at line 135; goreleaser/tinygo stay in
  `postCreateCommand` — no official feature exists). `goj` keeps its prebuilt
  image (its comment carries no `[PROPOSED]`).
- `docs/config-toml-findings.md`: #6 → DONE with an implementation summary +
  limitations; update the Status list (lines 9–13).

## Validation per step

`cargo test`, `cargo clippy`, `cargo fmt --check`. Docker- and network-gated
tests skip gracefully where the tool is unavailable (this sandbox has no
docker) and run for real on CI: `.github/workflows/ci.yml` already executes
`cargo test` on `ubuntu-latest`, which ships docker — no workflow change
needed. One step = one review = one commit.
