/**
 * DOM driver for one mini dashboard (`components/MiniTui.astro`) over the
 * `mini-tui` state machine: keyboard and pointer input, the optional autoplay
 * while it's on screen, and `render(state)`. Everything is scoped to the
 * widget's root and to the returned controller, so a page can mount several;
 * hosts (the landing diagram, a docs tutorial) watch it with `subscribe` and
 * drive it with `setState` / `dispatch`.
 */
import {
  CTRL_BRACKET,
  INITIAL,
  INSTANCES,
  NOTIFY_FROM,
  TABS,
  describe,
  helpMode,
  hintFor,
  isHandledKey,
  keycapFor,
  playing,
  reduce,
  selectedName,
  settled,
  termRows,
  termTabLabel,
  type Event,
  type Instance,
  type State,
  type Tab,
  TIMELINE,
} from '../lib/mini-tui';

/** How long a keycap looks pressed; the autoplay presses it this early. */
const FLASH_MS = 320;
/** How long an `o` / `t` result stays in the status line. */
const MESSAGE_MS = 2600;

export interface MiniTuiOptions {
  /**
   * Play `TIMELINE` while `observe` is on screen, until the user takes over
   * (skipped under reduced motion or without IntersectionObserver). Default:
   * the root's `data-tui-autoplay`.
   */
  autoplay?: boolean;
  /** What must be ≥ 45% visible for the autoplay to run. Default: the root. */
  observe?: Element;
  /** Take keys and clicks; off, it's a picture driven only from outside. Default: the root's `data-tui-interactive`. */
  interactive?: boolean;
  /** Only take keys while this media query matches (the landing stage is desktop-only). */
  media?: string;
  /** Starting state. Default: `INITIAL` when the autoplay will run, else its end (`settled()`). */
  initial?: State;
}

/** Called after every state change with the new and the previous state. */
export type Listener = (state: State, prev: State) => void;

export interface MiniTui {
  readonly root: HTMLElement;
  getState(): State;
  /** Run one event through `reduce`; `user` makes it act like input (announced, ends the autoplay). */
  dispatch(event: Event, opts?: { user?: boolean }): void;
  /** Replace the state (a host jumping to a scene); a still-running autoplay carries on from its `step`. */
  setState(state: State): void;
  /** Called at once with the current state (`prev` = it too), then on every change; returns the unsubscribe. */
  subscribe(listener: Listener): () => void;
  stopAutoplay(): void;
}

const flag = (el: Element, name: string, on: boolean) => el.toggleAttribute(name, on);

export function mountMiniTui(root: HTMLElement, opts: MiniTuiOptions = {}): MiniTui {
  const all = <T extends Element = HTMLElement>(sel: string) => [...root.querySelectorAll<T>(sel)];
  const one = (sel: string) => root.querySelector<HTMLElement>(sel);
  const hint = one('[data-tui-hint]');
  const live = one('[data-tui-live]');
  const helpOverlay = one('[data-tui-help-overlay]');
  const body = one('.tui-body');
  const termPane = one('[data-tui-term]');
  const termTabs = one('[data-tui-term-tabs]');
  const termScreen = one('[data-tui-term-screen]');

  const interactive = opts.interactive ?? root.dataset.tuiInteractive !== 'false';
  const media = opts.media ? window.matchMedia(opts.media) : null;
  const reducedMotion = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
  const autoplay =
    (opts.autoplay ?? root.dataset.tuiAutoplay === 'true') && !reducedMotion && 'IntersectionObserver' in window;

  let state: State = opts.initial ?? (autoplay ? INITIAL : settled());
  const listeners = new Set<Listener>();
  let timer = 0;
  let messageTimer = 0;
  let visible = false;

  // Writes only; nothing here reads layout, so a render never forces a reflow.
  function render(s: State) {
    const sel = selectedName(s);
    for (const el of all('[data-tui-tab]')) {
      const on = el.dataset.tuiTab === s.tab;
      flag(el, 'data-active', on);
      el.setAttribute('aria-pressed', String(on));
    }
    for (const el of all('[data-tui-panel]')) el.hidden = el.dataset.tuiPanel !== s.tab;
    for (const el of all('[data-tui-panel="instances"] [data-tui-row]')) flag(el, 'data-selected', el.dataset.tuiRow === sel);
    for (const el of all('[data-tui-detail-for]')) el.hidden = el.dataset.tuiDetailFor !== sel;
    for (const el of all('[data-tui-unread]')) {
      el.hidden = s.unread === 0;
      el.textContent = ` (${s.unread})`;
    }
    for (const el of all('[data-tui-row-unread]')) {
      const row = el.closest<HTMLElement>('[data-tui-row]');
      el.hidden = s.unread === 0 || row?.dataset.tuiRow !== NOTIFY_FROM;
    }
    for (const el of all('[data-tui-notified]')) el.hidden = !s.notified;
    for (const el of all('[data-tui-inbox-empty]')) el.hidden = s.notified;
    for (const el of all('[data-tui-row="notify-1"]')) flag(el, 'data-unread', s.unread > 0);
    if (helpOverlay) helpOverlay.hidden = !s.help;
    renderTerms(s);
    renderHint(s);
  }

  // The terminal pane, rebuilt from state (a handful of nodes); text only, never HTML.
  function renderTerms(s: State) {
    const open = s.terms.length > 0;
    const term = s.terms[s.active];
    const focused = s.focus === 'terminal';
    if (body) {
      flag(body, 'data-terms', open);
      body.dataset.focus = s.focus;
    }
    const mode = helpMode(s);
    for (const el of all('[data-tui-help]')) el.hidden = el.dataset.tuiHelp !== mode;
    for (const el of all('[data-tui-terms-hint]')) el.hidden = !open;
    if (!termPane || !termTabs || !termScreen) return;
    termPane.hidden = !open;
    if (!term) return;

    const tabs: Node[] = [];
    if (focused) tabs.push(span('tui-term-mark', '▶ '));
    s.terms.forEach((t, i) => {
      if (i > 0) tabs.push(document.createTextNode('  '));
      const tab = document.createElement('button');
      tab.type = 'button';
      tab.tabIndex = -1;
      tab.className = 'tui-term-tab';
      tab.dataset.tuiTermTab = String(i);
      tab.textContent = termTabLabel(t, i);
      flag(tab, 'data-active', i === s.active);
      flag(tab, 'data-exited', t.exited);
      tabs.push(tab);
    });
    termTabs.replaceChildren(...tabs);

    const rows = termRows(term).map(div);
    if (focused && !term.exited) rows.at(-1)?.append(span('tui-cursor', ''));
    termScreen.replaceChildren(...rows);
  }

  function renderHint(s: State) {
    if (!hint) return;
    const { kind, text } = hintFor(s, root.contains(document.activeElement));
    // A picture-only widget has nothing to invite; it still shows messages.
    hint.hidden = !interactive && kind === 'hint';
    hint.dataset.kind = kind;
    hint.textContent = text;
  }

  function flash(key: string) {
    const cap = keycapFor(key);
    if (!cap) return;
    // Several hint bars share keycaps (`esc`, `x`); only the shown one is visible.
    for (const el of all(`.tui-key[data-key="${CSS.escape(cap)}"]`)) {
      el.setAttribute('data-flash', '');
      window.setTimeout(() => el.removeAttribute('data-flash'), FLASH_MS);
    }
  }

  function commit(next: State) {
    const prev = state;
    state = next;
    render(state);
    for (const fn of listeners) fn(state, prev);
    if (state.message && state.message !== prev.message) {
      window.clearTimeout(messageTimer);
      messageTimer = window.setTimeout(() => {
        if (state.message) commit({ ...state, message: null });
      }, MESSAGE_MS);
    }
  }

  function dispatch(e: Event, user: boolean) {
    const prev = state;
    const next = reduce(state, e);
    if (next === prev) return;
    commit(next);
    if (user && live) {
      const text = describe(prev, state);
      if (text) live.textContent = text;
    }
    if (user) stopAutoplay();
  }

  // ---- Autoplay: one timer for the pending step, dropped when off screen and
  // re-armed (with that step's full pause) when the widget comes back.
  function schedule() {
    window.clearTimeout(timer);
    if (!observer || !visible || !playing(state)) return;
    const { after, event } = TIMELINE[state.step];
    const lead = event.type === 'key' ? FLASH_MS : 0;
    timer = window.setTimeout(() => {
      if (event.type === 'key') flash(event.key);
      timer = window.setTimeout(() => {
        dispatch({ type: 'tick' }, false);
        schedule();
      }, lead);
    }, after - lead);
  }

  let observer: IntersectionObserver | undefined;
  function stopAutoplay() {
    window.clearTimeout(timer);
    observer?.disconnect();
    observer = undefined;
  }

  if (autoplay && playing(state)) {
    observer = new IntersectionObserver(
      ([entry]) => {
        visible = entry.isIntersecting;
        if (visible) schedule();
        else window.clearTimeout(timer);
      },
      { threshold: 0.45 },
    );
    observer.observe(opts.observe ?? root);
  }

  if (interactive) listen();

  // ---- Interaction. The widget is one tab stop; its buttons stay out of the
  // tab order (tabindex=-1) but work with a pointer.
  function listen() {
    root.tabIndex = 0;
    root.setAttribute('role', 'application');
    root.setAttribute('aria-roledescription', 'interactive dashboard preview');

    root.addEventListener('keydown', (event) => {
      // Tab, `/`, ⌘K / Ctrl+K and friends fall through to the page, even from the
      // terminal; only ctrl-] (by key or physical key, layouts differ) is ours.
      const ctrlBracket =
        event.ctrlKey && !event.metaKey && !event.altKey && (event.key === ']' || event.code === 'BracketRight');
      const key = ctrlBracket ? CTRL_BRACKET : event.key;
      const modified = !ctrlBracket && (event.metaKey || event.ctrlKey || event.altKey);
      if ((media && !media.matches) || modified || event.isComposing || !isHandledKey(state, key)) return;
      event.preventDefault();
      // Typing into the shell doesn't flash dashboard keycaps (they're hidden anyway).
      if (state.focus === 'dashboard' || keycapFor(key) === 'Escape' || key === 'x') flash(key);
      dispatch({ type: 'key', key }, true);
    });

    root.addEventListener('click', (event) => {
      const target = event.target as Element;
      const key = target.closest<HTMLElement>('.tui-key[data-key]')?.dataset.key;
      const tab = target.closest<HTMLElement>('[data-tui-tab]')?.dataset.tuiTab as Tab | undefined;
      const row = target.closest<HTMLElement>('[data-tui-panel="instances"] [data-tui-row]')?.dataset.tuiRow as
        | Instance
        | undefined;
      const inTerm = target.closest('[data-tui-term]');
      const termTab = target.closest<HTMLElement>('[data-tui-term-tab]')?.dataset.tuiTermTab;
      root.focus({ preventScroll: true });
      // Like the real mouse routing: a click on the pane focuses it (a tab title
      // also activates that tab), a click elsewhere hands focus back.
      if (inTerm) dispatch({ type: 'focus', focus: 'terminal', term: termTab === undefined ? undefined : Number(termTab) }, true);
      else if (key) {
        flash(key);
        dispatch({ type: 'key', key }, true);
      } else if (tab && TABS.includes(tab)) dispatch({ type: 'tab', tab }, true);
      else if (row && INSTANCES.includes(row)) dispatch({ type: 'select', name: row }, true);
      else if (state.focus === 'terminal') dispatch({ type: 'focus', focus: 'dashboard' }, true);
      else stopAutoplay();
    });

    root.addEventListener('focusin', () => renderHint(state));
    root.addEventListener('focusout', (event) => {
      if (!root.contains(event.relatedTarget as Node | null)) renderHint(state);
    });
  }

  render(state);

  return {
    root,
    getState: () => state,
    dispatch: (event, { user = false } = {}) => dispatch(event, user),
    setState(next) {
      if (next === state) return;
      commit(next);
      schedule();
    },
    subscribe(listener) {
      listeners.add(listener);
      listener(state, state);
      return () => listeners.delete(listener);
    },
    stopAutoplay,
  };
}

function span(className: string, text: string) {
  const el = document.createElement('span');
  el.className = className;
  el.textContent = text;
  return el;
}

function div(text: string) {
  const el = document.createElement('div');
  // Keep empty lines one row tall.
  el.textContent = text || ' ';
  return el;
}
