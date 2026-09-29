/**
 * Drives the landing page's "how it works" stage from the `mini-tui` state
 * machine: the autoplay while the section is on screen, then the keyboard /
 * pointer once someone tries it. Only the desktop stage exists for this
 * script: below 981px it's `display: none` (the stacked flow is CSS-only),
 * so the observer never fires and the widget can't take focus.
 */
import {
  CTRL_BRACKET,
  INITIAL,
  INSTANCES,
  STATUS_LABEL,
  TABS,
  NOTIFY_FROM,
  describe,
  isHandledKey,
  keycapFor,
  playing,
  promptFor,
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
  const body = tui.querySelector<HTMLElement>('.tui-body');
  const termPane = tui.querySelector<HTMLElement>('[data-tui-term]');
  const termTabs = tui.querySelector<HTMLElement>('[data-tui-term-tabs]');
  const termScreen = tui.querySelector<HTMLElement>('[data-tui-term-screen]');
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
    renderTerms(s);
    renderHint(s);
  }

  // The terminal pane, rebuilt from state (a handful of nodes); text only, never HTML.
  function renderTerms(s: State) {
    const open = s.terms.length > 0;
    const term = s.terms[s.active];
    const focused = s.focus === 'terminal';
    if (body) {
      setFlag(body, 'data-terms', open);
      body.dataset.focus = s.focus;
    }
    const mode = focused && term ? (term.exited ? 'exited' : 'terminal') : 'dashboard';
    for (const el of $$('[data-tui-help]', tui)) el.hidden = el.dataset.tuiHelp !== mode;
    for (const el of $$('[data-tui-terms-hint]', tui)) el.hidden = !open;
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
      tab.textContent = `${i + 1}:${t.name}${t.exited ? ' (exited)' : ''}`;
      setFlag(tab, 'data-active', i === s.active);
      setFlag(tab, 'data-exited', t.exited);
      tabs.push(tab);
    });
    termTabs.replaceChildren(...tabs);

    const lines = term.lines.map((line) => div(line));
    if (!term.exited) {
      const input = div(promptFor(term.name) + term.input);
      if (focused) input.append(span('tui-cursor', ''));
      lines.push(input);
    }
    termScreen.replaceChildren(...lines);
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

  function renderHint(s: State) {
    if (!hint) return;
    hint.hidden = false;
    const focused = tui.contains(document.activeElement);
    hint.dataset.kind = s.message ? 'message' : 'hint';
    const keys = s.focus === 'terminal' ? 'ESC TO DASHBOARD · TAB LEAVES' : '? FOR KEYS · TAB LEAVES';
    hint.textContent = s.message ?? (focused ? keys : 'CLICK TO TRY IT');
  }

  function flash(key: string) {
    const cap = keycapFor(key);
    if (!cap) return;
    // Several hint bars share keycaps (`esc`, `x`); only the shown one is visible.
    for (const el of $$(`.tui-key[data-key="${CSS.escape(cap)}"]`, tui)) {
      el.setAttribute('data-flash', '');
      window.setTimeout(() => el.removeAttribute('data-flash'), FLASH_MS);
    }
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
    // Tab, `/`, ⌘K / Ctrl+K and friends fall through to the page, even from the
    // terminal; only ctrl-] (by key or physical key, layouts differ) is ours.
    const ctrlBracket =
      event.ctrlKey && !event.metaKey && !event.altKey && (event.key === ']' || event.code === 'BracketRight');
    const key = ctrlBracket ? CTRL_BRACKET : event.key;
    const modified = !ctrlBracket && (event.metaKey || event.ctrlKey || event.altKey);
    if (!desktop.matches || modified || event.isComposing || !isHandledKey(state, key)) return;
    event.preventDefault();
    // Typing into the shell doesn't flash dashboard keycaps (they're hidden anyway).
    if (state.focus === 'dashboard' || keycapFor(key) === 'Escape' || key === 'x') flash(key);
    dispatch({ type: 'key', key }, true);
  });

  tui.addEventListener('click', (event) => {
    const target = event.target as Element;
    const key = target.closest<HTMLElement>('.tui-key[data-key]')?.dataset.key;
    const tab = target.closest<HTMLElement>('[data-tui-tab]')?.dataset.tuiTab as Tab | undefined;
    const row = target.closest<HTMLElement>('[data-tui-panel="instances"] [data-tui-row]')?.dataset.tuiRow as Instance | undefined;
    const inTerm = target.closest('[data-tui-term]');
    const termTab = target.closest<HTMLElement>('[data-tui-term-tab]')?.dataset.tuiTermTab;
    tui.focus({ preventScroll: true });
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

  tui.addEventListener('focusin', () => renderHint(state));
  tui.addEventListener('focusout', (event) => {
    if (!tui.contains(event.relatedTarget as Node | null)) renderHint(state);
  });

  render(state);
}
