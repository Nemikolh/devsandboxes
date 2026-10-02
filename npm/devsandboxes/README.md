# devsandboxes

Manage devcontainer-style sandboxes on docker, podman, or Apple `container`: many throwaway instances per repo from one `devsandboxes.toml`. This package ships the native `devsandbox` binary (Linux x64/arm64, macOS arm64, Windows x64) plus a typed Node API.

The package installs a `devsandbox` command (`npm i -g devsandboxes`); `npx devsandboxes` runs it without installing.

Config reference and concepts: [github.com/Nemikolh/devsandboxes](https://github.com/Nemikolh/devsandboxes).

## CLI

```bash
npx devsandboxes            # dashboard (on a TTY)
npx devsandboxes run web    # start an instance of [sandbox.web]
npx devsandboxes exec -it web zsh
```

## Node API

```ts
import { spawn } from 'node:child_process';
import * as devsandbox from 'devsandboxes';

const { name, worktree } = await devsandbox.run('web', { dir: './sandboxes', stderr: 'inherit' });
const { instances } = await devsandbox.status({ dir: './sandboxes' });
for (const i of instances) console.log(i.name, i.status.state, i.drift);

const { exitCode, stdout } = await devsandbox.exec(name, ['git', 'status']);

// Your own process or pty (node-pty takes the same file/args): a login shell.
const { file, args } = devsandbox.execArgv(name, [], { interactive: true, tty: true });
spawn(file, args, { stdio: 'inherit' });

await devsandbox.rm(name, { deleteBranch: true });
```

| Function                                   | Resolves to                          |
| ------------------------------------------ | ------------------------------------ |
| `status()`                                 | `Snapshot` (sandboxes, instances, services) |
| `ls()` / `ps({ all })` / `stats()`         | `SandboxRow[]` / `ContainerRow[]` / `StatsRow[]` |
| `service.ls()`                             | `ServiceRow[]`                       |
| `inspect(name)`                            | runtime inspect document (`unknown`) |
| `run(sandbox, { name, branch, base })`     | `RunRecord` (name, container, workspace, folder, worktree, branch) |
| `start` / `stop` / `rebuild(name \| { all: true })` | `void`                      |
| `rm(name, { deleteBranch, force })` / `rename(a, b)` / `done(name)` / `undone(name)` / `gc()` / `service.rebuild(name)` | `void` |
| `logs(name, { lines })`                    | log text                             |
| `exec(name, argv, { input })`              | `{ exitCode, stdout, stderr }` (never rejects on exit code) |
| `execArgv(name, argv?, { tty, interactive })` | `{ file, args }` to spawn yourself (sync; no argv: login shell) |
| `cli(args)`                                | raw `{ exitCode, stdout, stderr }`   |

Every function takes `{ dir, cwd, env, signal, stderr }`, except the synchronous `binaryPath()` and `execArgv()`. `dir` is the config root (`-C`). A failed command rejects with `DevsandboxError` (`exitCode`, `stdout`, `stderr`).

`rm` never prompts: `deleteBranch: true` deletes the worktree branch `run` created (`git branch -D`, unmerged commits too); `false` or omitted keeps it. Branches `run` reused are always kept. It rejects, removing nothing, when the worktree has uncommitted or untracked changes, unless `force: true`: that discards them, and a step that fails is skipped with a warning on stderr so the instance is still removed.

## Platforms

The binary comes from an optional dependency (`@devsandboxes/<platform>-<arch>`), so don't install with `--omit=optional`. On other platforms, `cargo install devsandbox` and point `DEVSANDBOX_BINARY` at the result.
