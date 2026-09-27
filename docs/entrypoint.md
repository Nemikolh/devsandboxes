# Feature entrypoints

A devcontainer feature can declare an `entrypoint` in its
`devcontainer-feature.json`: a command to run at **every container start**, for
setup that can't be baked in at build time. docker-in-docker's
`/usr/local/share/docker-init.sh` starts `dockerd`; docker-outside-of-docker's
(non-root user) starts a `socat` relay that makes the host socket usable by
that user. Without them the feature installs fine but doesn't work.

## Behaviour

When at least one enabled feature declares an `entrypoint`, the derived image
`devsandbox-img-<sandbox>-feat` gets an entrypoint chain
(`/usr/local/share/devsandbox-entrypoint.sh`) that, on every start:

1. runs each feature's entrypoint, in install order (`installsAfter`), with no
   arguments. They conventionally end with `exec "$@"`, which is a no-op
   without arguments, so each returns and the next one runs;
2. `exec`s the image's own `ENTRYPOINT` followed by the container command
   (`sleep infinity`), exactly what ran before features added a chain.

A failing feature entrypoint prints
`devsandbox: entrypoint of feature '<id>' exited with status N; continuing` to
the container logs and the chain goes on, so one broken feature can't keep the
sandbox from starting. Sandboxes whose features declare no entrypoint are built
exactly as before.

Because it's the image `ENTRYPOINT`, `devsandbox start` (a container restart)
re-runs the chain too. `run_container` is untouched.

### Build

`ENTRYPOINT` in a Dockerfile replaces the parent's and can't refer to it, so
the image's own is read with an image inspect (`Backend::image_entrypoint`).
The features build runs as before. The resulting image inherits the base's
`ENTRYPOINT` untouched (the features Dockerfile never sets one). That image is
inspected, then a second, metadata-only build on the same tag adds:

```dockerfile
FROM devsandbox-img-<sandbox>-feat
COPY devsandbox-entrypoint.sh /usr/local/share/devsandbox-entrypoint.sh
ENTRYPOINT ["/bin/sh", "/usr/local/share/devsandbox-entrypoint.sh", <image ENTRYPOINT…>]
```

The chain runs through `/bin/sh`, so it doesn't depend on how a builder handles
the exec bit on `COPY`.

### Sandbox-level `entrypoint`?

devcontainer.json has no `entrypoint` property, and devsandbox rejects unknown
keys. The only other entrypoint in play is the image's `ENTRYPOINT`, which is
chained after the feature entrypoints (never replaced).

## Differences with the devcontainers CLI

| | devcontainers CLI | devsandbox |
| --- | --- | --- |
| Where the chain lives | Runtime: `docker run --entrypoint /bin/sh -c '<script>'` (`singleContainer.ts`) | Build time: the derived image's `ENTRYPOINT` |
| Image's own `ENTRYPOINT` | Dropped by default (`overrideCommand` defaults to `true`). Only with `overrideCommand: false` does it run after the feature entrypoints, with its `CMD` | Always kept, run after the feature entrypoints (devsandbox never overrode it; `overrideCommand` is accepted but ignored) |
| Keep-alive command | Shell loop with a `SIGTERM` trap | `sleep infinity`, unchanged |
| Failing feature entrypoint | Silently continues | Continues with a warning in the container logs |
| Order, arguments | Install order, no arguments, command inserted verbatim | Same |

## Caveats

- **Image inspect.** The image's `ENTRYPOINT` comes from `docker`/`podman image
  inspect --format '{{json .Config.Entrypoint}}'`, or from Apple's `container
  image inspect` (`variants[].config.config.Entrypoint`, the `linux/<host
  arch>` variant, else the first). The Apple shape was checked against the
  `apple/container` / `containerization` sources, not on a real Mac; an
  unexpected shape fails the build with `cannot read the ENTRYPOINT of …`
  rather than silently dropping the image's entrypoint.
- **User.** The chain runs as `containerUser` (`run --user`). Entrypoints that
  need root fall back to `sudo` when not root (docker-in-docker does), which
  requires passwordless sudo, e.g. from the common-utils feature. Same as
  upstream.
- **Readiness race.** `run` carries on (devsbd install, lifecycle commands) as
  soon as the container is up, while the chain may still be working:
  docker-in-docker waits up to ~25s for `dockerd`, so a `postCreateCommand`
  that uses `docker` right away can lose the race. The CLI doesn't wait either.
- **The image's `ENTRYPOINT` must `exec "$@"`**, or `sleep infinity` never
  runs. That's a pre-existing requirement, not new to the chain.
- **Drift.** Feature metadata isn't part of the config hash, so a feature
  gaining, changing or dropping an `entrypoint` doesn't trigger the "config
  changed" warning. Containers created before this support need a `devsandbox
  rebuild` to get the chain.
