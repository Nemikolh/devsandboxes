// Every page's agent prompt, by id (`<AgentPrompt prompt="…" />`). Kept here,
// not in the MDX, so a test can check them all. No `<placeholders>` but
// `<agent>` (filled on copy): the agent works out the specifics itself.
import type { AgentPromptSpec } from './examples';

export const AGENT_PROMPTS = {
  'quick-start': {
    summary: 'A sandbox for your repo, plus the services it needs.',
    goal: 'Set up devsandbox for the repo you are in: a sandbox for it in my devsandboxes.toml, plus the services it needs, such as its database.',
    steps: [
      'If I have no config dir yet, create one outside the repo (a sibling folder named `sandboxes` is fine) holding a `devsandboxes.toml`; a sandbox\'s `folder` is its repo\'s path relative to that dir.',
      'Look at the repo to pick the base image, devcontainer Features and `postCreateCommand` it needs. If it has a `.devcontainer/devcontainer.json`, port that instead.',
      'Put the shared parts in a `[template.base]` the sandbox `extends`. If the repo uses a database, add it as a `[services.*]` entry with `scope = "global"`, listed in the sandbox\'s `services`.',
    ],
    verify: ["Start an instance with `devsandbox run`, run the repo's tests in it with `devsandbox exec`, and report what happened."],
  },
  'parallel-agents': {
    summary: 'One sandbox instance per agent session, each on its own worktree.',
    goal: 'Set up devsandbox so several <agent> sessions can work on the repo you are in, in parallel, each in its own sandbox instance on its own git worktree and branch.',
    steps: [
      "Add a sandbox for the repo to my devsandboxes.toml, or reuse the one whose `folder` already points at it, with an `image` that has the repo's toolchain. Install <agent> in `postCreateCommand` and mount its host config dir into the container.",
      'Put gitignored files every instance needs, like .env, in `worktree-link`.',
    ],
    verify: [
      'Ask me which tasks to start, then start one instance per task with `devsandbox run --name` and a short name for the task, and check `git status` in each shows its own branch.',
    ],
  },
  services: {
    summary: 'One shared Postgres, and a private Redis per instance.',
    goal: 'Give my devsandbox sandboxes one shared Postgres, and every instance its own private Redis.',
    steps: [
      'Add `[services.postgres]` with the Postgres version the repo targets, `scope = "global"` and `POSTGRES_PASSWORD` in `env`. Add `[services.redis]` without a `scope`.',
      'List both in `services` of a template every sandbox extends, with `containerEnv` pointing the variables the repo reads at hosts `postgres` and `redis`.',
    ],
    verify: [
      "Start two instances of the repo's sandbox with `devsandbox run --name`, and check `devsandbox service ls` shows one postgres and a redis per instance.",
    ],
  },
  caches: {
    summary: 'Package caches shared across every sandbox.',
    goal: 'Share package caches across my devsandbox sandboxes so new instances stop re-downloading dependencies.',
    steps: [
      'Find which package managers my sandboxes use and add them to `caches`, in a template every sandbox `extends` if there is one.',
    ],
    verify: [
      'Existing instances now drift: list them with `devsandbox ps -a` and ask me before running `devsandbox rebuild` on each.',
    ],
  },
  browsers: {
    summary: "Browser tests that keep Chromium's sandbox.",
    goal: "Make the browser tests of the repo you are in keep Chromium's sandbox wherever its devsandbox sandbox supports it.",
    steps: [
      'Find where the repo launches a browser. For Playwright, pass `chromiumSandbox: !!process.env.DEVSANDBOX_BROWSER_SANDBOX` and `executablePath: process.env.DEVSANDBOX_CHROME`.',
      "Remove any hand-added `--no-sandbox` flag. Do not change the sandbox's `userns`.",
    ],
    verify: [
      'Start an instance with `devsandbox run`, report its `browser sandbox:` line, then run the browser tests in the instance and report what happened.',
    ],
  },
  'port-a-devcontainer': {
    summary: 'Your devcontainer.json, ported to a sandbox.',
    goal: "Port the devcontainer.json of the repo you are in to a sandbox in my devsandboxes.toml.",
    steps: [
      "Read the repo's `.devcontainer/devcontainer.json` and any Dockerfile or docker-compose.yml it references; do not modify them.",
      'Add a sandbox whose `folder` points at the repo and copy the implemented properties over; rebase `build` paths onto the config dir.',
      'Drop the properties the config-toml-spec lists as ignored and tell me what each did; turn compose services into `[services.*]` listed in `services`.',
    ],
    verify: ['Start an instance with `devsandbox run` and report any `warning:` lines.'],
  },
  credentials: {
    summary: 'gh, git push and sentry in a sandbox, tokens kept on the host.',
    goal: 'Give the devsandbox sandbox of the repo you are in gh, git push and sentry, with the tokens kept on my host.',
    steps: [
      'Add the features `ghcr.io/devsandboxes/features/gh:1` and `ghcr.io/devsandboxes/features/sentry:1` to that sandbox. Add no token, env var or mount for them.',
      'Do not run `devsandbox login` yourself: ask me to run `devsandbox login github --feature ghcr.io/devsandboxes/features/gh:1` and `devsandbox login sentry --feature ghcr.io/devsandboxes/features/sentry:1` on the host, adding `--profile` with the sandbox\'s profile if it has `profiles`, and wait until I have.',
    ],
    verify: [
      'Start an instance with `devsandbox run`; if it asks to approve a claim, let me answer. Check `gh pr list` and `sentry issue list` work in it through `devsandbox exec`. Push only when I ask.',
    ],
  },
} satisfies Record<string, AgentPromptSpec>;

export type AgentPromptId = keyof typeof AGENT_PROMPTS;
