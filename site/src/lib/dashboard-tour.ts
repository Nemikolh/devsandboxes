/**
 * The docs dashboard tour (`/docs/dashboard`): which mini dashboard state and
 * which parts of it each scroll section shows. A scene is a replay of input
 * from one base state through the real state machine (`reduce` in
 * `mini-tui.ts`), so a preset can never be a state the widget couldn't reach
 * by hand. The DOM side is `scripts/dashboard-tour.ts`.
 *
 * Pure and DOM-free, like `mini-tui.ts`.
 */

import { type Event, reduce, type State, STATIC } from './mini-tui';

/** The `data-tui-region` hooks of `components/MiniTui.astro` (a test keeps the two in sync). */
export const REGIONS = [
  'tabs',
  'instances',
  'detail',
  'services',
  'ports',
  'inbox',
  'terminal',
  'prompt',
  'hints',
  'help',
  'logs',
  'config',
  'suspended',
  'status',
] as const;
export type Region = (typeof REGIONS)[number];

export interface Scene {
  /** Input replayed from `TOUR_BASE`: a string is a key press. */
  input: readonly (string | Event)[];
  /** What the section talks about; the rest of the widget is dimmed. */
  regions: readonly Region[];
}

/** Every scene starts here: `web` selected on Instances, one unread notification. */
export const TOUR_BASE: State = STATIC;

const typed = (text: string): string[] => [...text];

export const SCENES = {
  instances: { input: ['j'], regions: ['instances', 'detail'] },
  services: { input: ['2'], regions: ['tabs', 'services'] },
  config: { input: ['Enter'], regions: ['config'] },
  run: { input: [{ type: 'selectSandbox' }, 'r'], regions: ['instances', 'prompt'] },
  logs: { input: ['l'], regions: ['logs'] },
  terminal: { input: ['t', ...typed('git status'), 'Enter'], regions: ['terminal'] },
  ports: { input: ['p', ...typed('3000'), 'Enter'], regions: ['tabs', 'ports'] },
  vscode: { input: ['o'], regions: ['instances', 'status'] },
  inbox: { input: ['4'], regions: ['tabs', 'inbox'] },
  prompt: { input: [':', ...typed('rm '), 'Tab'], regions: ['prompt'] },
  help: { input: ['?'], regions: ['help'] },
} as const satisfies Record<string, Scene>;

export type SceneId = keyof typeof SCENES;

export const isSceneId = (id: string): id is SceneId => Object.hasOwn(SCENES, id);

const toEvent = (i: string | Event): Event => (typeof i === 'string' ? { type: 'key', key: i } : i);

/** The widget state a scene shows. */
export function sceneState(id: SceneId): State {
  return SCENES[id].input.map(toEvent).reduce(reduce, TOUR_BASE);
}

/**
 * The active section, from which sections cross the activation line (in
 * document order): the topmost of them, else the previous one, so the gap
 * between two sections doesn't blank the widget.
 */
export function activeSection(prev: number, crossing: readonly boolean[]): number {
  const i = crossing.indexOf(true);
  return i >= 0 ? i : prev;
}
