/**
 * Drives the landing page's "how it works" stage from the `mini-tui` state
 * machine: the autoplay while the section is on screen, then the keyboard /
 * pointer once someone tries it. Only the desktop stage exists for this
 * script: below 981px it's `display: none` (the stacked flow is CSS-only),
 * so the observer never fires and the widget can't take focus.
 */
import {
  INITIAL,
  INSTANCES,
  STATUS_LABEL,
  TABS,
  NOTIFY_FROM,
  describe,
  isHandledKey,
  keycapFor,
  playing,
  reduce,
  selectedName,
  settled,
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

const stage = document.querySelector<HTMLElement>('[data-hiw]');
const tui = stage?.querySelector<HTMLElement>('[data-tui]');
if (stage && tui) init(stage, tui);

function init(stage: HTMLElement, tui: HTMLElement) {
  const $$ = <T extends Element = HTMLElement>(sel: string, root: ParentNode = stage) => [...root.querySelectorAll<T>(sel)];
  const vscode = stage.querySelector<HTMLElement>('[data-vscode]');
  const hint = tui.querySelector<HTMLElement>('[data-tui-hint]');
  const live = tui.querySelector<HTMLElement>('[data-tui-live]');
  const helpOverlay = tui.querySelector<HTMLElement>('[data-tui-help-overlay]');
  const desktop = window.matchMedia('(min-width: 981px)');
  const reducedMotion = window.matchMedia('(prefers-reduced-motion: reduce)').matches;

  let state: State = reducedMotion || !('IntersectionObserver' in window) ? settled() : INITIAL;
  let timer = 0;
  let messageTimer = 0;
  let visible = false;

  const setFlag = (el: Element, name: string, on: boolean) => el.toggleAttribute(name, on);

  // Writes only; nothing here reads layout, so a render never forces a reflow.
  function render(s: State) {
    const sel = selectedName(s);
    stage.dataset.selected = sel;
    stage.dataset.tab = s.tab;

    for (const el of $$('[data-sandbox-node]')) setFlag(el, 'data-selected', el.dataset.sandboxNode === sel);
    for (const el of $$('[data-connector="dashboard"]')) setFlag(el, 'data-active', el.dataset.sandbox === sel);
    for (const el of $$('[data-connector="service"]')) setFlag(el, 'data-active', el.dataset.sandbox === sel);
    for (const el of $$('[data-connector="vscode"]')) setFlag(el, 'data-active', el.dataset.sandbox === s.vscode);
    for (const name of INSTANCES) {
      const agent = stage.querySelector<HTMLElement>(`[data-agent="${name}"]`);
      if (!agent) continue;
      agent.dataset.status = s.agents[name];
      const label = agent.querySelector('[data-agent-status]');
      if (label) label.textContent = STATUS_LABEL[s.agents[name]];
    }
    if (vscode) {
      vscode.dataset.open = String(s.vscode !== null);
      // Keep the last title while it fades out.
      if (s.vscode) {
        vscode.dataset.target = s.vscode;
        for (const el of $$('[data-vscode-for]', vscode)) el.hidden = el.dataset.vscodeFor !== s.vscode;
      }
    }

    for (const el of $$('[data-tui-tab]', tui)) {
      const on = el.dataset.tuiTab === s.tab;
      setFlag(el, 'data-active', on);
      el.setAttribute('aria-pressed', String(on));
    }
    for (const el of $$('[data-tui-panel]', tui)) el.hidden = el.dataset.tuiPanel !== s.tab;
    for (const el of $$('[data-tui-panel="instances"] [data-tui-row]', tui)) setFlag(el, 'data-selected', el.dataset.tuiRow === sel);
    for (const el of $$('[data-tui-detail-for]', tui)) el.hidden = el.dataset.tuiDetailFor !== sel;
    for (const el of $$('[data-tui-unread]', tui)) {
      el.hidden = s.unread === 0;
      el.textContent = ` (${s.unread})`;
    }
    for (const el of $$('[data-tui-row-unread]', tui)) {
      const row = el.closest<HTMLElement>('[data-tui-row]');
      el.hidden = s.unread === 0 || row?.dataset.tuiRow !== NOTIFY_FROM;
    }
    for (const el of $$('[data-tui-notified]', tui)) el.hidden = !s.notified;
    for (const el of $$('[data-tui-inbox-empty]', tui)) el.hidden = s.notified;
    for (const el of $$('[data-tui-row="notify-1"]', tui)) setFlag(el, 'data-unread', s.unread > 0);
    if (helpOverlay) helpOverlay.hidden = !s.help;
    renderHint(s);
  }

  function renderHint(s: State) {
    if (!hint) return;
    hint.hidden = false;
    const focused = tui.contains(document.activeElement);
    hint.dataset.kind = s.message ? 'message' : 'hint';
    hint.textContent = s.message ?? (focused ? '? FOR KEYS · TAB LEAVES' : 'CLICK TO TRY IT');
  }

  function flash(key: string) {
    const cap = keycapFor(key);
    const el = cap && tui.querySelector<HTMLElement>(`.tui-key[data-key="${CSS.escape(cap)}"]`);
    if (!el) return;
    el.setAttribute('data-flash', '');
    window.setTimeout(() => el.removeAttribute('data-flash'), FLASH_MS);
  }

  function dispatch(e: Event, user: boolean) {
    const prev = state;
    state = reduce(state, e);
    if (state === prev) return;
    render(state);
    if (user && live) {
      const text = describe(prev, state);
      if (text) live.textContent = text;
    }
    if (state.message && state.message !== prev.message) {
      window.clearTimeout(messageTimer);
      messageTimer = window.setTimeout(() => {
        state = { ...state, message: null };
        renderHint(state);
      }, MESSAGE_MS);
    }
    if (user) stopAutoplay();
  }

  // ---- Autoplay: one timer for the pending step, dropped when off screen and
  // re-armed (with that step's full pause) when the section comes back.
  function schedule() {
    window.clearTimeout(timer);
    if (!visible || !playing(state)) return;
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

  if (playing(state)) {
    observer = new IntersectionObserver(
      ([entry]) => {
        visible = entry.isIntersecting;
        if (visible) schedule();
        else window.clearTimeout(timer);
      },
      { threshold: 0.45 },
    );
    observer.observe(stage);
  }

  // ---- Interaction. The widget is one tab stop; its buttons stay out of the
  // tab order (tabindex=-1) but work with a pointer.
  tui.tabIndex = 0;
  tui.setAttribute('role', 'application');
  tui.setAttribute('aria-roledescription', 'interactive dashboard preview');

  tui.addEventListener('keydown', (event) => {
    // Tab, `/`, ⌘K / Ctrl+K and friends fall through to the page.
    if (!desktop.matches || event.metaKey || event.ctrlKey || event.altKey || !isHandledKey(event.key)) return;
    event.preventDefault();
    flash(event.key);
    dispatch({ type: 'key', key: event.key }, true);
  });

  tui.addEventListener('click', (event) => {
    const target = event.target as Element;
    const key = target.closest<HTMLElement>('.tui-key[data-key]')?.dataset.key;
    const tab = target.closest<HTMLElement>('[data-tui-tab]')?.dataset.tuiTab as Tab | undefined;
    const row = target.closest<HTMLElement>('[data-tui-panel="instances"] [data-tui-row]')?.dataset.tuiRow as Instance | undefined;
    tui.focus({ preventScroll: true });
    if (key) {
      flash(key);
      dispatch({ type: 'key', key }, true);
    } else if (tab && TABS.includes(tab)) dispatch({ type: 'tab', tab }, true);
    else if (row && INSTANCES.includes(row)) dispatch({ type: 'select', name: row }, true);
    else stopAutoplay();
  });

  tui.addEventListener('focusin', () => renderHint(state));
  tui.addEventListener('focusout', (event) => {
    if (!tui.contains(event.relatedTarget as Node | null)) renderHint(state);
  });

  render(state);
}
