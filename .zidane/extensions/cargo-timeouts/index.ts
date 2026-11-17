import { defineExtension } from 'zidane'
import { uncoveredCargo } from './detect'

/**
 * cargo-timeouts: refuses `shell` calls that run `cargo` without a `timeout`
 * wrapper. A deadlocked test otherwise hangs the call (or a background task
 * the agent waits on) and the agent stops making progress; the block reason
 * tells it how to rerun the command bounded.
 */

const DEFAULT_EXEMPT = '-V, --version, -h, --help, --list, help, version, fmt, metadata, tree, pkgid, locate-project, search, add, remove'

function exemptSet(raw: string | undefined): Set<string> {
  return new Set((raw ?? DEFAULT_EXEMPT).split(',').map(s => s.trim()).filter(Boolean))
}

export default defineExtension({
  apiVersion: 1,
  id: 'cargo-timeouts',
  displayName: 'Cargo timeouts',
  version: '0.1.0',
  description: 'Makes the agent wrap cargo commands in `timeout` so a hanging test can\'t stall it.',
  settings: {
    items: {
      seconds: {
        type: 'number',
        label: 'Suggested timeout (seconds)',
        description: 'The duration the block message suggests.',
        default: 600,
        min: 10,
        step: 10,
      },
      exempt: {
        type: 'string',
        label: 'Exempt subcommands',
        description: 'Comma-separated cargo subcommands (or info flags) that may run without a timeout.',
        default: DEFAULT_EXEMPT,
      },
    },
  },
  setup(ctx) {
    ctx.hooks.intercept(
      'tool.call',
      (call) => {
        const command = call.input.command
        if (typeof command !== 'string') return undefined
        const opts = { sessionId: call.sessionId }
        const found = uncoveredCargo(command, exemptSet(ctx.settings.get('exempt', opts)))
        if (!found.length) return undefined
        const secs = ctx.settings.get('seconds', opts) ?? 600
        const first = found[0]!.words.join(' ')
        return {
          block: [
            `cargo-timeouts: \`${first}\` has no \`timeout\` around it.`,
            'cargo commands (tests especially) can deadlock or hang and stall you indefinitely.',
            `Rerun with each cargo invocation wrapped, e.g. \`timeout -k 10 ${secs} ${first}\`,`,
            'choosing a bound that fits the command (and set the shell call\'s own `timeout` above it).',
            'If it times out (exit 124), treat it as a hang: find the stuck test instead of retrying with a larger bound.',
          ].join(' '),
        }
      },
      { match: ['shell'] },
    )
  },
})
