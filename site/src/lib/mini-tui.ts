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
}

export type Event =
  | { type: 'key'; key: string }
  | { type: 'select'; name: Instance }
  | { type: 'tab'; tab: Tab }
  | { type: 'agent'; name: Instance; status: AgentStatus }
  | { type: 'notify' }
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

/** Keys `reduce` knows (the DOM layer only forwards and swallows these). */
export function isHandledKey(key: string): boolean {
  return /^(Arrow(Up|Down|Left|Right)|[jkotq?1-4]|Escape)$/.test(key);
}

/** The keycap (`data-key`) a key press lights up, if the hint bar has one. */
export function keycapFor(key: string): string | null {
  if (key === 'ArrowUp' || key === 'k') return 'ArrowUp';
  if (key === 'ArrowDown' || key === 'j') return 'ArrowDown';
  if (key === 'ArrowLeft' || key === 'ArrowRight' || key === 'Tab') return 'Tab';
  if (key === 'o' || key === 't' || key === '?') return key;
  return null;
}

export const selectedName = (s: State): Instance => INSTANCES[s.selected];

function switchTab(s: State, tab: Tab): State {
  // Like the dashboard, entering the Inbox marks everything read.
  return { ...s, tab, unread: tab === 'inbox' ? 0 : s.unread, message: null };
}

function pressKey(s: State, key: string): State {
  if (s.help) {
    // The help modal swallows everything but its close keys (`esc`/`q`/`?`).
    return key === 'Escape' || key === '?' || key === 'q' ? { ...s, help: false } : s;
  }
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
      if (!onInstances) return s;
      return { ...s, message: `terminal opened in devsandbox-${selectedName(s)}` };
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
      return selected < 0 || s.help ? s : { ...s, tab: 'instances', selected, message: null };
    }
    case 'tab':
      return s.help ? s : switchTab(s, e.tab);
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
  const user = e.type === 'key' || e.type === 'select' || e.type === 'tab';
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
  if (next.tab !== prev.tab) {
    const read = prev.unread > 0 && next.unread === 0 ? ', notifications marked read' : '';
    return `${TAB_LABEL[next.tab]} tab${read}`;
  }
  if (next.selected !== prev.selected) return `${selectedName(next)} selected`;
  return null;
}
