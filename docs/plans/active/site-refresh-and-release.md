# Site refresh and the 0.11.0 release

Status: agreed; building.

## Goal

Cut devsandbox 0.11.0 (74 commits since v0.10.0: the credential broker,
GitHub login, feature updates, host git hardening) with a site that says
what the release does, reviewed with fresh eyes.

Site checks for every step: `cd site && pnpm check && pnpm build`; look at
changed pages with `pnpm shot /<path> --width 1440,420` (PNGs in
`/tmp/site-shots/`, read them back).

## Step 1: examples: credentials in, ssh/git push and babysit out

- Remove `site/src/content/examples/babysit-pr.mdx` and what only it uses
  (`site/src/components/BabysitDiagram.astro`, entries in
  `site/src/lib/examples.ts`, tests in `site/src/lib/examples.test.ts`,
  links from other pages). A *Coming from k8s* page replaces it later
  (not now).
- Replace `site/src/content/examples/ssh-and-git-push.mdx` with
  `credentials.mdx`: gh (and git push) and sentry working in a sandbox
  with no token in it. The config (features `gh:1`, `sentry:1`, YAML
  like the features README), `devsandbox login` per credential (narrow
  PAT page, `--profile`), what the sandbox sees (a phantom), the repo
  fence in two lines, approving a changed claim at `run`, `devsandbox
  access ls`. Source of truth: `docs/reference/credentials-cli.md`,
  `docs/architecture/credentials/{gh,login,broker}.md`, the features
  READMEs (github.com/devsandboxes/features `src/gh`, `src/sentry`).
  Short, example-first, like the other examples.
- Redirect or drop the old slug consistently with how the site handles
  removed pages (check `astro.config`, 404).

## Step 2: agent prompts are a copy button

`site/src/components/AgentPrompt.astro` (built by `buildAgentPrompt` /
`agentPromptItems` in `site/src/lib/examples.ts`; used in
`docs/quick-start.mdx`, `examples/parallel-agents.mdx`, `AgentPromptLink`
in `examples/services.mdx`) shows the prompt text with `<…>`
placeholders to fill in. Instead: a button that copies the prompt; the
text isn't shown on the page. No placeholders: the prompt tells the agent
to work out the specifics (the project, the config dir, sandbox names)
from the repo and devsandbox itself (`devsandbox ls`, the config skill),
asking the user only when it can't.

## Step 3: the configuration page, condensed

`/docs/config` renders `skills/config-toml-spec/SKILL.md` verbatim
(`site/src/pages/docs/config.astro`). Check it against the code
(`src/config.rs` `SandboxProperties`, `Config`, services, profiles,
`updates`, `access`) and cut each property's explanation to 10-100 words
by how much it needs; details live in `docs/` and the per-topic pages.
Keep every field, merge rule and `${…}` variable, and the skill's
frontmatter (agents load it).

## Step 4: fresh-eye site review

A reviewer reads every page against the code and docs of this release
(credentials, feature updates, `devsandbox update`, the gh/zidane/sentry
features, removed `sentry-cli`, no manual `gh-bin` mount) and fixes what's
outdated or wrong; anything bigger is reported, not rewritten.

## Step 5: the release

Per `.agents/skills/release/SKILL.md`, by a reviewer who hasn't seen the
work: `## Unreleased` brought up to date (it misses feature updates and
`devsandbox update`, host git hardening, the rollback of refused runs,
and the commit list), bump `minor`. Then: push, CI green, `RELEASE_SIGNING_KEY`
present, `./release.sh minor`, watch the Release workflow, check the
release body and `SHA256SUMS.sig`. Afterwards the features READMEs say
"needs devsandbox ≥ 0.11".

## Closing

Plan archived.
