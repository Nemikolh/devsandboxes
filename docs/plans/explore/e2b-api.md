# An E2B-compatible API, both ways

Status: explored, not agreed. Two directions: devsandbox *serving* the
E2B API, so E2B's SDKs and the agents built on them run sandboxes on the
user's own machines and fleet; and devsandbox *consuming* an
E2B-compatible cluster such as sandbawks, as a backend or a fleet member
([below](#the-reverse-direction-running-on-an-e2b-compatible-cluster)).
Absorbs the former `sandbawks-runtime` plan.

## E2B in brief

[E2B](https://e2b.dev) runs cloud sandboxes for agents, a Firecracker
microVM each. Its infra is Apache-2.0 and self-hostable; the API is open
in `e2b-dev/E2B`:

- **Control API** (`spec/openapi.yml`, `X-API-Key` header): `GET`/`POST
  /sandboxes` and `POST /v2/sandboxes` (`NewSandboxV2`: `templateID`,
  `timeout` in seconds, default 300, `autoPause`, `autoPauseMemory`,
  `autoResume`, `metadata`, `envVars`, `network`, `volumeMounts`),
  `GET`/`DELETE /sandboxes/{id}`, `/pause`, `/connect` (resumes a paused
  sandbox; `/resume` is deprecated), `/timeout`, `/refreshes`, `/logs`,
  `/metrics`, `/snapshots`, `/fork`, `PUT /network`; templates under
  `/templates`, `/v2/templates`, `/v3/templates` (builds, tags). A
  created `Sandbox` answers `sandboxID`, `envdVersion`, an
  `envdAccessToken` and the `domain` its traffic is served under.
- **envd**, the in-sandbox daemon (`spec/envd/`, port 49983, `X-Access-Token`):
  Connect RPC services `Process` (`List`, `Start` and `Connect` streaming,
  `Update`, `StreamInput`, `SendInput`, `SendSignal`, `CloseStdin`; PTY
  sizes) and `Filesystem` (`Stat`, `MakeDir`, `Move`, `ListDir`, `Remove`,
  `WatchDir` and polled watchers), plus HTTP `/files` upload and download,
  `/envs`, `/health`, `/init`.
- **SDKs** (JS, Python) take an API key and a domain (`E2B_DOMAIN`,
  default `e2b.app`; the API at `api.<domain>`, or `E2B_API_URL`;
  `E2B_SANDBOX_URL` overrides where envd is reached), and reach a
  sandbox's ports, envd's included, at per-port hosts under the sandbox
  domain (`getHost`). A `debug` mode talks to `localhost`.

## Serving it: `devsandbox serve --e2b <addr>`

One binary: a listener in the per-user daemon
([client-server](../../architecture/daemon/client-server.md)), off unless asked, translating
E2B calls into the API methods the dashboard and CLI already use
([api](../../reference/api.md), `serve::verbs`).

| E2B | devsandbox |
|---|---|
| template (`templateID`) | a sandbox config, `[sandbox.<name>]` of the root the listener serves; template builds refused (config is written, not uploaded: [ADR 0002](../../decisions/0002-config-is-the-security-boundary.md)) |
| sandbox (`sandboxID`) | an instance, by its persistent id ([ADR 0006](../../decisions/0006-persistent-instance-id-separate-from-name.md)) |
| `POST /v2/sandboxes` | the `run` verb, detached, answered once the instance is up; with `--place` semantics on a control plane, so the API fronts the fleet |
| `GET /sandboxes`, `GET /sandboxes/{id}` | `instances.list` rows, `metadata` stored with the instance |
| `DELETE /sandboxes/{id}` | `rm` |
| `timeout`, `/timeout`, `/refreshes` | a new per-instance deadline: at expiry `rm`, or `stop` with `autoPause` |
| `/pause`, `/connect` | `stop` / `start`. Memory isn't kept: that's E2B's own `autoPauseMemory: false` kind (a filesystem-only snapshot, cold boot on resume). Kept memory needs gVisor's checkpoint ([isolation](../active/isolation.md)) |
| `/logs`, `/metrics` | `containers.logs`, `containers.stats` |
| `/snapshots`, `/fork` | nothing yet: WIP checkpoints (fleet step 14) and `run --from` ([sandbawks-ideas](sandbawks-ideas.md)) are the nearest |
| `envVars` | refused or allowlisted per key: a request's env bypasses the config boundary otherwise |
| `network` (egress rules) | nothing; egress policy is an idea in [sandbawks-ideas](sandbawks-ideas.md) |

**envd onto the helper.** The listener speaks envd's Connect RPC itself
and maps each call onto the instance's helper over `RPC` streams
([in-container](../../architecture/helper/in-container.md)):

- `Process.Start` without a PTY → `run.start`, `run.logs {follow}`,
  `run.wait`; with a PTY → `session.attach {create}` (resize records
  for `Update`); `Connect` → `run.logs` / `session.attach` by id;
  `SendSignal` → `instances.signal`; `List` → `instances.procs`;
  `SendInput` / `StreamInput` / `CloseStdin` → the session's `stdin` and
  `stdin-eof` records.
- `Filesystem.*` and `/files` → nothing exists: a new helper `fs.*` RPC
  family (stat, list, read, write, move, remove, watch). The helper has
  no dependencies and no libc ([ADR 0022](../../decisions/0022-helper-has-no-dependencies.md)),
  so `WatchDir` means raw inotify syscalls, or polling.
- A sandbox's other ports (`getHost(port)`) → the forward engine's
  `Connect` streams into the instance
  ([forwards](../../architecture/daemon/forwards.md)), routed by the
  request's `Host` (`<port>-<id>.<domain>`), so a wildcard name must point
  at the listener (or one `E2B_SANDBOX_URL`).

**Auth and exposure.** API keys in a file next to `state.toml`, stored
hashed, each scoped to one config root (and optionally some sandboxes),
served under a table like a link peer's (`lent::Peer`'s confinement),
never `Full`. `envdAccessToken` minted per instance at create, checked on
envd and port traffic. The listener binds loopback by default; a tailnet
address is opt-in, and off loopback the SDKs expect `https`, so it needs
TLS (rustls, shared with [mtls-links](../active/mtls-links.md)) or a
front such as `tailscale serve`.

**Cost.** No HTTP server is in the tree; envd's streaming RPCs (server
and client streaming) want HTTP/2 to work well. Either an async stack
(hyper and friends, in a daemon built on threads) or a hand-rolled
HTTP/1.1 plus the Connect envelope, which limits streaming. This is the
bulk of the work.

## The reverse direction: running on an E2B-compatible cluster

[sandbawks](https://github.com/hypotheosis/Sandbawks) is an
E2B-compatible sandbox control plane on Kubernetes: gVisor-isolated Pods
(Kata optional), warm pools, and pause/resume that checkpoints memory,
processes and files to S3 (`runsc checkpoint` plus a containerd shim that
restores at start). Its in-sandbox agent `envd` serves exec, filesystem
and PTY; clients use the E2B SDKs. The goal there:
`DEVSANDBOX_RUNTIME=sandbawks` (or a per-sandbox runtime) runs instances
on such a cluster, for capacity beyond the user's machines, stronger
isolation, and instances that pause with their processes alive.

| devsandbox needs | an E2B cluster has | gap |
| --- | --- | --- |
| build an image | templates from an image | no build here; push to a registry first |
| bind-mount the worktree | none; files via `envd` or PVCs | code must move in, as fleet step 12 moves it ([fleet](../active/fleet.md)) |
| `exec -it` | `envd` process/PTY RPCs | a Rust client, or the helper over an `envd` exec |
| `devsbd` bridges | none | the helper runs inside; its frames need a stream (an `envd` PTY or a port) |
| services, networks | none (one Pod per sandbox) | unsupported, or extra containers in the Pod |
| `stop`/`start` | `pause`/`connect`, memory kept | better than today |
| events, `list`, `stats` | the API's sandbox list and metrics | poll |

Much of `Backend` is docker-shaped argv (`run`, `build`, `exec`); this
backend is an API client, a different kind of backend, closer to a fleet
member than to podman. **Alternative: a fleet member.** A daemon fronting
the cluster joins the fleet with capacity, placement sends work there,
and the member translates verbs to E2B calls, reusing code transfer,
routed streams and fetch-back.

The two directions meet: a devsandbox serving the E2B API is an
E2B-compatible cluster, so the reverse direction's client could be
tested against `devsandbox serve --e2b` itself.

## Questions

- Worth serving at all, or only the reverse direction? The case for
  serving: agent harnesses written for E2B run unchanged on the user's
  own machines and fleet.
- Which subset first: create/list/kill/timeout plus `Process`, leaving
  `Filesystem`, snapshots and templates refused?
- HTTP stack: an async dependency in a threaded daemon, or a minimal
  hand-rolled server with limited streaming?
- Reverse: backend or fleet member? Is losing services and bind mounts
  acceptable for the sandboxes that would run there (a dispatcher's agent
  children, not hand-edited ones)? A key per user, stored where?
- One client for zidane: `devsandboxes-zidane-extension` and sandbawks'
  connection contract (`docs/sandbox-connection-ux-contract.md` there)
  are two session environments for the same harness.
