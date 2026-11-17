/**
 * Finds `cargo` invocations in a shell command line that no `timeout` wraps.
 *
 * Deliberately a small shell lexer, not a parser: it splits on control
 * operators, skips env assignments and transparent wrappers, and recurses
 * into `sh -c '…'` / `$( … )` bodies. Good enough for the commands an agent
 * writes; anything it can't follow is treated as uncovered (the safe side).
 */

/** Tokens: words, or a control operator that ends a simple command. */
type Token = { kind: 'word'; text: string } | { kind: 'op' }

const OPS = ['&&', '||', ';;', '|&', ';', '|', '&', '\n', '(', ')', '{', '}']

function lex(src: string): Token[] {
  const out: Token[] = []
  let word = ''
  let inWord = false
  const flush = () => {
    if (inWord) out.push({ kind: 'word', text: word })
    word = ''
    inWord = false
  }
  let i = 0
  while (i < src.length) {
    const c = src[i]!
    if (c === '\\' && i + 1 < src.length) {
      if (src[i + 1] !== '\n') {
        word += src[i + 1]
        inWord = true
      }
      i += 2
      continue
    }
    if (c === "'") {
      const end = src.indexOf("'", i + 1)
      const stop = end === -1 ? src.length : end
      word += src.slice(i + 1, stop)
      inWord = true
      i = stop + 1
      continue
    }
    if (c === '"') {
      let j = i + 1
      while (j < src.length && src[j] !== '"') {
        if (src[j] === '\\' && j + 1 < src.length) {
          word += src[j + 1]
          j += 2
          continue
        }
        word += src[j]
        j++
      }
      inWord = true
      i = j + 1
      continue
    }
    if (c === '#' && !inWord) {
      const nl = src.indexOf('\n', i)
      i = nl === -1 ? src.length : nl
      continue
    }
    if (c === ' ' || c === '\t' || c === '\r') {
      flush()
      i++
      continue
    }
    // `$(` and backticks open a nested command: treat them as boundaries.
    if (c === '`' || (c === '$' && src[i + 1] === '(')) {
      flush()
      out.push({ kind: 'op' })
      i += c === '`' ? 1 : 2
      continue
    }
    // `2>&1`, `>&2`, `<&0`: a redirection's `&`, not a control operator.
    if (c === '&' && /[<>]$/.test(word)) {
      word += c
      i++
      continue
    }
    const op = OPS.find(o => src.startsWith(o, i))
    if (op) {
      flush()
      out.push({ kind: 'op' })
      i += op.length
      continue
    }
    word += c
    inWord = true
    i++
  }
  flush()
  return out
}

const ASSIGNMENT = /^[A-Za-z_][A-Za-z0-9_]*=/
const SHELLS = new Set(['sh', 'bash', 'zsh', 'dash', 'ksh'])
/** Wrappers that run their argument as the command; their own flags are skipped. */
const WRAPPERS = new Set(['env', 'time', 'nice', 'nohup', 'exec', 'command', 'sudo', 'stdbuf', 'ionice', 'xargs', 'unbuffer'])
const TIMEOUTS = new Set(['timeout', 'gtimeout'])
/** Reserved words that may precede a command on the same line. */
const KEYWORDS = new Set(['then', 'do', 'else', 'elif', 'if', 'while', 'until', '!'])

/** Global flags that make cargo print and exit; reported as the subcommand. */
const INFO_FLAGS = new Set(['-V', '--version', '-h', '--help', '--list'])

const base = (w: string) => w.slice(w.lastIndexOf('/') + 1)

/** A `cargo` call found without a timeout around it. */
export interface Uncovered {
  /** The cargo subcommand (`test`, `run`, …), `''` for a bare `cargo`. */
  subcommand: string
  /** The words of the call, from `cargo` on. */
  words: string[]
}

/** The cargo subcommand: the first word after `cargo` that isn't a `+toolchain` or a flag. */
function subcommandOf(args: string[]): string {
  for (let i = 0; i < args.length; i++) {
    const a = args[i]!
    if (a.startsWith('+')) continue
    // Global flags that take a value.
    if (['-C', '--config', '-Z', '--manifest-path', '--color'].includes(a)) {
      i++
      continue
    }
    if (INFO_FLAGS.has(a)) return a
    if (a.startsWith('-')) continue
    return a
  }
  return ''
}

function scanCommand(words: string[], covered: boolean, found: Uncovered[]): void {
  let i = 0
  while (i < words.length) {
    const w = words[i]!
    const name = base(w)
    if (ASSIGNMENT.test(w) || KEYWORDS.has(w)) {
      i++
      continue
    }
    if (TIMEOUTS.has(name)) {
      covered = true
      i++
      // Options, `-k DUR` / `-s SIG` taking a value, then the duration.
      while (i < words.length && words[i]!.startsWith('-')) {
        const opt = words[i]!
        i += opt === '-k' || opt === '-s' || opt === '--kill-after' || opt === '--signal' ? 2 : 1
      }
      i++
      continue
    }
    if (WRAPPERS.has(name)) {
      i++
      while (i < words.length && (words[i]!.startsWith('-') || (name === 'env' && ASSIGNMENT.test(words[i]!)))) {
        // `nice -n 5`, `stdbuf -o L`: short options with a separate value.
        const opt = words[i]!
        i += /^-[nuoeiI]$/.test(opt) && name !== 'env' ? 2 : 1
      }
      continue
    }
    if (SHELLS.has(name)) {
      const c = words.indexOf('-c', i + 1)
      if (c !== -1 && c + 1 < words.length) scan(words[c + 1]!, covered, found)
      return
    }
    if (name === 'cargo') {
      if (!covered) found.push({ subcommand: subcommandOf(words.slice(i + 1)), words: words.slice(i) })
      return
    }
    return
  }
}

/** Every `cargo` call in `src` with no `timeout` around it. */
function scan(src: string, covered: boolean, found: Uncovered[]): void {
  let words: string[] = []
  for (const t of [...lex(src), { kind: 'op' } as Token]) {
    if (t.kind === 'word') {
      words.push(t.text)
      continue
    }
    if (words.length) scanCommand(words, covered, found)
    words = []
  }
}

export function uncoveredCargo(src: string, exempt: ReadonlySet<string>): Uncovered[] {
  const found: Uncovered[] = []
  scan(src, false, found)
  return found.filter(u => !exempt.has(u.subcommand))
}
