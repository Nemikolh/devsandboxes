/**
 * Drives the landing page's "how it works" stage: mounts its mini dashboard
 * (`scripts/mini-tui.ts`), autoplaying while the stage is on screen, and
 * mirrors the dashboard's state on the diagram. Only the desktop stage exists
 * for this script: below 981px it's `display: none` (the stacked flow is
 * CSS-only), so the observer never fires and the widget takes no keys.
 */
import { INSTANCES, STATUS_LABEL, selectedName, type State } from '../lib/mini-tui';
import { mountMiniTui } from './mini-tui';

const stage = document.querySelector<HTMLElement>('[data-hiw]');
const tui = stage?.querySelector<HTMLElement>('[data-tui]');
if (stage && tui) init(stage, tui);

function init(stage: HTMLElement, tui: HTMLElement) {
  const $$ = <T extends Element = HTMLElement>(sel: string, root: ParentNode = stage) => [...root.querySelectorAll<T>(sel)];
  const vscode = stage.querySelector<HTMLElement>('[data-vscode]');
  const setFlag = (el: Element, name: string, on: boolean) => el.toggleAttribute(name, on);

  // Only the story's instances have a card: others (`:run` adds `web-4`) and
  // the sandbox row select nothing here, VS Code on them shows as closed.
  const known = (name: string | null): name is (typeof INSTANCES)[number] =>
    (INSTANCES as readonly (string | null)[]).includes(name);

  // Writes only; nothing here reads layout, so a render never forces a reflow.
  function render(s: State) {
    const picked = selectedName(s);
    const sel = known(picked) ? picked : '';
    const code = known(s.vscode) ? s.vscode : null;
    stage.dataset.selected = sel;
    stage.dataset.tab = s.tab;

    for (const el of $$('[data-sandbox-node]')) setFlag(el, 'data-selected', el.dataset.sandboxNode === sel);
    for (const el of $$('[data-connector="dashboard"]')) setFlag(el, 'data-active', el.dataset.sandbox === sel);
    for (const el of $$('[data-connector="service"]')) setFlag(el, 'data-active', el.dataset.sandbox === sel);
    for (const el of $$('[data-connector="vscode"]')) setFlag(el, 'data-active', el.dataset.sandbox === code);
    for (const el of $$('[data-sb-state]')) {
      // An `rm`'d instance keeps its card as it last was.
      const status = s.instances.find((i) => i.name === el.dataset.sbState)?.status;
      if (!status) continue;
      setFlag(el, 'data-exited', status === 'exited');
      const label = el.querySelector('[data-sb-state-label]');
      if (label) label.textContent = status;
    }
    for (const name of INSTANCES) {
      const agent = stage.querySelector<HTMLElement>(`[data-agent="${name}"]`);
      // An `rm`'d instance keeps its card, with the agent it last had.
      const status = s.agents[name];
      if (!agent || !status) continue;
      // A stopped container's agent looks idle on the card, labelled `stopped`.
      agent.dataset.status = status === 'stopped' ? 'idle' : status;
      const label = agent.querySelector('[data-agent-status]');
      if (label) label.textContent = STATUS_LABEL[status];
    }
    if (vscode) {
      vscode.dataset.open = String(code !== null);
      // Keep the last title while it fades out.
      if (code) {
        vscode.dataset.target = code;
        for (const el of $$('[data-vscode-for]', vscode)) el.hidden = el.dataset.vscodeFor !== code;
      }
    }
  }

  const widget = mountMiniTui(tui, { observe: stage, media: '(min-width: 981px)' });
  widget.subscribe(render);
}
