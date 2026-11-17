// A CHANGELOG entry usually means a user-facing change, and the docs site
// (site/) renders several surfaces from sources that drift silently. Appending
// the reminder to the edit's tool result puts it in front of the agent at the
// moment it records the change.

import { defineExtension } from 'zidane'

const FILE_TOOLS = ['edit', 'multi_edit', 'write_file']

const REMINDER = `<system-reminder>
CHANGELOG.md was edited. Check the docs site (site/) reflects the change:
- Node API: site/src/pages/docs/node-api.astro renders npm/devsandboxes/index.d.ts + README.md; are the typings/JSDoc current?
- Config: site/src/pages/docs/config.astro renders skills/config-toml-spec/SKILL.md; update it for any config field, merge rule or \${…} variable change.
- Examples: site/src/content/examples/*.mdx; does any example use changed flags, config or API?
- Docs pages: site/src/content/docs/**.
If anything changed, run: cd site && pnpm check && pnpm build
</system-reminder>`

function isChangelog(path: unknown): boolean {
  if (typeof path !== 'string') return false
  const p = path.replace(/\\/g, '/')
  return p === 'CHANGELOG.md' || p.endsWith('/CHANGELOG.md')
}

export default defineExtension({
  apiVersion: 1,
  id: 'changelog-site-reminder',
  description: 'Reminds the agent to update the docs site when CHANGELOG.md is edited.',
  setup(ctx) {
    ctx.hooks.intercept(
      'tool.result',
      ({ input }, current) => {
        if (current.isError || !isChangelog(input.path)) return
        const { result } = current
        if (typeof result === 'string') return { ...current, result: `${result}\n${REMINDER}` }
        return { ...current, result: [...result, { type: 'text', text: `\n${REMINDER}` }] }
      },
      { match: FILE_TOOLS },
    )
  },
})
