// The page-wide coding-agent pick. Any `[data-agent-menu]` (the chip, an
// agent word in prose, code or a prompt) opens one shared dropdown under
// itself; picking sets `<html data-agent>`, which CSS uses to show only that
// agent's variants, and remembers it for the next page.
import { AGENT_STORAGE_KEY, AGENTS, findAgent } from '../lib/agents';

const root = document.documentElement;
let menu: HTMLElement | undefined;
let opener: HTMLElement | undefined;

function setAgent(id: string) {
  root.dataset.agent = id;
  try {
    localStorage.setItem(AGENT_STORAGE_KEY, id);
  } catch {}
}

function items(): HTMLButtonElement[] {
  return [...(menu?.querySelectorAll<HTMLButtonElement>('[role="menuitemradio"]') ?? [])];
}

function buildMenu(): HTMLElement {
  const el = document.createElement('div');
  el.className = 'agent-menu';
  el.setAttribute('role', 'menu');
  el.setAttribute('aria-label', 'Coding agent');
  el.hidden = true;
  for (const agent of AGENTS) {
    const item = document.createElement('button');
    item.type = 'button';
    item.setAttribute('role', 'menuitemradio');
    item.dataset.agentTint = agent.id;
    item.dataset.agentPick = agent.id;
    item.tabIndex = -1;
    const dot = document.createElement('span');
    dot.className = 'agent-dot';
    dot.setAttribute('aria-hidden', 'true');
    item.append(dot, agent.name);
    el.append(item);
  }
  document.body.append(el);
  return el;
}

function close(refocus: boolean) {
  if (!menu || menu.hidden) return;
  menu.hidden = true;
  opener?.setAttribute('aria-expanded', 'false');
  if (refocus) opener?.focus();
  opener = undefined;
}

function open(from: HTMLElement) {
  menu ??= buildMenu();
  opener?.setAttribute('aria-expanded', 'false');
  opener = from;
  from.setAttribute('aria-expanded', 'true');
  const current = findAgent(root.dataset.agent)?.id;
  for (const item of items()) item.setAttribute('aria-checked', String(item.dataset.agentPick === current));
  menu.hidden = false;
  // Under the opener, kept inside the viewport horizontally.
  const r = from.getBoundingClientRect();
  const left = Math.min(r.left, window.innerWidth - menu.offsetWidth - 8);
  menu.style.left = `${Math.max(8, left) + window.scrollX}px`;
  menu.style.top = `${r.bottom + 6 + window.scrollY}px`;
  (items().find((i) => i.getAttribute('aria-checked') === 'true') ?? items()[0])?.focus();
}

document.addEventListener('click', (e) => {
  const target = e.target as Element;
  const pick = target.closest<HTMLElement>('[data-agent-pick]');
  if (pick?.dataset.agentPick) {
    setAgent(pick.dataset.agentPick);
    close(true);
    return;
  }
  const trigger = target.closest<HTMLElement>('[data-agent-menu]');
  if (trigger) {
    if (trigger === opener) close(false);
    else open(trigger);
    return;
  }
  if (!target.closest('.agent-menu')) close(false);
});

document.addEventListener('keydown', (e) => {
  if (!menu || menu.hidden) return;
  if (e.key === 'Escape') {
    e.preventDefault();
    close(true);
    return;
  }
  const list = items();
  const i = list.indexOf(document.activeElement as HTMLButtonElement);
  const to =
    e.key === 'ArrowDown' ? (i + 1) % list.length
    : e.key === 'ArrowUp' ? (i - 1 + list.length) % list.length
    : e.key === 'Home' ? 0
    : e.key === 'End' ? list.length - 1
    : -1;
  if (to >= 0) {
    e.preventDefault();
    list[to].focus();
  } else if (e.key === 'Tab') close(false);
});

window.addEventListener('resize', () => close(false));
