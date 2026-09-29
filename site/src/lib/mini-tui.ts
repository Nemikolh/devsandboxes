/**
 * The mini dashboard as a pure state machine: what the widget driver renders
 * (`scripts/mini-tui.ts`, markup `components/MiniTui.astro`) and what the
 * autoplay drives. Keys follow the real TUI (`on_key` in `src/tui/app/mod.rs`,
 * `HELP_BODY` in `src/tui/app/view.rs`), with deliberate differences, all
 * spelled out in the `?` overlay:
 *
 * - Tab is left to the browser so keyboard users can always leave the widget;
 *   ←/→ (and 1-4, like the real thing) switch tabs, and the `tab` keycap still
 *   works when clicked. The one exception is the open `:` prompt, an explicit
 *   mode where Tab completes; Esc closes it first.
 * - Esc closes VS Code, so the demo can be replayed; the dashboard can't close
 *   an editor window.
 * - Esc also leaves a focused terminal: browsers deliver `ctrl-]` unreliably
 *   across keyboard layouts and F12 opens devtools, so both still work where
 *   they arrive but Esc is the one the hints teach. Tab is not sent to the
 *   fake shell either, for the same leave-the-widget reason.
 * - The sandbox row (`▾ web`) is selected by clicking it; ↑ stops at the first
 *   instance. `r` there prefills `run web `, like the real one.
 * - Background work (`s`, forwards) lands at once, with the message the real
 *   one shows when it finishes; commands that suspend the real dashboard
 *   (`run`, `rm`, `stop`, `start`, `exec`) show their output in a "suspended"
 *   overlay until a key is pressed, as the real one waits for one.
 *
 * No DOM here: `reduce` is a plain function over plain data (unit-tested).
 */

import {
  CONFIG_HASH,
  instanceInspect,
  logLines,
  SANDBOX,
  SANDBOX_ORIGINAL,
  SANDBOX_RESOLVED,
  SERVICE,
  SERVICE_CONTAINER,
  SERVICE_TABLE,
  serviceInspect,
  devServerPid,
} from './mini-tui-content';
import {
  type Action,
  backspace,
  candidatesFor,
  complete,
  newPrompt,
  parseLine,
  type Prompt,
  typeChar,
} from './mini-tui-spec';

export { SANDBOX, SERVICE } from './mini-tui-content';
export type { Prompt } from './mini-tui-spec';

export const TABS = ['instances', 'services', 'ports', 'inbox'] as const;
export type Tab = (typeof TABS)[number];

/** The landing story's instances (the diagram has a card for each). `:run` can add more. */
export const INSTANCES = ['web', 'web-2', 'web-3'] as const;
export type Instance = string;

export type AgentStatus = 'editing' | 'testing' | 'idle' | 'waiting' | 'stopped';

export const STATUS_LABEL: Record<AgentStatus, string> = {
  editing: 'editing',
  testing: 'running tests',
  idle: 'idle',
  waiting: 'waiting for you',
  stopped: 'stopped',
};

export const TAB_LABEL: Record<Tab, string> = {
  instances: 'Instances',
  services: 'Services',
  ports: 'Ports',
  inbox: 'Inbox',
};

/** Who sends the one notification the Inbox can hold (an agent's `devsbd notify`). */
export const NOTIFY_FROM: Instance = 'web-3';

/** Container status, labelled like `ContainerStatus::label` (`src/snapshot.rs`). */
export type RunState = 'running' | 'exited';

/** One Instances-tab row. */
export interface Inst {
  name: string;
  status: RunState;
  /** A repeat instance: a git worktree on `sandbox/<name>` (the first one mounts `../web`). */
  worktree: boolean;
  /** The UPTIME column (`humanize_secs` of the time since `run`). */
  uptime: string;
}

export const BASE_INSTANCES: readonly Inst[] = [
  { name: 'web', status: 'running', worktree: false, uptime: '2h05m' },
  { name: 'web-2', status: 'running', worktree: true, uptime: '48m' },
  { name: 'web-3', status: 'running', worktree: true, uptime: '12m' },
];

/** `web` is your checkout; repeat instances are worktrees on `sandbox/<name>`. */
export const branchOf = (i: Inst): string => (i.worktree ? `sandbox/${i.name}` : 'main');

/** Where the instance's files live, relative to the config dir (worktrees under `.worktrees/`). */
export const folderOf = (i: Inst): string => (i.worktree ? `.worktrees/${i.name}` : '../web');

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

/** One Ports-tab row (`PortRow` in `src/tui/app/mod.rs`). */
export interface Forward {
  id: number;
  /** Host side, `127.0.0.1:3000`. */
  local: string;
  /** Route label, `web-2:3000` or `postgres:5432 (via instance web)`. */
  target: string;
  /** Listening process, when known (`-` in the table otherwise). */
  process: string | null;
  /** `active`, or `error: …` while its instance is stopped. */
  state: string;
  conns: number;
  instance: Instance;
}

/** The two full-body views (`Modal::Logs` / `Modal::Config`); help stays its own flag. */
export type Modal =
  | { kind: 'logs'; name: Instance; scroll: number }
  | {
      kind: 'config';
      /** The sandbox's config (Instances tab) or the service's (Services tab). */
      target: 'sandbox' | 'service';
      side: 'original' | 'resolved';
      scroll: number;
      /** Container the inspect pane shows, or null (`(no running instance)`). */
      container: string | null;
    };

/**
 * A command that suspends the real dashboard: it leaves the alternate screen,
 * the command prints, then "press any key to return" (`run_suspended` in
 * `src/tui/mod.rs`). `confirm` is `rm`'s branch question, answered first.
 */
export interface Suspended {
  lines: readonly string[];
  confirm: { instance: Instance; branch: string } | null;
}

/** What each agent has touched (web-3's agent is done and committed). */
const CHANGED: Record<string, string> = { web: 'src/app.ts', 'web-2': 'src/api/users.ts' };

/** Debian's default root prompt, in the workspace the Detail box shows. */
export const promptFor = (name: Instance): string => `root@devsandbox-${name}:/workspaces/${name}# `;

export interface State {
  instances: readonly Inst[];
  /** Index into `instances` (the Instances table cursor); `SANDBOX_ROW` is the `▾ web` row. */
  selected: number;
  tab: Tab;
  /** Instance VS Code is attached to, or null when closed. */
  vscode: Instance | null;
  /** Whether the notification has arrived in the Inbox. */
  notified: boolean;
  unread: number;
  agents: Record<Instance, AgentStatus>;
  help: boolean;
  /** Transient status-line message (the result of `o` / `t` / `s` / a forward). */
  message: string | null;
  /** Next `TIMELINE` step; -1 once the user took over. */
  step: number;
  /** Open terminals, in open order (the tab strip). */
  terms: readonly Term[];
  /** Index into `terms` of the shown one. */
  active: number;
  focus: Focus;
  /** The `:` command line, when open (it takes the hint bar's place). */
  prompt: Prompt | null;
  modal: Modal | null;
  suspended: Suspended | null;
  ports: readonly Forward[];
  /** Ports-tab cursor. */
  portSel: number;
  /** Next forward id. */
  nextPort: number;
}

/** `selected` for the sandbox row. */
export const SANDBOX_ROW = -1;

export type Event =
  | { type: 'key'; key: string }
  | { type: 'select'; name: Instance }
  /** A click on the `▾ web` sandbox row. */
  | { type: 'selectSandbox' }
  /** A click on a Ports-tab row. */
  | { type: 'selectPort'; index: number }
  | { type: 'tab'; tab: Tab }
  | { type: 'agent'; name: Instance; status: AgentStatus }
  | { type: 'notify' }
  /** A click on the terminal pane (optionally a tab in its title) or away from it. */
  | { type: 'focus'; focus: Focus; term?: number }
  | { type: 'tick' };

/** What the server-rendered markup shows (no JS): `web` selected, one unread. */
export const STATIC: State = {
  instances: BASE_INSTANCES,
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
  prompt: null,
  modal: null,
  suspended: null,
  ports: [],
  portSel: 0,
  nextPort: 1,
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

const DASHBOARD_KEYS = /^(Arrow(Up|Down|Left|Right)|[jkotqx?1-4[\]:prlsde]|Enter|Escape|F12)$/;
const VIEW_KEYS = /^(Arrow(Up|Down)|Page(Up|Down)|[jkgGtq?]|Escape)$/;

/**
 * Keys `reduce` knows in state `s` (the DOM layer only forwards and swallows
 * these). A focused terminal takes every key but Tab, so typing never scrolls
 * the page or triggers the dashboard; the open prompt takes every key,
 * Tab included (it completes; Esc closes the prompt first); a suspended
 * command takes any key but Tab. Modified keys are filtered by the DOM layer
 * before this (apart from ctrl-]).
 */
export function isHandledKey(s: State, key: string): boolean {
  if (s.suspended) return key !== 'Tab';
  if (s.prompt) return true;
  if (s.help || s.modal) return VIEW_KEYS.test(key);
  if (s.focus === 'terminal') return key !== 'Tab';
  return DASHBOARD_KEYS.test(key) || key === CTRL_BRACKET;
}

/** The keycap (`data-key`) a key press lights up, if a hint bar has one. */
export function keycapFor(key: string): string | null {
  if (key === 'ArrowUp' || key === 'k') return 'ArrowUp';
  if (key === 'ArrowDown' || key === 'j') return 'ArrowDown';
  if (key === 'ArrowLeft' || key === 'ArrowRight' || key === 'Tab') return 'Tab';
  if (key === 'Escape' || key === 'F12' || key === CTRL_BRACKET) return 'Escape';
  if ('otx[]?:srlpdegG'.includes(key) && key.length === 1) return key;
  return null;
}

export const selectedInst = (s: State): Inst | undefined => s.instances[s.selected];

/** The instance under the cursor, or null on the sandbox row. */
export const selectedName = (s: State): Instance | null => selectedInst(s)?.name ?? null;

export const activeTerm = (s: State): Term | undefined => s.terms[s.active];

const findInst = (s: State, name: string): Inst | undefined => s.instances.find((i) => i.name === name);

const branchFor = (s: State, name: Instance): string => {
  const inst = findInst(s, name);
  return inst ? branchOf(inst) : `sandbox/${name}`;
};

/** The fake shell's answer to one command line. */
export function shell(
  name: Instance,
  line: string,
  branch = name === SANDBOX ? 'main' : `sandbox/${name}`,
): { lines: string[]; clear?: true; exit?: true } {
  const cmd = line.trim().split(/\s+/).join(' ');
  const changed = CHANGED[name];
  switch (cmd) {
    case '':
      return { lines: [] };
    case 'git status':
      return {
        lines: [
          `On branch ${branch}`,
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
  const inst = selectedInst(s);
  // `term_target` in `src/tui/app/terminal.rs`.
  if (!inst) return { ...s, message: 'terminal: select an instance' };
  if (inst.status !== 'running') return { ...s, message: `terminal: \`${inst.name}\` is not running` };
  const name = inst.name;
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
    const out = shell(term.name, term.input, branchFor(s, term.name));
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

// ---- Instances: run / stop / start / rm, shared by the keys and the prompt.

/** `default_instance_name` in `src/commands/run/mod.rs`: the sandbox name, else the first free `web-<n>`. */
export function defaultInstanceName(s: State, sandbox: string): string {
  if (!findInst(s, sandbox)) return sandbox;
  for (let n = 2; ; n++) if (!findInst(s, `${sandbox}-${n}`)) return `${sandbox}-${n}`;
}

/** Instances stay sorted by name, like state's map; the cursor stays on the same row. */
function withInstances(s: State, instances: Inst[]): State {
  const sorted = [...instances].sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
  const was = selectedName(s);
  let selected = was === null ? SANDBOX_ROW : sorted.findIndex((i) => i.name === was);
  if (selected < 0) selected = Math.min(s.selected, sorted.length - 1);
  return { ...s, instances: sorted, selected };
}

/** Stop or start `name`: its row, its agent, the terminals and forwards riding on it. */
function setRunState(s: State, name: Instance, status: RunState): State {
  const next = withInstances(
    s,
    s.instances.map((i) => (i.name === name ? { ...i, status } : i)),
  );
  const running = status === 'running';
  return {
    ...next,
    // A stopped container has no agent; a started one has an idle shell, no agent yet.
    agents: { ...s.agents, [name]: running ? 'idle' : 'stopped' },
    // The `docker exec` behind a terminal dies with its container.
    terms: running ? s.terms : s.terms.map((t) => (t.name === name ? { ...t, exited: true } : t)),
    ports: s.ports.map((p) => (p.instance === name ? { ...p, state: forwardState(name, status) } : p)),
  };
}

const forwardState = (name: Instance, status: RunState): string =>
  status === 'running' ? 'active' : `error: instance \`${name}\` is not running (devsandbox start ${name})`;

function removeInstance(s: State, name: Instance): State {
  const next = withInstances(
    s,
    s.instances.filter((i) => i.name !== name),
  );
  const ports = s.ports.filter((p) => p.instance !== name);
  return {
    ...next,
    vscode: s.vscode === name ? null : s.vscode,
    terms: s.terms.map((t) => (t.name === name ? { ...t, exited: true } : t)),
    ports,
    portSel: Math.min(s.portSel, Math.max(0, ports.length - 1)),
  };
}

/** Where the real one logs a failed suspended command (`log_error`). */
const errorLines = (verb: string, e: string): string[] => [
  `error: ${e}`,
  `logged to /home/you/.local/share/devsandbox/logs/${verb}-1790690400.log`,
];

const noMatch = (name: string) => `no sandbox instance matches \`${name}\` (see \`devsandbox ps -a\`)`;

const suspend = (s: State, lines: string[]): State => ({ ...s, suspended: { lines, confirm: null } });

/** `run <sandbox>`: a new instance; a worktree when the base checkout is taken. */
function runInstance(s: State, a: Extract<Action, { cmd: 'run' }>): State {
  if (a.sandbox !== SANDBOX) return suspend(s, errorLines('run', `unknown sandbox \`${a.sandbox}\``));
  const name = a.name ?? defaultInstanceName(s, a.sandbox);
  if (findInst(s, name)) {
    return suspend(
      s,
      errorLines(
        'run',
        `instance \`${name}\` already exists; \`devsandbox start ${name}\` restarts it, \`devsandbox rm ${name}\` frees the name`,
      ),
    );
  }
  const worktree = s.instances.some((i) => !i.worktree);
  const branch = a.branch?.replaceAll('${instance}', name) ?? `sandbox/${name}`;
  const lines = worktree
    ? [`Preparing worktree (new branch '${branch}')`, 'HEAD is now at 3f9c21a feat: users api', name]
    : [name];
  const next = withInstances(s, [...s.instances, { name, status: 'running', worktree, uptime: '0s' }]);
  return suspend({ ...next, agents: { ...s.agents, [name]: 'idle' } }, lines);
}

/** `rm`: a worktree instance asks about its branch first (`rm` in `src/commands/rm.rs`). */
function rmInstance(s: State, name: Instance): State {
  const inst = findInst(s, name);
  if (!inst) return suspend(s, errorLines('rm', noMatch(name)));
  if (inst.worktree) return { ...s, suspended: { lines: [], confirm: { instance: name, branch: branchOf(inst) } } };
  return suspend(removeInstance(s, name), [`removed ${name}`]);
}

export const confirmQuestion = (branch: string): string => `delete branch \`${branch}\`? [y/N] `;

/** A key on the suspended screen: `rm`'s y/N first, then any key returns. */
function suspendedKey(s: State, key: string): State {
  const sus = s.suspended;
  if (!sus) return s;
  if (!sus.confirm) return { ...s, suspended: null };
  const { instance, branch } = sus.confirm;
  const yes = key === 'y' || key === 'Y';
  if (!yes && key !== 'n' && key !== 'N' && key !== 'Enter') return s;
  const answer = key === 'Enter' ? '' : key;
  const lines = [
    ...sus.lines,
    confirmQuestion(branch) + answer,
    ...(yes ? [`Deleted branch ${branch} (was 3f9c21a).`] : []),
    `removed ${instance}`,
  ];
  return { ...removeInstance(s, instance), suspended: { lines, confirm: null } };
}

// ---- Ports.

const localPort = (f: Forward): number => Number(f.local.slice(f.local.lastIndexOf(':') + 1));

/** How many ports a `<port>` spec tries upwards (`HostPort::Prefer`). */
const PREFER_TRIES = 10;

/**
 * A `port` request, as the forwarder worker handles it (`Forwards::start` in
 * `src/tui/forwards.rs`): same host port or the next free one up for `3000`,
 * exactly that one for `8080:3000`; the status line says what it bound.
 */
function addForward(s: State, a: Extract<Action, { cmd: 'port' }>): State {
  // Submitting switches to the Ports tab whatever the worker makes of it.
  const onPorts = { ...s, tab: 'ports' as const };
  const inst = findInst(s, a.instance);
  if (!inst) return { ...onPorts, message: noMatch(a.instance) };
  const addr = a.address ?? '127.0.0.1';
  if (!/^\d{1,3}(\.\d{1,3}){3}$/.test(addr)) return { ...onPorts, message: `bad address \`${addr}\`` };
  const taken = new Set(s.ports.map(localPort));
  let host = a.host ?? a.port;
  if (a.host !== null) {
    if (taken.has(host)) return { ...onPorts, message: `port ${host}: address in use` };
  } else {
    while (taken.has(host) && host < a.port + PREFER_TRIES) host++;
    if (taken.has(host)) return { ...onPorts, message: `port ${a.port}: address in use` };
  }
  const service = a.service;
  const target = service ? `${service}:${a.port} (via instance ${inst.name})` : `${inst.name}:${a.port}`;
  let state = forwardState(inst.name, inst.status);
  if (service && service !== SERVICE) state = `error: instance \`${inst.name}\` doesn't use service \`${service}\``;
  const running = inst.status === 'running';
  const process = !service && a.port === 3000 && running ? `node (pid ${devServerPid(inst.name)})` : null;
  const row: Forward = { id: s.nextPort, local: `${addr}:${host}`, target, process, state, conns: 0, instance: inst.name };
  return {
    ...onPorts,
    ports: [...s.ports, row],
    portSel: s.ports.length,
    nextPort: s.nextPort + 1,
    message: `forwarding ${row.local} -> ${service ? `${service}:${a.port}` : target}`,
  };
}

/** `d` (Ports): stop the selected forward. */
function stopForward(s: State): State {
  const row = s.ports[s.portSel];
  if (!row) return s;
  const ports = s.ports.filter((p) => p.id !== row.id);
  return { ...s, ports, portSel: Math.min(s.portSel, Math.max(0, ports.length - 1)), message: `stopped ${row.local}` };
}

// ---- The `:` prompt.

const openPrompt = (s: State, input = ''): State => ({ ...s, prompt: newPrompt(input), message: null });

/** What completes right now (`instance_names`, `service_names`, config sandboxes). */
function names(s: State) {
  return {
    servicesTab: s.tab === 'services',
    sandboxes: [SANDBOX],
    instances: s.instances.map((i) => i.name),
    services: [SERVICE],
  };
}

const notInPreview = (cmd: string) => `${cmd} is not in this preview`;

/** Run a parsed line in the fake state (the real event loop's dispatch in `src/tui/mod.rs`). */
function runAction(s: State, a: Action): State {
  switch (a.cmd) {
    case 'run':
      return runInstance(s, a);
    case 'rm':
      return rmInstance(s, a.instance);
    case 'stop':
    case 'start': {
      const inst = findInst(s, a.instance);
      if (!inst) return suspend(s, errorLines(a.cmd, noMatch(a.instance)));
      const next = setRunState(s, inst.name, a.cmd === 'stop' ? 'exited' : 'running');
      return suspend(next, [`${a.cmd === 'stop' ? 'stopped' : 'started'} ${inst.name}`]);
    }
    case 'code':
      // `code` never suspends: a status line (`launch_code`).
      return findInst(s, a.instance)
        ? { ...s, vscode: a.instance, message: `VS Code opened on ${a.instance}` }
        : { ...s, message: `code: unknown instance \`${a.instance}\`` };
    case 'exec': {
      const inst = findInst(s, a.instance);
      if (!inst) return suspend(s, errorLines('exec', `no sandbox instance matches \`${a.instance}\` (see \`devsandbox ps\`)`));
      if (inst.status !== 'running') {
        return suspend(s, [`Error response from daemon: container devsandbox-${inst.name} is not running`]);
      }
      if (/^(ba|z)?sh$/.test(a.argv[0])) return suspend(s, ['(an interactive shell: press t for one in the dashboard)']);
      return suspend(s, shell(inst.name, a.argv.join(' '), branchOf(inst)).lines);
    }
    case 'port':
      return addForward(s, a);
    case 'rename':
    case 'rebuild':
      return { ...s, message: notInPreview(a.cmd) };
  }
}

/** Keys while the prompt is open (`on_key_prompt` in `src/tui/app/command_line.rs`). */
function promptKey(s: State, key: string): State {
  const p = s.prompt;
  if (!p) return s;
  switch (key) {
    case 'Escape':
      return { ...s, prompt: null };
    case 'Tab':
      return { ...s, prompt: complete(p, (idx, tokens) => candidatesFor(idx, tokens, names(s))) };
    case 'Backspace':
      return { ...s, prompt: backspace(p) };
    case 'Enter': {
      const parsed = parseLine(p.input);
      // A parse error stays inline and keeps the prompt open.
      if ('error' in parsed) return { ...s, prompt: { ...p, error: parsed.error } };
      return runAction({ ...s, prompt: null }, parsed);
    }
  }
  return [...key].length === 1 ? { ...s, prompt: typeChar(p, key) } : s;
}

// ---- The logs / config views.

/** The config explorer for the selection (`open_config` in `src/tui/app/view.rs`). */
function openConfig(s: State): State {
  if (s.tab === 'services') {
    const modal: Modal = { kind: 'config', target: 'service', side: 'original', scroll: 0, container: SERVICE_CONTAINER };
    return { ...s, modal, message: null };
  }
  // Ports and Inbox have no config (`enter` on the Inbox opens a link; this one has none).
  if (s.tab !== 'instances') return s;
  // An instance inspects its own container; the sandbox row its first running one.
  const inst = selectedInst(s) ?? s.instances.find((i) => i.status === 'running');
  const container = inst ? `devsandbox-${inst.name}` : null;
  return { ...s, modal: { kind: 'config', target: 'sandbox', side: 'original', scroll: 0, container }, message: null };
}

/** The config pane's body for the side shown. */
export function configLines(m: Extract<Modal, { kind: 'config' }>): readonly string[] {
  if (m.target === 'service') return SERVICE_TABLE;
  return m.side === 'original' ? SANDBOX_ORIGINAL : SANDBOX_RESOLVED;
}

/** The inspect pane's body (`set_inspect`). */
export function inspectLines(s: State, m: Extract<Modal, { kind: 'config' }>): readonly string[] {
  if (!m.container) return ['(no running instance)'];
  if (m.target === 'service') return serviceInspect();
  const name = m.container.replace(/^devsandbox-/, '');
  return instanceInspect(name, findInst(s, name)?.status !== 'exited');
}

/** `" config: web — original (t: resolved) — <hash> "`, as `draw_config_modal` titles it. */
export function configTitle(m: Extract<Modal, { kind: 'config' }>): string {
  const [side, other] = m.side === 'original' ? ['original', 'resolved'] : ['resolved', 'original'];
  const name = m.target === 'service' ? SERVICE : SANDBOX;
  const hash = m.target === 'sandbox' ? ` — ${CONFIG_HASH}` : '';
  // The config pane always has focus here (Tab, the pane switch, leaves the widget).
  return `▶ config: ${name} — ${side} (t: ${other})${hash}`;
}

export const inspectTitle = (m: Extract<Modal, { kind: 'config' }>): string =>
  m.container ? `inspect — ${m.container}` : 'inspect';

export const logsTitle = (name: Instance): string => `logs — devsandbox-${name} (last 50)`;

export const logsFor = (name: Instance): readonly string[] => logLines(name);

const viewLines = (m: Modal): readonly string[] => (m.kind === 'logs' ? logsFor(m.name) : configLines(m));

/** `scroll_key` in `src/tui/app/view.rs`: clamped to `[0, lines - 1]`. */
function scrolled(scroll: number, lines: number, key: string): number | null {
  const max = Math.max(0, lines - 1);
  switch (key) {
    case 'ArrowUp':
    case 'k':
      return Math.max(0, scroll - 1);
    case 'ArrowDown':
    case 'j':
      return Math.min(max, scroll + 1);
    case 'PageUp':
      return Math.max(0, scroll - 20);
    case 'PageDown':
      return Math.min(max, scroll + 20);
    case 'g':
      return 0;
    case 'G':
      return max;
  }
  return null;
}

/** Keys while logs or the config explorer is up (`on_key_modal`). */
function modalKey(s: State, key: string): State {
  const m = s.modal;
  if (!m) return s;
  if (key === 'Escape' || key === 'q') return { ...s, modal: null };
  if (m.kind === 'config' && key === 't') {
    const next = { ...m, side: m.side === 'original' ? ('resolved' as const) : ('original' as const) };
    return { ...s, modal: { ...next, scroll: Math.min(next.scroll, Math.max(0, configLines(next).length - 1)) } };
  }
  const scroll = scrolled(m.scroll, viewLines(m).length, key);
  return scroll === null || scroll === m.scroll ? s : { ...s, modal: { ...m, scroll } };
}

// ---- The dashboard.

export function pressKey(s: State, key: string): State {
  // Precedence like `on_key`: a suspended command owns the screen, then the
  // prompt, a modal, a focused terminal, the dashboard.
  if (s.suspended) return suspendedKey(s, key);
  if (s.prompt) return promptKey(s, key);
  if (s.help) {
    // The help modal swallows everything but its close keys (`esc`/`q`/`?`).
    return key === 'Escape' || key === '?' || key === 'q' ? { ...s, help: false } : s;
  }
  if (s.modal) return modalKey(s, key);
  if (s.focus === 'terminal') return terminalKey(s, key);
  const i = TABS.indexOf(s.tab);
  const onInstances = s.tab === 'instances';
  const inst = selectedInst(s);
  switch (key) {
    case 'ArrowUp':
    case 'k':
      if (s.tab === 'ports') return { ...s, portSel: Math.max(0, s.portSel - 1), message: null };
      return onInstances ? { ...s, selected: Math.max(Math.min(0, s.selected), s.selected - 1), message: null } : s;
    case 'ArrowDown':
    case 'j':
      if (s.tab === 'ports') return { ...s, portSel: Math.max(0, Math.min(s.ports.length - 1, s.portSel + 1)), message: null };
      return onInstances ? { ...s, selected: Math.min(s.instances.length - 1, s.selected + 1), message: null } : s;
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
    case ':':
      return openPrompt(s);
    case 'o':
      if (!onInstances || !inst) return s;
      return { ...s, vscode: inst.name, message: `VS Code opened on ${inst.name}` };
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
    case 'Enter':
    case 'e':
      return openConfig(s);
    case 'r':
      // `open_rename_or_run_prompt`: rename an instance, run on the sandbox row.
      if (!onInstances) return s;
      return openPrompt(s, inst ? `rename ${inst.name} ` : `run ${SANDBOX} `);
    case 's':
      // `stop_or_start_instance`: the real one runs in the background and
      // reports `stopped web-2` when done; here it's done at once.
      if (!onInstances || !inst) return s;
      return inst.status === 'running'
        ? { ...setRunState(s, inst.name, 'exited'), message: `stopped ${inst.name}` }
        : { ...setRunState(s, inst.name, 'running'), message: `started ${inst.name}` };
    case 'l':
      return onInstances && inst ? { ...s, modal: { kind: 'logs', name: inst.name, scroll: 0 }, message: null } : s;
    case 'p':
      if (onInstances) return inst ? openPrompt(s, `port ${inst.name} `) : s;
      // `open_port_prompt_service`: the first instance using it fills the slot.
      if (s.tab === 'services') return openPrompt(s, `port ${s.instances[0]?.name ?? ''} --service ${SERVICE} `);
      return s;
    case 'd':
      return s.tab === 'ports' ? stopForward(s) : s;
    case 'Escape':
      return s.vscode ? { ...s, vscode: null, message: 'VS Code closed' } : s;
    case '?':
      return { ...s, help: true };
    default:
      return s;
  }
}

/** Something covers the dashboard, so clicks on it don't count. */
const covered = (s: State): boolean => s.help || s.modal !== null || s.prompt !== null || s.suspended !== null;

function apply(s: State, e: Event): State {
  switch (e.type) {
    case 'key':
      return pressKey(s, e.key);
    case 'select': {
      const selected = s.instances.findIndex((i) => i.name === e.name);
      return selected < 0 || covered(s) ? s : { ...s, tab: 'instances', selected, message: null, focus: 'dashboard' };
    }
    case 'selectSandbox':
      return covered(s) ? s : { ...s, tab: 'instances', selected: SANDBOX_ROW, message: null, focus: 'dashboard' };
    case 'selectPort':
      return covered(s) || !s.ports[e.index] ? s : { ...s, tab: 'ports', portSel: e.index, message: null, focus: 'dashboard' };
    case 'tab':
      return covered(s) ? s : { ...switchTab(s, e.tab), focus: 'dashboard' };
    case 'focus': {
      if (covered(s) || (e.focus === 'terminal' && s.terms.length === 0)) return s;
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
  const user = e.type !== 'agent' && e.type !== 'notify';
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

// ---- Pure render helpers: what the driver writes, kept here to be testable.

/**
 * Which hint bar shows (`data-tui-help`): the dashboard's, a live shell's, an
 * exited one's, or a view's own (`draw_help`); the prompt takes the bar's place.
 */
export type HelpMode = 'dashboard' | 'terminal' | 'exited' | 'prompt' | 'help' | 'logs' | 'config';

export function helpMode(s: State): HelpMode {
  if (s.prompt) return 'prompt';
  if (s.help) return 'help';
  if (s.modal) return s.modal.kind;
  const term = activeTerm(s);
  return s.focus === 'terminal' && term ? (term.exited ? 'exited' : 'terminal') : 'dashboard';
}

/** One tab in the terminal pane's title strip, like `TermTabs` titles. */
export const termTabLabel = (t: Term, i: number): string => `${i + 1}:${t.name}${t.exited ? ' (exited)' : ''}`;

/** The pane's rows: scrollback, then the prompt line while the shell lives (the cursor goes after it). */
export const termRows = (t: Term): string[] => (t.exited ? [...t.lines] : [...t.lines, promptFor(t.name) + t.input]);

/** A run of text and its colour class, for rows and Detail lines. */
export interface Span {
  text: string;
  cls?: string;
}
export type Line = Span[];

const sp = (text: string, cls?: string): Span => (cls ? { text, cls } : { text });

const statusCls = (st: RunState) => (st === 'running' ? 'tui-green' : 'tui-red');

/** An Instances-table row's four cells (`instance_tree_row` in `src/tui/ui.rs`, trimmed). */
export function instanceCells(s: State, i: Inst): Line[] {
  const mail = i.name === NOTIFY_FROM && s.unread > 0 ? [sp(` ✉${s.unread}`, 'tui-mail')] : [];
  return [
    [sp(`  ${i.name}`), ...mail],
    [sp(i.status, statusCls(i.status))],
    [sp(i.uptime)],
    [sp(i.worktree ? `⎇ ${folderOf(i)}` : folderOf(i))],
  ];
}

const running = (s: State) => s.instances.filter((i) => i.status === 'running').length;

/** The `▾ web` row (`sandbox_tree_row`). */
export function sandboxCells(s: State): Line[] {
  const up = running(s);
  const stats = `${up}/${s.instances.length} running`;
  return [
    [sp(`▾ ${SANDBOX}`, 'tui-accent')],
    [sp(stats, up > 0 ? 'tui-green' : s.instances.length === 0 ? 'tui-dim' : undefined)],
    [],
    [sp('image node:22', 'tui-source'), sp(' '), sp('../web', 'tui-dim')],
  ];
}

/** Header totals (`totals_line` in `src/tui/data.rs`). */
export function totalsLine(s: State): string {
  const up = running(s);
  return `1 sandbox · ${up} running / ${s.instances.length - up} stopped · 1 service container · docker 27.3.1`;
}

/** The service's USED BY column: every instance referencing it. */
export const usedBy = (s: State): string => s.instances.map((i) => i.name).join(',') || '-';

const kv = (k: string, v: string, cls?: string): Span[] => [sp(`${k}: `, 'tui-dim'), sp(v, cls)];

/** The Instances tab's Detail box (`instance_detail` / `sandbox_detail`, trimmed to fit). */
export function detailLines(s: State): Line[] {
  const i = selectedInst(s);
  if (!i) {
    const up = running(s);
    return [
      [...kv('source', 'image node:22'), sp('   '), ...kv('folder', '../web')],
      [...kv('services', SERVICE), sp('   '), ...kv('extends', 'agent')],
      kv('config hash', CONFIG_HASH),
      kv('instances', `${s.instances.length} (${up} running)`),
    ];
  }
  const agents = i.status === 'running' ? kv('agents', '1', 'tui-blue') : kv('agents', '-', 'tui-dim');
  return [
    kv('container', `devsandbox-${i.name}`),
    kv('workspace', `/workspaces/${i.name}`),
    agents,
    i.worktree ? [...kv('base', '../web'), sp('   '), ...kv('worktree', folderOf(i))] : kv('base folder', '../web'),
  ];
}

/** A Ports-tab row's cells (`port_row`). */
export function portCells(f: Forward): Line[] {
  const stateCls = f.state === 'active' ? 'tui-green' : f.state.startsWith('error') ? 'tui-red' : 'tui-yellow';
  return [[sp(f.local)], [sp(f.target)], [sp(f.process ?? '-')], [sp(f.state, stateCls)], [sp(String(f.conns))]];
}

/** TOML highlighting like `highlight_toml_line`: headers accent, `key =` green, comments dim. */
export function tomlSpans(line: string): Line {
  const t = line.trimStart();
  if (t.startsWith('#')) return [sp(line, 'tui-dim')];
  if (t.startsWith('[')) return [sp(line, 'tui-accent')];
  const eq = line.indexOf('=');
  return eq < 0 ? [sp(line)] : [sp(line.slice(0, eq + 1), 'tui-green'), sp(line.slice(eq + 1))];
}

/** JSON highlighting like `classify_json_line`: punctuation-only lines dim, `"key":` green. */
export function jsonSpans(line: string): Line {
  const t = line.trim();
  if (t && /^[[\]{},]+$/.test(t)) return [sp(line, 'tui-dim')];
  const colon = line.indexOf(':');
  return line.trimStart().startsWith('"') && colon >= 0
    ? [sp(line.slice(0, colon + 1), 'tui-green'), sp(line.slice(colon + 1))]
    : [sp(line)];
}

/** `s`'s verb in the hint bar (`stop_start_hint`). */
export const stopStartHint = (s: State): 'stop' | 'start' => (selectedInst(s)?.status === 'exited' ? 'start' : 'stop');

/** `r`'s verb in the hint bar (`run_rename_hint`). */
export const runRenameHint = (s: State): 'run' | 'rename' => (selectedInst(s) ? 'rename' : 'run');

/** The suspended screen's lines, the pending `rm` question last. */
export function suspendedLines(sus: Suspended): string[] {
  return sus.confirm ? [...sus.lines, confirmQuestion(sus.confirm.branch)] : [...sus.lines];
}

/**
 * The status-bar slot: a pending message wins, else the keys that work in the
 * current focus while the widget has page focus, else the invitation to click.
 */
export function hintFor(s: State, pageFocus: boolean): { kind: 'message' | 'hint'; text: string } {
  if (s.message && !s.prompt && !s.suspended) return { kind: 'message', text: s.message };
  let keys = '? FOR KEYS · TAB LEAVES';
  if (s.suspended) keys = s.suspended.confirm ? 'Y OR N ANSWERS' : 'ANY KEY RETURNS';
  else if (s.prompt) keys = 'TAB COMPLETES · ESC CANCELS';
  else if (s.help || s.modal) keys = 'ESC CLOSES · TAB LEAVES';
  else if (s.focus === 'terminal') keys = 'ESC TO DASHBOARD · TAB LEAVES';
  return { kind: 'hint', text: pageFocus ? keys : 'CLICK TO TRY IT' };
}

/** A short, polite announcement of what a user action changed (or null). */
export function describe(prev: State, next: State): string | null {
  if (next.help !== prev.help) return next.help ? 'Help open. Escape or ? closes it.' : 'Help closed';
  if (next.suspended !== prev.suspended) {
    if (!next.suspended) return 'Back to the dashboard';
    const lines = suspendedLines(next.suspended).join('. ');
    return next.suspended.confirm ? lines : `${lines}. Press any key to return.`;
  }
  if (next.prompt !== prev.prompt) {
    const p = next.prompt;
    if (!p) return next.message ?? 'Prompt closed';
    if (!prev.prompt) {
      return `Command prompt${p.input ? `: ${p.input}` : ''}. Tab completes, Enter runs, Escape cancels.`;
    }
    if (p.error && p.error !== prev.prompt.error) return p.error;
    if (p.completion && p.completion !== prev.prompt.completion) return p.input;
    return null;
  }
  if (next.modal?.kind !== prev.modal?.kind) {
    const m = next.modal;
    if (!m) return 'Closed';
    return m.kind === 'logs'
      ? `${logsTitle(m.name)}. Escape closes.`
      : `Config explorer for ${m.target === 'service' ? SERVICE : SANDBOX}, ${m.side}. t toggles, Escape closes.`;
  }
  if (next.modal?.kind === 'config' && prev.modal?.kind === 'config' && next.modal.side !== prev.modal.side) {
    return `Showing ${next.modal.side}`;
  }
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
  if (next.selected !== prev.selected) return `${selectedName(next) ?? `sandbox ${SANDBOX}`} selected`;
  if (next.portSel !== prev.portSel && next.ports[next.portSel]) return `${next.ports[next.portSel].local} selected`;
  return null;
}
