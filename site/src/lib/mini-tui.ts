/**
 * The landing page's mini dashboard as a pure state machine: what the "how it
 * works" script renders (`scripts/how-it-works.ts`) and what the autoplay
 * drives. Keys follow the real TUI (`HELP_BODY` in `src/tui/app/view.rs`), with
 * two deliberate differences, both spelled out in the `?` overlay:
 *
 * - Tab is left to the browser so keyboard users can always leave the widget;
 *   ←/→ (and 1-4, like the real thing) switch tabs, and the `tab` keycap still
 *   works when clicked. In the dashboard, ←/→ collapse/expand tree rows.
 * - Esc closes VS Code, so the demo can be replayed; the dashboard can't close
 *   an editor window.
 * - Esc also leaves a focused terminal: browsers deliver `ctrl-]` unreliably
 *   across keyboard layouts and F12 opens devtools, so both still work where
 *   they arrive but Esc is the one the hints teach. Tab is not sent to the
 *   fake shell either, for the same leave-the-widget reason.
 *
 * No DOM here: `reduce` is a plain function over plain data (unit-tested).
 */

export const TABS = ['instances', 'services', 'ports', 'inbox'] as const;
export type Tab = (typeof TABS)[number];

export const INSTANCES = ['web', 'web-2', 'web-3'] as const;
export type Instance = (typeof INSTANCES)[number];

export type AgentStatus = 'editing' | 'testing' | 'idle' | 'waiting';

export const STATUS_LABEL: Record<AgentStatus, string> = {
  editing: 'editing',
  testing: 'running tests',
  idle: 'idle',
  waiting: 'waiting for you',
};

export const TAB_LABEL: Record<Tab, string> = {
  instances: 'Instances',
  services: 'Services',
  ports: 'Ports',
  inbox: 'Inbox',
};

/** Who sends the one notification the Inbox can hold (an agent's `devsbd notify`). */
export const NOTIFY_FROM: Instance = 'web-3';

/** Which part holds the keys, like the real `Focus` (`src/tui/app/mod.rs`). */
export type Focus = 'dashboard' | 'terminal';

/** One integrated-terminal tab: a fake shell in `devsandbox-<name>`. */
export interface Term {
  name: Instance;
  /** Scrollback, oldest first, capped to what the pane shows (`TERM_ROWS`). */
  lines: readonly string[];
  /** The line being typed after the prompt. */
  input: string;
  /** The shell exited: the tab stays readable, `x` closes it (like the real one). */
  exited: boolean;
}

/** Rows the terminal pane shows: scrollback plus the prompt line. */
export const TERM_ROWS = 7;
const INPUT_MAX = 80;

/** Key token for ctrl-] (the DOM layer maps the chord to it). */
export const CTRL_BRACKET = 'ctrl-]';

/** `web` is your checkout; repeat instances are worktrees on `sandbox/<name>`. */
export const BRANCH: Record<Instance, string> = { web: 'main', 'web-2': 'sandbox/web-2', 'web-3': 'sandbox/web-3' };

/** What each agent has touched (web-3's agent is done and committed). */
const CHANGED: Record<Instance, string | null> = { web: 'src/app.ts', 'web-2': 'src/api/users.ts', 'web-3': null };

/** Debian's default root prompt, in the workspace the Detail box shows. */
export const promptFor = (name: Instance): string => `root@devsandbox-${name}:/workspaces/${name}# `;

export interface State {
  /** Index into `INSTANCES` (the Instances table cursor). */
  selected: number;
  tab: Tab;
  /** Instance VS Code is attached to, or null when closed. */
  vscode: Instance | null;
  /** Whether the notification has arrived in the Inbox. */
  notified: boolean;
  unread: number;
  agents: Record<Instance, AgentStatus>;
  help: boolean;
  /** Transient status-line message (the result of `o` / `t`). */
  message: string | null;
  /** Next `TIMELINE` step; -1 once the user took over. */
  step: number;
  /** Open terminals, in open order (the tab strip). */
  terms: readonly Term[];
  /** Index into `terms` of the shown one. */
  active: number;
  focus: Focus;
}

export type Event =
  | { type: 'key'; key: string }
  | { type: 'select'; name: Instance }
  | { type: 'tab'; tab: Tab }
  | { type: 'agent'; name: Instance; status: AgentStatus }
  | { type: 'notify' }
  /** A click on the terminal pane (optionally a tab in its title) or away from it. */
  | { type: 'focus'; focus: Focus; term?: number }
  | { type: 'tick' };

/** What the server-rendered markup shows (no JS): `web` selected, one unread. */
export const STATIC: State = {
  selected: 0,
  tab: 'instances',
  vscode: null,
  notified: true,
  unread: 1,
  agents: { web: 'editing', 'web-2': 'testing', 'web-3': 'idle' },
  help: false,
  message: null,
  step: -1,
  terms: [],
  active: 0,
  focus: 'dashboard',
};

/** Where the autoplay starts: web-3's agent still busy, nothing in the Inbox yet. */
export const INITIAL: State = {
  ...STATIC,
  notified: false,
  unread: 0,
  agents: { web: 'editing', 'web-2': 'testing', 'web-3': 'testing' },
  step: 0,
};

/**
 * The autoplay story, one step per `tick`; `after` is the pause (ms) before
 * the step. web-3's agent finishes and notifies, web-2's agent waits for you,
 * you move to web-2 and press `o`: VS Code opens on it (≈ 3.5 s in), then
 * the agents carry on and it settles.
 */
export const TIMELINE: readonly { after: number; event: Event }[] = [
  { after: 700, event: { type: 'agent', name: 'web-3', status: 'idle' } },
  { after: 500, event: { type: 'notify' } },
  { after: 700, event: { type: 'agent', name: 'web-2', status: 'waiting' } },
  { after: 700, event: { type: 'key', key: 'j' } },
  { after: 900, event: { type: 'key', key: 'o' } },
  { after: 1200, event: { type: 'agent', name: 'web-2', status: 'editing' } },
  { after: 800, event: { type: 'agent', name: 'web', status: 'testing' } },
];

/**
 * Keys `reduce` knows in state `s` (the DOM layer only forwards and swallows
 * these). A focused terminal takes every key but Tab, so typing never scrolls
 * the page or triggers the dashboard; modified keys are filtered by the DOM
 * layer before this (apart from ctrl-]).
 */
export function isHandledKey(s: State, key: string): boolean {
  if (s.focus === 'terminal') return key !== 'Tab';
  return /^(Arrow(Up|Down|Left|Right)|[jkotqx?1-4[\]]|Escape|F12)$/.test(key) || key === CTRL_BRACKET;
}

/** The keycap (`data-key`) a key press lights up, if a hint bar has one. */
export function keycapFor(key: string): string | null {
  if (key === 'ArrowUp' || key === 'k') return 'ArrowUp';
  if (key === 'ArrowDown' || key === 'j') return 'ArrowDown';
  if (key === 'ArrowLeft' || key === 'ArrowRight' || key === 'Tab') return 'Tab';
  if (key === 'Escape' || key === 'F12' || key === CTRL_BRACKET) return 'Escape';
  if (key === 'o' || key === 't' || key === 'x' || key === '[' || key === ']' || key === '?') return key;
  return null;
}

export const selectedName = (s: State): Instance => INSTANCES[s.selected];

export const activeTerm = (s: State): Term | undefined => s.terms[s.active];

/** The fake shell's answer to one command line. */
export function shell(name: Instance, line: string): { lines: string[]; clear?: true; exit?: true } {
  const cmd = line.trim().split(/\s+/).join(' ');
  const changed = CHANGED[name];
  switch (cmd) {
    case '':
      return { lines: [] };
    case 'git status':
      return {
        lines: [
          `On branch ${BRANCH[name]}`,
          ...(changed
            ? ['Changes not staged for commit:', `        modified:   ${changed}`]
            : ['nothing to commit, working tree clean']),
        ],
      };
    case 'ls':
      return { lines: ['README.md  node_modules  package.json  src  tests'] };
    case 'pwd':
      return { lines: [`/workspaces/${name}`] };
    case 'whoami':
      return { lines: ['root'] };
    case 'help':
      return { lines: ['this demo shell knows: git status, ls, pwd, whoami, clear, exit'] };
    case 'clear':
      return { lines: [], clear: true };
    case 'exit':
    case 'logout':
      // A login shell (`bash -l`, what the dashboard spawns) says `logout`.
      return { lines: ['logout'], exit: true };
  }
  const [first, sub] = cmd.split(' ');
  if (first === 'git') {
    return { lines: [sub ? `git: '${sub}' is not in this demo, try git status` : 'usage: git <command> [<args>]'] };
  }
  return { lines: [`bash: ${first}: command not found`] };
}

/** `t`: focus the live terminal for the selection, or open one (Instances only). */
function openTerm(s: State): State {
  if (s.tab !== 'instances') return s;
  const name = selectedName(s);
  // Like `TermTabs::find`: an exited tab doesn't count, `t` opens a fresh one.
  const existing = s.terms.findIndex((t) => t.name === name && !t.exited);
  if (existing >= 0) return { ...s, active: existing, focus: 'terminal', message: null };
  const term: Term = { name, lines: [], input: '', exited: false };
  return { ...s, terms: [...s.terms, term], active: s.terms.length, focus: 'terminal', message: null };
}

/** `x`: drop the active terminal; the next one shifts into its place. */
function closeTerm(s: State): State {
  if (s.terms.length === 0) return s;
  const terms = s.terms.filter((_, i) => i !== s.active);
  const active = Math.min(s.active, Math.max(0, terms.length - 1));
  return { ...s, terms, active, focus: terms.length > 0 ? s.focus : 'dashboard' };
}

function updateTerm(s: State, change: Partial<Term>): State {
  return { ...s, terms: s.terms.map((t, i) => (i === s.active ? { ...t, ...change } : t)) };
}

/** A key while the terminal has focus (`on_key_terminal` in `src/tui/app/terminal.rs`). */
function terminalKey(s: State, key: string): State {
  if (key === 'Escape' || key === 'F12' || key === CTRL_BRACKET) return { ...s, focus: 'dashboard' };
  const term = activeTerm(s);
  if (!term) return { ...s, focus: 'dashboard' };
  // An exited shell swallows keys; `x` closes it in place.
  if (term.exited) return key === 'x' ? closeTerm(s) : s;
  if (key === 'Backspace') return term.input ? updateTerm(s, { input: term.input.slice(0, -1) }) : s;
  if (key === 'Enter') {
    const out = shell(term.name, term.input);
    const lines = out.clear ? [] : [...term.lines, promptFor(term.name) + term.input, ...out.lines];
    return updateTerm(s, { lines: lines.slice(-(TERM_ROWS - 1)), input: '', exited: Boolean(out.exit) });
  }
  // Printable characters only (named keys like `Shift` or `F5` are swallowed).
  if ([...key].length === 1 && term.input.length < INPUT_MAX) return updateTerm(s, { input: term.input + key });
  return s;
}

function switchTab(s: State, tab: Tab): State {
  // Like the dashboard, entering the Inbox marks everything read.
  return { ...s, tab, unread: tab === 'inbox' ? 0 : s.unread, message: null };
}

function pressKey(s: State, key: string): State {
  if (s.help) {
    // The help modal swallows everything but its close keys (`esc`/`q`/`?`).
    return key === 'Escape' || key === '?' || key === 'q' ? { ...s, help: false } : s;
  }
  if (s.focus === 'terminal') return terminalKey(s, key);
  const i = TABS.indexOf(s.tab);
  const onInstances = s.tab === 'instances';
  switch (key) {
    case 'ArrowUp':
    case 'k':
      return onInstances ? { ...s, selected: Math.max(0, s.selected - 1), message: null } : s;
    case 'ArrowDown':
    case 'j':
      return onInstances ? { ...s, selected: Math.min(INSTANCES.length - 1, s.selected + 1), message: null } : s;
    case 'Tab':
    case 'ArrowRight':
      return switchTab(s, TABS[(i + 1) % TABS.length]);
    case 'ArrowLeft':
      return switchTab(s, TABS[(i - 1 + TABS.length) % TABS.length]);
    case '1':
    case '2':
    case '3':
    case '4':
      return switchTab(s, TABS[Number(key) - 1]);
    case 'o':
      if (!onInstances) return s;
      return { ...s, vscode: selectedName(s), message: `VS Code opened on ${selectedName(s)}` };
    case 't':
      return openTerm(s);
    case 'x':
      return closeTerm(s);
    case ']':
      return s.terms.length ? { ...s, active: (s.active + 1) % s.terms.length } : s;
    case '[':
      return s.terms.length ? { ...s, active: (s.active + s.terms.length - 1) % s.terms.length } : s;
    case 'F12':
    case CTRL_BRACKET:
      return s.terms.length ? { ...s, focus: 'terminal', message: null } : { ...s, message: 'no terminal open — press t' };
    case 'Escape':
      return s.vscode ? { ...s, vscode: null, message: 'VS Code closed' } : s;
    case '?':
      return { ...s, help: true };
    default:
      return s;
  }
}

function apply(s: State, e: Event): State {
  switch (e.type) {
    case 'key':
      return pressKey(s, e.key);
    case 'select': {
      const selected = INSTANCES.indexOf(e.name);
      return selected < 0 || s.help ? s : { ...s, tab: 'instances', selected, message: null, focus: 'dashboard' };
    }
    case 'tab':
      return s.help ? s : { ...switchTab(s, e.tab), focus: 'dashboard' };
    case 'focus': {
      if (s.help || (e.focus === 'terminal' && s.terms.length === 0)) return s;
      const active = e.term !== undefined && e.term >= 0 && e.term < s.terms.length ? e.term : s.active;
      return e.focus === s.focus && active === s.active ? s : { ...s, focus: e.focus, active };
    }
    case 'agent':
      return { ...s, agents: { ...s.agents, [e.name]: e.status } };
    case 'notify':
      return s.notified ? s : { ...s, notified: true, unread: s.tab === 'inbox' ? 0 : s.unread + 1 };
    case 'tick':
      return s;
  }
}

/**
 * One transition. `tick` plays the next `TIMELINE` step (a no-op once it's
 * done or the user took over); anything the user does ends the autoplay.
 */
export function reduce(s: State, e: Event): State {
  if (e.type === 'tick') {
    const step = TIMELINE[s.step];
    return step ? { ...apply(s, step.event), step: s.step + 1 } : s;
  }
  const user = e.type === 'key' || e.type === 'select' || e.type === 'tab' || e.type === 'focus';
  const next = apply(s, e);
  return user && s.step !== -1 ? { ...next, step: -1 } : next;
}

/** True while the autoplay has steps left to play. */
export const playing = (s: State): boolean => s.step >= 0 && s.step < TIMELINE.length;

/** The end of the autoplay, shown as-is under `prefers-reduced-motion`. */
export function settled(): State {
  let s = INITIAL;
  while (playing(s)) s = reduce(s, { type: 'tick' });
  return { ...s, message: null, step: -1 };
}

/** A short, polite announcement of what a user action changed (or null). */
export function describe(prev: State, next: State): string | null {
  if (next.help !== prev.help) return next.help ? 'Help open. Escape or ? closes it.' : 'Help closed';
  if (next.message && next.message !== prev.message) return next.message;
  const term = activeTerm(next);
  if (next.terms.length < prev.terms.length) return next.terms.length ? 'Terminal closed' : 'Terminal closed, none left';
  if (next.focus !== prev.focus) {
    return next.focus === 'terminal' && term
      ? `Terminal ${next.active + 1} on ${term.name} focused, keys go to the shell. Escape returns to the dashboard.`
      : 'Dashboard focused';
  }
  if (term && next.active !== prev.active) return `Terminal ${next.active + 1} on ${term.name}`;
  const was = activeTerm(prev);
  if (term && was && term !== was && term.lines !== was.lines) {
    if (term.exited) return 'Shell exited. x closes the terminal.';
    if (term.lines.length === 0) return 'Screen cleared';
    // What the last command printed: everything after its echoed prompt line.
    const prompt = promptFor(term.name);
    let at = term.lines.length - 1;
    while (at >= 0 && !term.lines[at].startsWith(prompt)) at--;
    const output = term.lines.slice(at + 1).join('. ');
    return output || null;
  }
  if (next.tab !== prev.tab) {
    const read = prev.unread > 0 && next.unread === 0 ? ', notifications marked read' : '';
    return `${TAB_LABEL[next.tab]} tab${read}`;
  }
  if (next.selected !== prev.selected) return `${selectedName(next)} selected`;
  return null;
}
