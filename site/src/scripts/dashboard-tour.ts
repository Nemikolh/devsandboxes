/**
 * Drives the docs dashboard tour (`components/DashboardTour.astro`): mounts
 * its mini dashboard (`scripts/mini-tui.ts`) and, as a section becomes the
 * active one, shows that section's scene and highlights its regions
 * (`lib/dashboard-tour.ts`).
 *
 * - Active section: an IntersectionObserver on a thin band across the
 *   viewport (35% down on wide screens, just under the pinned widget on
 *   narrow ones); the topmost section on the band wins.
 * - Once the reader plays with the widget, the tour stops driving it and
 *   drops the highlight, until another section becomes active.
 * - Wide screens (`WIDE`) get a playable widget; narrow ones a picture,
 *   zoomed to fit the column and capped to a share of the viewport height.
 *   Both are decided at load, like the landing stage.
 */
import { activeSection, isSceneId, SCENES, type SceneId, sceneState } from '../lib/dashboard-tour';
import { mountMiniTui } from './mini-tui';

const WIDE = '(min-width: 1100px)';
/** Width the narrow layout renders the widget at before zooming it down. */
const NARROW_WIDTH = 560;
/** Most of the viewport height the pinned narrow widget may take. */
const NARROW_MAX_VH = 0.4;
/** Where the activation band sits on wide screens, as a share of the viewport height. */
const WIDE_LINE = 0.35;

for (const tour of document.querySelectorAll<HTMLElement>('[data-dash-tour]')) init(tour);

function init(tour: HTMLElement) {
  const root = tour.querySelector<HTMLElement>('[data-tui]');
  const stage = tour.querySelector<HTMLElement>('[data-dash-tour-stage]');
  const steps = [...tour.querySelectorAll<HTMLElement>('[data-dash-step]')];
  const scenes = steps.map((el) => el.dataset.dashStep ?? '').filter(isSceneId);
  if (!root || !stage || scenes.length === 0 || scenes.length !== steps.length) return;

  const wide = window.matchMedia(WIDE).matches;
  if (!wide) {
    // A picture here: the text says everything the widget shows.
    root.removeAttribute('aria-describedby');
    stage.setAttribute('aria-hidden', 'true');
  }

  const widget = mountMiniTui(root, { autoplay: false, interactive: wide, initial: sceneState(scenes[0]) });
  const regions = [...root.querySelectorAll<HTMLElement>('[data-tui-region]')];

  let active = 0;
  /** The reader took over the widget since the section became active. */
  let manual = false;

  function highlight(scene: SceneId | null) {
    const on = new Set<string>(scene ? SCENES[scene].regions : []);
    for (const el of regions) el.toggleAttribute('data-tour-hl', on.has(el.dataset.tuiRegion ?? ''));
    root!.toggleAttribute('data-tour-focus', on.size > 0);
  }

  function show(i: number) {
    active = i;
    manual = false;
    steps.forEach((el, j) => el.toggleAttribute('data-active', j === i));
    widget.setState(sceneState(scenes[i]));
    highlight(scenes[i]);
  }

  // Input reaches the widget's own listeners after these (capture on an
  // ancestor), so the flag is up while that input's state change commits.
  let input = false;
  const mark = (e: Event) => {
    if (!root.contains(e.target as Node)) return;
    input = true;
    window.setTimeout(() => (input = false));
  };
  tour.addEventListener('keydown', mark, { capture: true });
  tour.addEventListener('click', mark, { capture: true });
  widget.subscribe((s, prev) => {
    if (!input || manual || s === prev) return;
    manual = true;
    highlight(null);
  });

  show(0);

  // ---- Narrow: zoom the pinned widget to the column and a share of the viewport.
  function fit() {
    if (wide) {
      root!.style.removeProperty('zoom');
      return;
    }
    root!.style.zoom = '1';
    const natural = root!.offsetHeight || 1;
    const zoom = Math.min(1, stage!.clientWidth / NARROW_WIDTH, (window.innerHeight * NARROW_MAX_VH) / natural);
    root!.style.zoom = zoom.toFixed(3);
    tour.style.setProperty('--tour-stage-h', `${Math.ceil(stage!.getBoundingClientRect().height)}px`);
  }

  // ---- The activation band, rebuilt when the viewport or the pinned stage changes size.
  let observer: IntersectionObserver | undefined;
  const crossing = new Map<Element, boolean>();
  function observe() {
    observer?.disconnect();
    crossing.clear();
    const vh = window.innerHeight;
    const header = parseFloat(getComputedStyle(document.documentElement).getPropertyValue('--header-h')) || 0;
    const line = wide ? Math.round(vh * WIDE_LINE) : Math.round(header + stage!.getBoundingClientRect().height + 60);
    const top = Math.min(line, vh - 2);
    observer = new IntersectionObserver(
      (entries) => {
        for (const e of entries) crossing.set(e.target, e.isIntersecting);
        const next = activeSection(
          active,
          steps.map((el) => crossing.get(el) ?? false),
        );
        if (next !== active) show(next);
      },
      { rootMargin: `-${top}px 0px -${vh - top - 1}px 0px` },
    );
    for (const el of steps) observer.observe(el);
  }

  let resizeTimer = 0;
  function relayout() {
    fit();
    observe();
  }
  window.addEventListener('resize', () => {
    window.clearTimeout(resizeTimer);
    resizeTimer = window.setTimeout(relayout, 150);
  });
  relayout();
}
