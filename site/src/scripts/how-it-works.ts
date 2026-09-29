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
  }

  const widget = mountMiniTui(tui, { observe: stage, media: '(min-width: 981px)' });
  widget.subscribe(render);
}
