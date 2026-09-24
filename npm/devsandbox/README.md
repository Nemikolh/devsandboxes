# devsandbox

Manage devcontainer-style sandboxes on docker, podman, or Apple `container`: many throwaway instances per repo from one `config.toml`. This package ships the native `devsandbox` binary (Linux x64/arm64, macOS arm64, Windows x64) plus a typed Node API.

Config reference and concepts: [github.com/Nemikolh/devsandboxes](https://github.com/Nemikolh/devsandboxes).

## CLI

```bash
npx devsandbox            # dashboard (on a TTY)
npx devsandbox run web    # start an instance of [sandbox.web]
npx devsandbox exec -it web zsh
```

## Node API

```ts
import * as devsandbox from 'devsandbox';

const name = await devsandbox.run('web', { dir: './sandboxes', stderr: 'inherit' });
const { instances } = await devsandbox.status({ dir: './sandboxes' });
for (const i of instances) console.log(i.name, i.status.state, i.drift);

const { exitCode, stdout } = await devsandbox.exec(name, ['git', 'status']);
await devsandbox.rm(name);
```

| Function                                   | Resolves to                          |
| ------------------------------------------ | ------------------------------------ |
| `status()`                                 | `Snapshot` (sandboxes, instances, services) |
| `ls()` / `ps({ all })` / `stats()`         | `SandboxRow[]` / `ContainerRow[]` / `StatsRow[]` |
| `service.ls()`                             | `ServiceRow[]`                       |
| `inspect(name)`                            | runtime inspect document (`unknown`) |
| `run(sandbox, { name, branch, base })`     | instance name                        |
| `start` / `stop` / `rebuild(name \| { all: true })` | `void`                      |
| `rm(name)` / `rename(a, b)` / `gc()` / `service.rebuild(name)` | `void`          |
| `logs(name, { lines })`                    | log text                             |
| `exec(name, argv, { input })`              | `{ exitCode, stdout, stderr }` (never rejects on exit code) |
| `cli(args)`                                | raw `{ exitCode, stdout, stderr }`   |

Every function takes `{ dir, cwd, env, signal, stderr }`. `dir` is the config root (`-C`). A failed command rejects with `DevsandboxError` (`exitCode`, `stdout`, `stderr`).

## Platforms

The binary comes from an optional dependency (`@devsandboxes/<platform>-<arch>`), so don't install with `--omit=optional`. On other platforms, `cargo install devsandbox` and point `DEVSANDBOX_BINARY` at the result.
