import { describe, expect, it } from 'vitest';
import {
  activeTerm,
  CTRL_BRACKET,
  describe as announce,
  type Event,
  helpMode,
  hintFor,
  INITIAL,
  isHandledKey,
  keycapFor,
  playing,
  promptFor,
  reduce,
  selectedName,
  settled,
  type State,
  STATIC,
  TERM_ROWS,
  termRows,
  termTabLabel,
  TIMELINE,
  configLines,
  configTitle,
  detailLines,
  inspectLines,
  inspectTitle,
  instanceCells,
  logsTitle,
  portCells,
  runRenameHint,
  SANDBOX_ROW,
  stopStartHint,
  suspendedLines,
  totalsLine,
  usedBy,
} from './mini-tui';

const keys = (s: State, ...ks: string[]) => ks.reduce((acc, key) => reduce(acc, { type: 'key', key }), s);
const run = (s: State, ...es: Event[]) => es.reduce(reduce, s);
/** Type `line` into the focused terminal and press Enter. */
const type = (s: State, line: string) => keys(s, ...line, 'Enter');
const tabs = (s: State) => s.terms.map((t) => t.name);

describe('selection', () => {
  it('moves with arrows and j/k, clamped like the dashboard', () => {
    expect(selectedName(keys(STATIC, 'j'))).toBe('web-2');
    expect(selectedName(keys(STATIC, 'ArrowDown', 'j', 'j', 'j'))).toBe('web-3');
    expect(selectedName(keys(STATIC, 'j', 'k', 'ArrowUp', 'k'))).toBe('web');
  });

  it('only moves on the Instances tab', () => {
    const s = keys(STATIC, '2', 'j');
    expect(s.tab).toBe('services');
    expect(s.selected).toBe(0);
  });

  it('clicking a row selects it and returns to Instances', () => {
    const s = run(STATIC, { type: 'tab', tab: 'ports' }, { type: 'select', name: 'web-3' });
    expect(s.tab).toBe('instances');
    expect(selectedName(s)).toBe('web-3');
  });
});

describe('tabs', () => {
  it('←/→ and the tab keycap cycle and wrap; 1-4 jump', () => {
    expect(keys(STATIC, 'ArrowRight').tab).toBe('services');
    expect(keys(STATIC, 'ArrowLeft').tab).toBe('inbox');
    expect(keys(STATIC, 'Tab', 'Tab', 'Tab', 'Tab').tab).toBe('instances');
    expect(keys(STATIC, '3').tab).toBe('ports');
    expect(keys(STATIC, '4', '1').tab).toBe('instances');
  });

  it('entering the Inbox marks it read', () => {
    expect(STATIC.unread).toBe(1);
    expect(keys(STATIC, '4').unread).toBe(0);
    expect(keys(STATIC, 'ArrowRight').unread).toBe(1);
  });

  it('a notification arriving while on the Inbox is read at once', () => {
    const s = run(keys(INITIAL, '4'), { type: 'notify' });
    expect(s.notified).toBe(true);
    expect(s.unread).toBe(0);
  });
});

describe('VS Code', () => {
  it('o opens it on the selected instance, Escape closes it', () => {
    const open = keys(STATIC, 'j', 'o');
    expect(open.vscode).toBe('web-2');
    expect(open.message).toBe('VS Code opened on web-2');
    expect(keys(open, 'j').vscode).toBe('web-2');
    expect(keys(open, 'j', 'o').vscode).toBe('web-3');
    expect(keys(open, 'Escape').vscode).toBeNull();
  });

  it('o is Instances-only, like the dashboard', () => {
    expect(keys(STATIC, '2', 'o').vscode).toBeNull();
  });

  it('Escape with nothing open changes nothing', () => {
    const s = keys(STATIC, 'Escape');
    expect(s).toEqual({ ...STATIC });
  });
});

describe('terminal', () => {
  it('t opens one on the selected instance and focuses it', () => {
    const s = keys(STATIC, 'j', 't');
    expect(tabs(s)).toEqual(['web-2']);
    expect(s.focus).toBe('terminal');
    expect(activeTerm(s)).toEqual({ name: 'web-2', lines: [], input: '', exited: false });
  });

  it('t is Instances-only; no terminals leaves the layout alone', () => {
    expect(keys(STATIC, '3', 't').terms).toEqual([]);
    expect(keys(STATIC, '3', 't').focus).toBe('dashboard');
  });

  it('t on an instance that has one focuses it instead of opening another', () => {
    const s = keys(STATIC, 't', 'Escape', 'j', 't', 'Escape', 'k', 't');
    expect(tabs(s)).toEqual(['web', 'web-2']);
    expect(s.active).toBe(0);
    expect(s.focus).toBe('terminal');
  });

  it('] and [ switch the shown terminal, wrapping', () => {
    const s = keys(STATIC, 't', 'Escape', 'j', 't', 'Escape', 'j', 't', 'Escape');
    expect(s.active).toBe(2);
    expect(keys(s, ']').active).toBe(0);
    expect(keys(s, '[').active).toBe(1);
    expect(keys(s, '[', '[', '[').active).toBe(2);
    expect(keys(STATIC, ']', '[')).toEqual(STATIC);
  });

  it('x closes the active one; closing the last restores the dashboard', () => {
    const two = keys(STATIC, 't', 'Escape', 'j', 't', 'Escape', '[');
    const one = keys(two, 'x');
    expect(tabs(one)).toEqual(['web-2']);
    expect(one.active).toBe(0);
    const none = keys(one, 'x');
    expect(none.terms).toEqual([]);
    expect(none.focus).toBe('dashboard');
    expect(keys(none, 'x')).toEqual(none);
  });

  it('Escape, ctrl-] and F12 leave; ctrl-] and F12 come back', () => {
    const open = keys(STATIC, 't');
    for (const leave of ['Escape', CTRL_BRACKET, 'F12']) expect(keys(open, leave).focus).toBe('dashboard');
    expect(keys(open, 'Escape', CTRL_BRACKET).focus).toBe('terminal');
    expect(keys(open, 'Escape', 'F12').focus).toBe('terminal');
    expect(keys(STATIC, CTRL_BRACKET).focus).toBe('dashboard');
    expect(keys(STATIC, CTRL_BRACKET).message).toBe('no terminal open \u2014 press t');
  });

  it('while focused, dashboard keys are typed, not run', () => {
    const s = keys(STATIC, 't', 'j', 'o', 'x', '2', '?', ' ', 'q');
    expect(activeTerm(s)?.input).toBe('jox2? q');
    expect(s.selected).toBe(0);
    expect(s.vscode).toBeNull();
    expect(s.tab).toBe('instances');
    expect(s.help).toBe(false);
    expect(s.terms).toHaveLength(1);
    expect(keys(s, 'Shift', 'ArrowUp', 'F5')).toEqual(s);
  });

  it('Backspace edits the input line', () => {
    const s = keys(STATIC, 't', 'l', 's', 'x', 'Backspace');
    expect(activeTerm(s)?.input).toBe('ls');
    expect(keys(s, 'Backspace', 'Backspace', 'Backspace')).toEqual(keys(s, 'Backspace', 'Backspace'));
  });

  it('Enter echoes the prompt line and the canned answer', () => {
    const s = type(keys(STATIC, 'j', 't'), 'git   status');
    expect(activeTerm(s)).toEqual({
      name: 'web-2',
      lines: [
        'root@devsandbox-web-2:/workspaces/web-2# git   status',
        'On branch sandbox/web-2',
        'Changes not staged for commit:',
        '        modified:   src/api/users.ts',
      ],
      input: '',
      exited: false,
    });
    expect(activeTerm(type(keys(STATIC, 't'), 'git status'))?.lines[1]).toBe('On branch main');
    expect(activeTerm(type(keys(STATIC, 't'), 'pwd'))?.lines).toEqual([`${promptFor('web')}pwd`, '/workspaces/web']);
    expect(activeTerm(type(keys(STATIC, 't'), 'vim x'))?.lines[1]).toBe('bash: vim: command not found');
    expect(activeTerm(keys(STATIC, 't', 'Enter'))?.lines).toEqual([promptFor('web')]);
  });

  it('clear empties the screen; scrollback keeps what fits', () => {
    let s = keys(STATIC, 't');
    for (let i = 0; i < 10; i++) s = type(s, 'whoami');
    expect(activeTerm(s)?.lines).toHaveLength(TERM_ROWS - 1);
    expect(activeTerm(s)?.lines.at(-1)).toBe('root');
    expect(activeTerm(type(s, 'clear'))?.lines).toEqual([]);
  });

  it('exit marks the tab exited: keys are swallowed, x closes, t opens a fresh one', () => {
    const dead = type(keys(STATIC, 't'), 'exit');
    expect(activeTerm(dead)?.exited).toBe(true);
    expect(activeTerm(dead)?.lines.at(-1)).toBe('logout');
    expect(dead.focus).toBe('terminal');
    expect(keys(dead, 'l', 's', 'Enter')).toEqual(dead);
    expect(keys(dead, 'x').terms).toEqual([]);
    expect(keys(dead, 'x').focus).toBe('dashboard');
    const fresh = keys(dead, 'Escape', 't');
    expect(fresh.terms.map((t) => t.exited)).toEqual([true, false]);
    expect(fresh.active).toBe(1);
  });

  it('clicks: the pane (or a tab) focuses it, a row or tab click leaves it', () => {
    const two = keys(STATIC, 't', 'Escape', 'j', 't', 'Escape');
    const clicked = run(two, { type: 'focus', focus: 'terminal', term: 0 });
    expect(clicked.focus).toBe('terminal');
    expect(clicked.active).toBe(0);
    expect(run(clicked, { type: 'select', name: 'web-3' }).focus).toBe('dashboard');
    expect(run(clicked, { type: 'tab', tab: 'ports' }).focus).toBe('dashboard');
    expect(run(clicked, { type: 'focus', focus: 'dashboard' }).focus).toBe('dashboard');
    expect(run(STATIC, { type: 'focus', focus: 'terminal' })).toBe(STATIC);
  });

  it('the terminals stay open across tabs', () => {
    const s = keys(STATIC, 't', 'Escape', '3');
    expect(tabs(s)).toEqual(['web']);
    expect(keys(s, 'x').terms).toEqual([]);
  });
});

describe('?', () => {
  it('the help overlay swallows keys until esc / q / ?', () => {
    const help = keys(STATIC, '?');
    expect(help.help).toBe(true);
    expect(keys(help, 'j', 'o', '2')).toEqual(help);
    expect(keys(help, 'Escape').help).toBe(false);
    expect(keys(help, '?').help).toBe(false);
    expect(keys(help, 'q').help).toBe(false);
  });
});

describe('autoplay', () => {
  it('reaches VS Code open on web-2 within ~6 s', () => {
    let s = INITIAL;
    let t = 0;
    for (const step of TIMELINE) {
      t += step.after;
      s = reduce(s, { type: 'tick' });
      if (s.vscode) break;
    }
    expect(s.vscode).toBe('web-2');
    expect(t).toBeLessThanOrEqual(5000);
  });

  it('delivers the notification on the way', () => {
    expect(INITIAL.unread).toBe(0);
    const s = settled();
    expect(s.notified).toBe(true);
    expect(s.unread).toBe(1);
  });

  it('settles on web-2 with VS Code open and nothing left to play', () => {
    const s = settled();
    expect(selectedName(s)).toBe('web-2');
    expect(s.vscode).toBe('web-2');
    expect(s.tab).toBe('instances');
    expect(playing(s)).toBe(false);
    expect(reduce(s, { type: 'tick' })).toBe(s);
  });

  it('any user action ends it; ticks then do nothing', () => {
    const s = run(INITIAL, { type: 'tick' }, { type: 'key', key: 'k' });
    expect(playing(s)).toBe(false);
    expect(reduce(s, { type: 'tick' })).toBe(s);
    expect(playing(run(INITIAL, { type: 'tick' }, { type: 'select', name: 'web' }))).toBe(false);
  });

  it('agent and notify events are not user actions', () => {
    const s = run(INITIAL, { type: 'agent', name: 'web', status: 'idle' }, { type: 'notify' });
    expect(playing(s)).toBe(true);
    expect(s.agents.web).toBe('idle');
  });
});

describe('key plumbing', () => {
  it('handles only the documented keys, never Tab or /', () => {
    const dashboard = ['ArrowUp', 'ArrowDown', 'ArrowLeft', 'ArrowRight', 'j', 'k', 'o', 't', 'x', '[', ']', '?', '1', '4', 'q'];
    const step3 = [':', 'p', 'l', 'r', 's', 'd', 'e', 'Enter'];
    for (const k of [...dashboard, ...step3, 'Escape', 'F12', CTRL_BRACKET]) expect(isHandledKey(STATIC, k)).toBe(true);
    for (const k of ['Tab', '/', 'K', ' ', '5', 'Shift', 'Backspace']) expect(isHandledKey(STATIC, k)).toBe(false);
  });

  it('a focused terminal takes every key but Tab', () => {
    const s = keys(STATIC, 't');
    for (const k of ['/', 'K', 'Enter', ' ', 'Backspace', 'Shift', 'PageDown', 'Escape']) expect(isHandledKey(s, k)).toBe(true);
    expect(isHandledKey(s, 'Tab')).toBe(false);
  });

  it('maps keys to the keycap they light up', () => {
    expect(keycapFor('j')).toBe('ArrowDown');
    expect(keycapFor('k')).toBe('ArrowUp');
    expect(keycapFor('ArrowRight')).toBe('Tab');
    expect(keycapFor('o')).toBe('o');
    expect(keycapFor(']')).toBe(']');
    expect(keycapFor('x')).toBe('x');
    expect(keycapFor(CTRL_BRACKET)).toBe('Escape');
    expect(keycapFor('F12')).toBe('Escape');
    expect(keycapFor('Enter')).toBeNull();
  });
});

describe('announcements', () => {
  it('say what changed', () => {
    expect(announce(STATIC, keys(STATIC, 'j'))).toBe('web-2 selected');
    expect(announce(STATIC, keys(STATIC, 'o'))).toBe('VS Code opened on web');
    expect(announce(STATIC, keys(STATIC, '4'))).toBe('Inbox tab, notifications marked read');
    expect(announce(STATIC, keys(STATIC, '2'))).toBe('Services tab');
    expect(announce(STATIC, keys(STATIC, '?'))).toMatch(/^Help open/);
    expect(announce(STATIC, keys(STATIC, 'Escape'))).toBeNull();
  });

  it('say what the terminal did', () => {
    const open = keys(STATIC, 't');
    expect(announce(STATIC, open)).toMatch(/^Terminal 1 on web focused/);
    expect(announce(open, keys(open, 'Escape'))).toBe('Dashboard focused');
    expect(announce(open, type(open, 'pwd'))).toBe('/workspaces/web');
    expect(announce(open, keys(open, 'p'))).toBeNull();
    expect(announce(open, type(open, 'exit'))).toMatch(/^Shell exited/);
    expect(announce(open, keys(open, 'Escape', 'x'))).toBe('Terminal closed, none left');
  });
});

describe('render helpers', () => {
  it('pick the hint bar for the focus and the shell state', () => {
    expect(helpMode(STATIC)).toBe('dashboard');
    const open = keys(STATIC, 't');
    expect(helpMode(open)).toBe('terminal');
    expect(helpMode(keys(open, 'Escape'))).toBe('dashboard');
    expect(helpMode(type(open, 'exit'))).toBe('exited');
  });

  it('label terminal tabs 1-based, marking exited ones', () => {
    const s = type(keys(STATIC, 't'), 'exit');
    expect(termTabLabel(s.terms[0], 0)).toBe('1:web (exited)');
    expect(termTabLabel(keys(STATIC, 'j', 't').terms[0], 1)).toBe('2:web-2');
  });

  it('end a live shell with the prompt and the typed input, an exited one without', () => {
    const s = keys(type(keys(STATIC, 't'), 'pwd'), 'l', 's');
    expect(termRows(activeTerm(s)!)).toEqual([promptFor('web') + 'pwd', '/workspaces/web', promptFor('web') + 'ls']);
    const done = type(s, '');
    expect(termRows(activeTerm(type(done, 'exit'))!).at(-1)).toBe('logout');
  });

  it('show a message first, then the keys while focused, else the invitation', () => {
    expect(hintFor(STATIC, false)).toEqual({ kind: 'hint', text: 'CLICK TO TRY IT' });
    expect(hintFor(STATIC, true).text).toBe('? FOR KEYS · TAB LEAVES');
    expect(hintFor(keys(STATIC, 't'), true).text).toBe('ESC TO DASHBOARD · TAB LEAVES');
    expect(hintFor(keys(STATIC, 'o'), true)).toEqual({ kind: 'message', text: 'VS Code opened on web' });
  });
});

// ---- Step 3: the rest of the dashboard.

const names = (s: State) => s.instances.map((i) => i.name);
const inst = (s: State, name: string) => s.instances.find((i) => i.name === name);
/** Open the prompt, type `line`, press Enter. */
const command = (s: State, line: string) => keys(s, ':', ...line, 'Enter');
/** Press any key on the suspended screen. */
const back = (s: State) => keys(s, ' ');
const text = (line: { text: string }[]) => line.map((sp) => sp.text).join('');

describe(': prompt', () => {
  it('opens empty, edits, and Esc cancels without running anything', () => {
    const s = keys(STATIC, ':', 'r', 'u', 'x', 'Backspace');
    expect(s.prompt).toEqual({ input: 'ru', completion: null, error: null });
    expect(keys(s, 'Escape').prompt).toBeNull();
    expect(keys(s, 'Escape')).toEqual({ ...STATIC });
    expect(keys(STATIC, ':', 'Backspace').prompt?.input).toBe('');
  });

  it('takes every key while open, Tab included; dashboard keys are typed', () => {
    const s = keys(STATIC, ':', 'j', 'o', 't', '2', '?');
    expect(s.prompt?.input).toBe('jot2?');
    expect(s.selected).toBe(0);
    expect(s.vscode).toBeNull();
    expect(s.help).toBe(false);
    for (const k of ['Tab', 'Enter', 'Backspace', '/', 'Escape']) expect(isHandledKey(s, k)).toBe(true);
    expect(keys(s, 'ArrowUp', 'Shift')).toEqual(s);
  });

  it('Tab completes commands and names, cycling', () => {
    expect(keys(STATIC, ':', 'r', 'u', 'Tab').prompt?.input).toBe('run');
    expect(keys(STATIC, ':', 'r', 'u', 'Tab', ' ', 'Tab').prompt?.input).toBe('run web');
    const stop = keys(STATIC, ':', ...'stop w', 'Tab');
    expect(stop.prompt?.input).toBe('stop web');
    expect(stop.prompt?.completion?.candidates).toEqual(['web', 'web-2', 'web-3']);
    expect(keys(stop, 'Tab', 'Tab').prompt?.input).toBe('stop web-3');
    // Typing ends the cycle.
    expect(keys(stop, 'x').prompt?.completion).toBeNull();
  });

  it('a parse error stays inline and keeps the prompt open', () => {
    const s = command(STATIC, 'frob');
    expect(s.prompt?.error).toBe('unknown command `frob` (run, exec, code, rm, rename, stop, start, rebuild, port)');
    expect(command(STATIC, 'port web 0').prompt?.error).toBe('port spec `0`: port must not be 0');
    expect(keys(s, 'Backspace').prompt?.error).toBeNull();
    expect(keys(STATIC, ':', 'Enter').prompt?.error).toBe('empty command');
  });

  it('run web adds web-4 on a worktree, behind the suspended screen', () => {
    const s = command(STATIC, 'run web');
    expect(s.prompt).toBeNull();
    expect(names(s)).toEqual(['web', 'web-2', 'web-3', 'web-4']);
    expect(inst(s, 'web-4')).toEqual({ name: 'web-4', status: 'running', worktree: true, uptime: '0s' });
    expect(s.agents['web-4']).toBe('idle');
    expect(s.suspended?.lines).toEqual([
      "Preparing worktree (new branch 'sandbox/web-4')",
      'HEAD is now at 3f9c21a feat: users api',
      'web-4',
    ]);
    // Any key returns (the real one waits for one too), the new row stays.
    expect(back(s).suspended).toBeNull();
    expect(names(back(s))).toHaveLength(4);
    expect(names(back(command(back(s), 'run web')))).toContain('web-5');
  });

  it('run names like the real one: --name, the sandbox name when free, errors', () => {
    expect(names(command(STATIC, 'run web --name api'))).toContain('api');
    const taken = command(STATIC, 'run web --name web-2');
    expect(taken.suspended?.lines[0]).toBe(
      'error: instance `web-2` already exists; `devsandbox start web-2` restarts it, `devsandbox rm web-2` frees the name',
    );
    expect(command(STATIC, 'run api').suspended?.lines[0]).toBe('error: unknown sandbox `api`');
    // With `web` removed the base checkout is free again: `run web` takes it back.
    const freed = back(command(STATIC, 'rm web'));
    const again = command(freed, 'run web');
    expect(inst(again, 'web')).toMatchObject({ worktree: false });
    expect(again.suspended?.lines).toEqual(['web']);
  });

  it('stop / start act on the named instance', () => {
    const stopped = command(STATIC, 'stop web-2');
    expect(stopped.suspended?.lines).toEqual(['stopped web-2']);
    expect(inst(stopped, 'web-2')?.status).toBe('exited');
    expect(stopped.agents['web-2']).toBe('stopped');
    const started = command(back(stopped), 'start web-2');
    expect(started.suspended?.lines).toEqual(['started web-2']);
    expect(inst(started, 'web-2')?.status).toBe('running');
    expect(command(STATIC, 'stop nope').suspended?.lines[0]).toBe(
      'error: no sandbox instance matches `nope` (see `devsandbox ps -a`)',
    );
  });

  it('rm asks about a worktree branch first, y / n / Enter answer', () => {
    const asking = command(STATIC, 'rm web-2');
    expect(names(asking)).toContain('web-2');
    expect(suspendedLines(asking.suspended!)).toEqual(['delete branch `sandbox/web-2`? [y/N] ']);
    expect(isHandledKey(asking, 'y')).toBe(true);
    expect(keys(asking, 'x', 'q')).toEqual(asking);
    const yes = keys(asking, 'y');
    expect(yes.suspended?.lines).toEqual([
      'delete branch `sandbox/web-2`? [y/N] y',
      'Deleted branch sandbox/web-2 (was 3f9c21a).',
      'removed web-2',
    ]);
    expect(names(yes)).toEqual(['web', 'web-3']);
    expect(keys(asking, 'Enter').suspended?.lines).toEqual(['delete branch `sandbox/web-2`? [y/N] ', 'removed web-2']);
    expect(back(yes).suspended).toBeNull();
    // The base checkout has no branch of its own to offer.
    expect(command(STATIC, 'rm web').suspended?.lines).toEqual(['removed web']);
  });

  it('rm keeps the cursor on its row, drops VS Code, terminals and forwards on it', () => {
    const setup = back(command(keys(STATIC, 'j', 'o', 't', 'Escape'), 'port web-2 3000'));
    const gone = keys(command(setup, 'rm web-2'), 'n');
    expect(gone.vscode).toBeNull();
    expect(gone.ports).toEqual([]);
    expect(gone.terms.map((t) => t.exited)).toEqual([true]);
    expect(selectedName(gone)).toBe('web-3');
    const last = back(keys(command(back(command(STATIC, 'rm web')), 'rm web-2'), 'n'));
    const empty = back(keys(command(last, 'rm web-3'), 'n'));
    expect(empty.instances).toEqual([]);
    expect(empty.selected).toBe(SANDBOX_ROW);
  });

  it('code is o; exec answers from the fake shell; rename/rebuild are not here', () => {
    const code = command(STATIC, 'code web-3');
    expect(code.vscode).toBe('web-3');
    expect(code.message).toBe('VS Code opened on web-3');
    expect(command(STATIC, 'code nope').message).toBe('code: unknown instance `nope`');
    expect(command(STATIC, 'exec web-2 git status').suspended?.lines[0]).toBe('On branch sandbox/web-2');
    expect(command(STATIC, 'exec web pwd').suspended?.lines).toEqual(['/workspaces/web']);
    expect(command(STATIC, 'rename web-2 api').message).toBe('rename is not in this preview');
    expect(command(STATIC, 'rebuild web').message).toBe('rebuild is not in this preview');
  });
});

describe('ports', () => {
  it('p prefills the port prompt for the instance (and the service)', () => {
    expect(keys(STATIC, 'j', 'p').prompt?.input).toBe('port web-2 ');
    expect(keys(STATIC, '2', 'p').prompt?.input).toBe('port web --service postgres ');
    expect(keys(STATIC, '3', 'p').prompt).toBeNull();
  });

  it('Enter adds a forward on the same host port, the next free one when taken', () => {
    const one = keys(STATIC, 'j', 'p', ...'3000', 'Enter');
    expect(one.tab).toBe('ports');
    expect(one.message).toBe('forwarding 127.0.0.1:3000 -> web-2:3000');
    expect(one.ports).toEqual([
      { id: 1, local: '127.0.0.1:3000', target: 'web-2:3000', process: 'node (pid 398)', state: 'active', conns: 0, instance: 'web-2' },
    ]);
    const two = command(one, 'port web 3000');
    expect(two.ports.map((p) => p.local)).toEqual(['127.0.0.1:3000', '127.0.0.1:3001']);
    expect(two.portSel).toBe(1);
    expect(command(two, 'port web 3000:3000').message).toBe('port 3000: address in use');
    expect(command(two, 'port web 3000:3000').ports).toHaveLength(2);
    expect(command(two, 'port nope 80').message).toBe('no sandbox instance matches `nope` (see `devsandbox ps -a`)');
  });

  it('a service forward goes via the instance', () => {
    const s = keys(STATIC, '2', 'p', ...'5432', 'Enter');
    expect(s.ports[0]).toMatchObject({ local: '127.0.0.1:5432', target: 'postgres:5432 (via instance web)', process: null });
    expect(s.message).toBe('forwarding 127.0.0.1:5432 -> postgres:5432');
  });

  it('j/k select a forward, d stops it', () => {
    const two = command(command(STATIC, 'port web 3000'), 'port web-2 8080:3000');
    const sel = keys(two, 'k');
    expect(sel.portSel).toBe(0);
    const stopped = keys(sel, 'd');
    expect(stopped.message).toBe('stopped 127.0.0.1:3000');
    expect(stopped.ports.map((p) => p.local)).toEqual(['127.0.0.1:8080']);
    expect(keys(keys(stopped, 'd'), 'd').ports).toEqual([]);
    expect(run(two, { type: 'selectPort', index: 0 }).portSel).toBe(0);
  });

  it('render the Ports columns', () => {
    const s = command(STATIC, 'port web 3000');
    expect(portCells(s.ports[0]).map(text)).toEqual(['127.0.0.1:3000', 'web:3000', 'node (pid 412)', 'active', '0']);
  });
});

describe('stop / start (s)', () => {
  it('stops the selected instance and starts it again', () => {
    const stopped = keys(STATIC, 'j', 's');
    expect(inst(stopped, 'web-2')?.status).toBe('exited');
    expect(stopped.message).toBe('stopped web-2');
    expect(stopped.agents['web-2']).toBe('stopped');
    expect(stopStartHint(stopped)).toBe('start');
    const started = keys(stopped, 's');
    expect(inst(started, 'web-2')?.status).toBe('running');
    expect(started.message).toBe('started web-2');
    expect(started.agents['web-2']).toBe('idle');
    expect(stopStartHint(started)).toBe('stop');
  });

  it('a stopped instance shows it: status, agents, terminal, forwards', () => {
    const setup = back(command(keys(STATIC, 'j', 't', 'Escape'), 'port web-2 3000'));
    const stopped = keys(setup, '1', 's');
    expect(instanceCells(stopped, inst(stopped, 'web-2')!).map(text)).toEqual([
      '  web-2',
      'exited',
      '48m',
      '⎇ .worktrees/web-2',
    ]);
    expect(text(detailLines(stopped)[2])).toBe('agents: -');
    expect(stopped.terms[0].exited).toBe(true);
    expect(stopped.ports[0].state).toBe('error: instance `web-2` is not running (devsandbox start web-2)');
    expect(keys(stopped, 't').message).toBe('terminal: `web-2` is not running');
    expect(keys(stopped, 's').ports[0].state).toBe('active');
    expect(totalsLine(stopped)).toBe('1 sandbox · 2 running / 1 stopped · 1 service container · docker 27.3.1');
  });

  it('is Instances-only, like the real one', () => {
    expect(keys(STATIC, '2', 's').instances).toBe(STATIC.instances);
  });
});

describe('r and the sandbox row', () => {
  it('r on an instance prefills rename, like the real one', () => {
    expect(runRenameHint(STATIC)).toBe('rename');
    expect(keys(STATIC, 'r').prompt?.input).toBe('rename web ');
  });

  it('clicking the sandbox row selects it; r there prefills run, Enter runs it', () => {
    const row = run(STATIC, { type: 'selectSandbox' });
    expect(row.selected).toBe(SANDBOX_ROW);
    expect(selectedName(row)).toBeNull();
    expect(runRenameHint(row)).toBe('run');
    expect(keys(row, 'r').prompt?.input).toBe('run web ');
    expect(names(keys(row, 'r', 'Enter'))).toContain('web-4');
    // ↓ goes to the first instance; o/s/l/p have nothing to act on.
    expect(selectedName(keys(row, 'j'))).toBe('web');
    expect(keys(row, 'o', 's', 'l', 'p')).toEqual({ ...row });
    expect(keys(row, 't').message).toBe('terminal: select an instance');
    expect(text(detailLines(row)[2])).toBe('config hash: c61bbaac526146dd');
  });
});

describe('logs (l)', () => {
  it('opens for the selected instance, scrolls clamped, Esc / q close', () => {
    const s = keys(STATIC, 'j', 'l');
    expect(s.modal).toEqual({ kind: 'logs', name: 'web-2', scroll: 0 });
    expect(logsTitle('web-2')).toBe('logs — devsandbox-web-2 (last 50)');
    expect(keys(s, 'j', 'j', 'k').modal).toMatchObject({ scroll: 1 });
    expect(keys(s, 'G').modal).toMatchObject({ scroll: 10 });
    expect(keys(s, 'G', 'j', 'g').modal).toMatchObject({ scroll: 0 });
    expect(keys(s, 'Escape').modal).toBeNull();
    expect(keys(s, 'q').modal).toBeNull();
    // Swallows dashboard keys while up.
    expect(keys(s, 'o', '2', 's').modal).toEqual(s.modal);
    expect(keys(s, 'o', '2', 's').tab).toBe('instances');
  });

  it('is Instances-only and needs an instance', () => {
    expect(keys(STATIC, '2', 'l').modal).toBeNull();
    expect(run(STATIC, { type: 'selectSandbox' }, { type: 'key', key: 'l' }).modal).toBeNull();
  });
});

describe('config explorer (enter)', () => {
  it('opens on the sandbox, original first; t toggles resolved', () => {
    const s = keys(STATIC, 'j', 'Enter');
    expect(s.modal).toEqual({ kind: 'config', target: 'sandbox', side: 'original', scroll: 0, container: 'devsandbox-web-2' });
    if (s.modal?.kind !== 'config') throw new Error('config');
    expect(configTitle(s.modal)).toBe('▶ config: web — original (t: resolved) — c61bbaac526146dd');
    expect(configLines(s.modal)).toContain('extends = "agent"');
    const resolved = keys(s, 't');
    if (resolved.modal?.kind !== 'config') throw new Error('config');
    expect(resolved.modal.side).toBe('resolved');
    expect(configTitle(resolved.modal)).toBe('▶ config: web — resolved (t: original) — c61bbaac526146dd');
    expect(configLines(resolved.modal)).toContain('caches = ["pnpm"]');
    expect(configLines(resolved.modal)).not.toContain('extends = "agent"');
    expect(keys(resolved, 't').modal).toMatchObject({ side: 'original' });
    expect(keys(s, 'Escape').modal).toBeNull();
    expect(keys(STATIC, 'e').modal?.kind).toBe('config');
  });

  it('inspects the instance, the sandbox row its first running one, or says there is none', () => {
    const s = keys(STATIC, 'Enter');
    if (s.modal?.kind !== 'config') throw new Error('config');
    expect(inspectTitle(s.modal)).toBe('inspect — devsandbox-web');
    expect(inspectLines(s, s.modal)).toContain('    "Name": "/devsandbox-web",');
    const row = keys(run(keys(STATIC, 's'), { type: 'selectSandbox' }), 'Enter');
    expect(row.modal).toMatchObject({ container: 'devsandbox-web-2' });
    const none = back(command(back(command(back(command(STATIC, 'stop web')), 'stop web-2')), 'stop web-3'));
    const empty = keys(run(none, { type: 'selectSandbox' }), 'Enter');
    if (empty.modal?.kind !== 'config') throw new Error('config');
    expect(inspectLines(empty, empty.modal)).toEqual(['(no running instance)']);
  });

  it('on Services shows the service table; nothing on Ports / Inbox', () => {
    const s = keys(STATIC, '2', 'Enter');
    if (s.modal?.kind !== 'config') throw new Error('config');
    expect(configTitle(s.modal)).toBe('▶ config: postgres — original (t: resolved)');
    expect(configLines(s.modal)).toEqual(configLines({ ...s.modal, side: 'resolved' }));
    expect(keys(STATIC, '3', 'Enter')).toEqual(keys(STATIC, '3'));
    expect(keys(STATIC, '4', 'Enter').modal).toBeNull();
  });
});

describe('step 3 plumbing', () => {
  it('clicks do nothing under a prompt, a view or the suspended screen', () => {
    for (const s of [keys(STATIC, ':'), keys(STATIC, 'l'), command(STATIC, 'stop web')]) {
      expect(run(s, { type: 'select', name: 'web-3' })).toBe(s);
      expect(run(s, { type: 'tab', tab: 'ports' })).toBe(s);
      expect(run(s, { type: 'selectSandbox' })).toBe(s);
    }
  });

  it('views take their own keys; the suspended screen any key but Tab', () => {
    const logs = keys(STATIC, 'l');
    for (const k of ['j', 'k', 'g', 'G', 'q', 'Escape', 't']) expect(isHandledKey(logs, k)).toBe(true);
    expect(isHandledKey(logs, 'Tab')).toBe(false);
    const sus = command(STATIC, 'stop web');
    expect(isHandledKey(sus, 'x')).toBe(true);
    expect(isHandledKey(sus, 'Tab')).toBe(false);
  });

  it('pick the hint bar and status slot for each mode', () => {
    expect(helpMode(keys(STATIC, ':'))).toBe('prompt');
    expect(helpMode(keys(STATIC, 'l'))).toBe('logs');
    expect(helpMode(keys(STATIC, 'Enter'))).toBe('config');
    expect(helpMode(keys(STATIC, '?'))).toBe('help');
    expect(hintFor(keys(STATIC, ':'), true).text).toBe('TAB COMPLETES · ESC CANCELS');
    expect(hintFor(command(STATIC, 'stop web'), true).text).toBe('ANY KEY RETURNS');
    expect(hintFor(command(STATIC, 'rm web-2'), true).text).toBe('Y OR N ANSWERS');
    expect(hintFor(keys(STATIC, 'l'), true).text).toBe('ESC CLOSES · TAB LEAVES');
  });

  it('keep the header and service columns in step with the instances', () => {
    expect(totalsLine(STATIC)).toBe('1 sandbox · 3 running / 0 stopped · 1 service container · docker 27.3.1');
    expect(usedBy(command(STATIC, 'run web'))).toBe('web,web-2,web-3,web-4');
    expect(instanceCells(STATIC, STATIC.instances[2]).map(text)).toEqual(['  web-3 ✉1', 'running', '12m', '⎇ .worktrees/web-3']);
  });

  it('announce prompts, views and suspended output', () => {
    expect(announce(STATIC, keys(STATIC, ':'))).toMatch(/^Command prompt\. Tab completes/);
    expect(announce(STATIC, keys(STATIC, 'j', 'p'))).toMatch(/^Command prompt: port web-2 \./);
    const open = keys(STATIC, ':', 'r', 'u');
    expect(announce(open, keys(open, 'Tab'))).toBe('run');
    expect(announce(open, keys(open, 'Enter'))).toMatch(/^unknown command `ru`/);
    expect(announce(STATIC, command(STATIC, 'stop web'))).toBe('stopped web. Press any key to return.');
    expect(announce(STATIC, keys(STATIC, 'l'))).toBe('logs — devsandbox-web (last 50). Escape closes.');
    const cfg = keys(STATIC, 'Enter');
    expect(announce(cfg, keys(cfg, 't'))).toBe('Showing resolved');
  });
});
