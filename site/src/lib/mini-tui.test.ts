import { describe, expect, it } from 'vitest';
import {
  describe as announce,
  type Event,
  INITIAL,
  isHandledKey,
  keycapFor,
  playing,
  reduce,
  selectedName,
  settled,
  type State,
  STATIC,
  TIMELINE,
} from './mini-tui';

const keys = (s: State, ...ks: string[]) => ks.reduce((acc, key) => reduce(acc, { type: 'key', key }), s);
const run = (s: State, ...es: Event[]) => es.reduce(reduce, s);

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

describe('t and ?', () => {
  it('t reports a terminal on the selected container', () => {
    expect(keys(STATIC, 'j', 'j', 't').message).toBe('terminal opened in devsandbox-web-3');
    expect(keys(STATIC, '3', 't').message).toBeNull();
  });

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
    for (const k of ['ArrowUp', 'ArrowDown', 'ArrowLeft', 'ArrowRight', 'j', 'k', 'o', 't', '?', '1', '4', 'q', 'Escape']) {
      expect(isHandledKey(k)).toBe(true);
    }
    for (const k of ['Tab', '/', 'K', 'Enter', ' ', '5', 'Shift']) expect(isHandledKey(k)).toBe(false);
  });

  it('maps keys to the keycap they light up', () => {
    expect(keycapFor('j')).toBe('ArrowDown');
    expect(keycapFor('k')).toBe('ArrowUp');
    expect(keycapFor('ArrowRight')).toBe('Tab');
    expect(keycapFor('o')).toBe('o');
    expect(keycapFor('Escape')).toBeNull();
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
});
