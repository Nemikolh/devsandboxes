# Container runtimes

devsandbox drives containers through a `Backend` (`src/runtime/`). Two
implementations exist:

| backend | binary | shape |
|---|---|---|
| `docker`, `podman` | `docker` / `podman` | `Dockerlike`: Go-template `inspect -f`, `ps --filter`, `network connect`, `--network-alias`, `top` |
| `container` | `container` (Apple, macOS 26+) | `AppleContainer`: JSON-only `inspect`/`ls`, all `--network`s at `run`, `/etc/hosts` for service names, `exec … ps` instead of `top` |

## Selection

1. `DEVSANDBOX_RUNTIME=docker|podman|container` when set.
2. Otherwise, on macOS, `docker` when the `docker` client is on `PATH`, else
   Apple `container`; `docker` everywhere else.

An unknown value warns and falls back to rule 2.

OrbStack needs no special handling: it *is* the `docker` backend. It ships the
standard `docker` CLI and auto-selects its own `docker context`, so once it's
installed the PATH probe picks `docker` and every call goes through
`Dockerlike`. (Same for Docker Desktop or any other engine that puts `docker`
on `PATH`.)

## Apple `container` caveats

- Networks (and therefore `services`) need macOS 26+ and `container system start`.
- Service discovery: `container` has no `--network-alias`; after services are
  up, each service's IPv4 is written to the sandbox's `/etc/hosts` under the
  service name (refreshed on every `run`, including restarts).
- `container build` has no `--cache-from`; `build.cacheFrom` is ignored with a
  warning.
- `container stats` reports raw counters, so the dashboard shows `-` for CPU.
- Status text in `ps` and the dashboard is the bare state (`running`/`stopped`)
  rather than docker's `Up 3 minutes`.

## Adding a runtime

A docker-CLI-compatible runtime is one line: another `Dockerlike` constant in
`src/runtime/dockerlike.rs` plus its name in `backend_named` /
`default_backend_name` (`src/runtime/mod.rs`). Anything else implements
`Backend`; the trait's query methods (`is_running`, `label`, `list`, …) are the
only places that have to understand the runtime's output format.
