import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';
import { activeSection, isSceneId, REGIONS, SCENES, type SceneId, sceneState, TOUR_BASE } from './dashboard-tour';
import { activeTerm, helpMode, SANDBOX_ROW, selectedName } from './mini-tui';

const ids = Object.keys(SCENES) as SceneId[];

describe('REGIONS', () => {
  it('matches the data-tui-region hooks of MiniTui.astro', () => {
    const src = readFileSync(new URL('../components/MiniTui.astro', import.meta.url), 'utf8');
    const hooks = new Set([...src.matchAll(/data-tui-region="([a-z]+)"/g)].map((m) => m[1]));
    expect([...hooks].sort()).toEqual([...REGIONS].sort());
  });

  it('are the only regions scenes point at', () => {
    for (const id of ids) for (const r of SCENES[id].regions) expect(REGIONS).toContain(r);
  });
});

describe('sceneState', () => {
  it('replays every scene from the base without leaving a suspended screen', () => {
    for (const id of ids) {
      const s = sceneState(id);
      expect(s, id).not.toBe(TOUR_BASE);
      expect(s.suspended, id).toBeNull();
    }
  });

  it('selects web-2 for the instances scene', () => {
    expect(selectedName(sceneState('instances'))).toBe('web-2');
  });

  it('opens the Services tab', () => {
    expect(sceneState('services').tab).toBe('services');
  });

  it('opens the config explorer on the sandbox as written', () => {
    expect(sceneState('config').modal).toMatchObject({ kind: 'config', target: 'sandbox', side: 'original' });
  });

  it('prefills `run web ` from the sandbox row', () => {
    const s = sceneState('run');
    expect(s.selected).toBe(SANDBOX_ROW);
    expect(s.prompt?.input).toBe('run web ');
  });

  it('shows web’s logs', () => {
    expect(sceneState('logs').modal).toMatchObject({ kind: 'logs', name: 'web' });
  });

  it('leaves a focused terminal on web with git status answered', () => {
    const s = sceneState('terminal');
    expect(s.focus).toBe('terminal');
    expect(helpMode(s)).toBe('terminal');
    expect(activeTerm(s)?.name).toBe('web');
    expect(activeTerm(s)?.lines).toContain('On branch main');
  });

  it('forwards web:3000 and lands on the Ports tab', () => {
    const s = sceneState('ports');
    expect(s.tab).toBe('ports');
    expect(s.ports).toHaveLength(1);
    expect(s.ports[0]).toMatchObject({ local: '127.0.0.1:3000', target: 'web:3000', state: 'active' });
  });

  it('opens VS Code on web', () => {
    const s = sceneState('vscode');
    expect(s.vscode).toBe('web');
    expect(s.message).toBe('VS Code opened on web');
  });

  it('reads the notification in the Inbox', () => {
    const s = sceneState('inbox');
    expect(s.tab).toBe('inbox');
    expect(s.notified).toBe(true);
    expect(s.unread).toBe(0);
  });

  it('completes an instance name after `rm `', () => {
    const p = sceneState('prompt').prompt;
    expect(p?.input).toBe('rm web');
    expect(p?.completion?.candidates).toEqual(['web', 'web-2', 'web-3']);
  });

  it('opens help', () => {
    expect(sceneState('help').help).toBe(true);
  });
});

describe('isSceneId', () => {
  it('knows the table and nothing else', () => {
    expect(isSceneId('ports')).toBe(true);
    expect(isSceneId('toString')).toBe(false);
    expect(isSceneId('nope')).toBe(false);
  });
});

describe('activeSection', () => {
  it('picks the topmost section on the line', () => {
    expect(activeSection(0, [false, true, true])).toBe(1);
  });

  it('keeps the previous one between sections', () => {
    expect(activeSection(2, [false, false, false])).toBe(2);
  });
});
