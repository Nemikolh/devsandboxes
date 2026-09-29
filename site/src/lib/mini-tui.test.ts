import { describe, expect, it } from 'vitest';
import {
  activeTerm,
  CTRL_BRACKET,
  describe as announce,
  type Event,
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
  TIMELINE,
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
    for (const k of [...dashboard, 'Escape', 'F12', CTRL_BRACKET]) expect(isHandledKey(STATIC, k)).toBe(true);
    for (const k of ['Tab', '/', 'K', 'Enter', ' ', '5', 'Shift', 'Backspace']) expect(isHandledKey(STATIC, k)).toBe(false);
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
