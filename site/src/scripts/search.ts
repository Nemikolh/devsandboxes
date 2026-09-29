// Client side of `Search.astro`. A native modal <dialog> gives the inert
// background, Esc and the top layer; this adds shortcuts, the combobox /
// listbox keyboard model (focus stays in the input, `aria-activedescendant`
// tracks the option), focus restore, and Pagefind, loaded on first open.
import { isApplePlatform, shortcutAction, stepIndex, toHit, type PagefindData, type SearchHit } from '../lib/search';

interface Pagefind {
  options(opts: Record<string, unknown>): Promise<void>;
  init(): Promise<void>;
  search(query: string): Promise<{ results: { data(): Promise<PagefindData> }[] } | null>;
}

// Written by the `astro:build:done` hook; a variable so Vite doesn't try to
// resolve or bundle it (dev serves the last build's copy, if any).
const PAGEFIND_URL = '/pagefind/pagefind.js';
const DEBOUNCE_MS = 120;
const MAX_HITS = 10;

let pagefind: Promise<Pagefind | null> | undefined;
function loadPagefind(): Promise<Pagefind | null> {
  pagefind ??= (import(/* @vite-ignore */ PAGEFIND_URL) as Promise<Pagefind>)
    .then(async (pf) => {
      await pf.options({ excerptLength: 22 });
      await pf.init();
      return pf;
    })
    .catch(() => null);
  return pagefind;
}

const dialog = document.getElementById('search-dialog') as HTMLDialogElement | null;
if (dialog) setup(dialog);

function setup(dialog: HTMLDialogElement) {
  const $ = <T extends HTMLElement>(sel: string) => dialog.querySelector<T>(sel)!;
  const input = $<HTMLInputElement>('[data-search-input]');
  const scroller = $('[data-search-scroll]');
  const browse = $('[data-search-browse]');
  const hits = $('[data-search-hits]');
  const hitsLabel = $('[data-search-hits-label]');
  const hitsList = $('[data-search-hits-list]');
  const empty = $('[data-search-empty]');
  const status = $('[data-search-status]');
  const arrow = $<HTMLTemplateElement>('template[data-search-arrow]');

  let restore: HTMLElement | null = null;
  let active = -1;
  let timer: number | undefined;
  let seq = 0;

  const options = () => [...(hits.hidden ? browse : hitsList).querySelectorAll<HTMLAnchorElement>('[role="option"]')];

  function setActive(i: number, scroll = true) {
    const opts = options();
    active = i;
    opts.forEach((o, j) => o.setAttribute('aria-selected', String(j === i)));
    const current = opts[i];
    if (current) {
      input.setAttribute('aria-activedescendant', current.id);
      if (scroll) current.scrollIntoView({ block: 'nearest' });
    } else {
      input.removeAttribute('aria-activedescendant');
    }
  }

  function showBrowse() {
    hits.hidden = true;
    browse.hidden = false;
    empty.hidden = true;
    status.textContent = '';
    setActive(-1);
    scroller.scrollTop = 0;
  }

  function showMessage(text: string) {
    browse.hidden = true;
    hits.hidden = true;
    empty.hidden = false;
    empty.textContent = text;
    status.textContent = text;
    setActive(-1);
  }

  function option(hit: SearchHit, i: number): HTMLAnchorElement {
    const a = document.createElement('a');
    a.className = 'search-option';
    a.href = hit.href;
    a.id = `search-hit-${i}`;
    a.tabIndex = -1;
    a.setAttribute('role', 'option');
    a.setAttribute('aria-selected', 'false');
    const icon = dialog.querySelector<HTMLTemplateElement>(`template[data-search-icon="${hit.section}"]`);
    if (icon) a.append(icon.content.cloneNode(true));
    const text = document.createElement('span');
    text.className = 'search-option-text';
    const head = document.createElement('span');
    head.className = 'search-option-head';
    const title = document.createElement('strong');
    title.textContent = hit.title;
    const badge = document.createElement('span');
    badge.className = 'search-badge';
    badge.textContent = hit.section;
    head.append(title, badge);
    text.append(head);
    if (hit.heading && hit.heading !== hit.title) {
      const heading = document.createElement('em');
      heading.className = 'search-option-heading';
      heading.textContent = hit.heading;
      text.append(heading);
    }
    const excerpt = document.createElement('small');
    // Pagefind's excerpt is escaped page text with <mark> around the matches.
    excerpt.innerHTML = hit.excerpt;
    text.append(excerpt);
    a.append(text, arrow.content.cloneNode(true));
    return a;
  }

  async function run(query: string) {
    const mine = ++seq;
    if (!query) return showBrowse();
    const pf = await loadPagefind();
    if (mine !== seq) return;
    if (!pf) {
      return showMessage(
        import.meta.env.DEV ? 'No search index yet: run `pnpm build` once, then reload.' : 'Search is unavailable right now.',
      );
    }
    const res = await pf.search(query);
    if (mine !== seq || !res) return;
    const data = await Promise.all(res.results.slice(0, MAX_HITS).map((r) => r.data()));
    if (mine !== seq) return;
    if (!data.length) return showMessage(`No results for “${query}”. Try a different term.`);
    const total = res.results.length;
    const label = total > data.length ? `${data.length} OF ${total} RESULTS` : `${total} RESULT${total === 1 ? '' : 'S'}`;
    hitsLabel.textContent = label;
    hitsList.replaceChildren(...data.map((d, i) => option(toHit(d), i)));
    browse.hidden = true;
    empty.hidden = true;
    hits.hidden = false;
    status.textContent = label.toLowerCase();
    scroller.scrollTop = 0;
    setActive(0);
  }

  function open() {
    if (dialog.open) return;
    // The drawer is modal too; close it first so search isn't stacked over it
    // (its close handler hands focus back to the menu button, our restore target).
    const drawer = document.getElementById('mobile-drawer') as HTMLDialogElement | null;
    if (drawer?.open) drawer.close();
    restore = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    dialog.showModal();
    document.body.classList.add('scroll-locked');
    input.focus();
    input.select();
    void loadPagefind();
  }

  dialog.addEventListener('close', () => {
    window.clearTimeout(timer);
    document.body.classList.remove('scroll-locked');
    restore?.focus();
    restore = null;
  });

  document.querySelectorAll('[data-search-open]').forEach((b) => b.addEventListener('click', open));
  $('[data-search-close]').addEventListener('click', () => dialog.close());

  dialog.addEventListener('click', (event) => {
    const target = event.target as Element;
    // The dialog box is the whole viewport; anything outside the panel is "outside".
    if (target === dialog) return dialog.close();
    // Following a result (possibly an anchor on this page) must not leave search over it.
    if (target.closest('a[href]')) dialog.close();
  });

  input.addEventListener('input', () => {
    window.clearTimeout(timer);
    const query = input.value.trim();
    timer = window.setTimeout(() => void run(query), query ? DEBOUNCE_MS : 0);
  });

  input.addEventListener('keydown', (event) => {
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault();
      setActive(stepIndex(active, event.key === 'ArrowDown' ? 1 : -1, options().length));
    } else if (event.key === 'Enter' && !event.isComposing) {
      const target = options()[active];
      if (!target) return;
      event.preventDefault();
      target.click();
    }
  });

  // Pointer and keyboard share one selection, like a native listbox.
  $('#search-listbox').addEventListener('pointermove', (event) => {
    const opt = (event.target as Element).closest<HTMLAnchorElement>('[role="option"]');
    const i = opt ? options().indexOf(opt) : -1;
    if (i >= 0 && i !== active) setActive(i, false);
  });

  document.addEventListener('keydown', (event) => {
    const t = event.target as HTMLElement | null;
    const editable = !!t && (t.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(t.tagName));
    const action = shortcutAction({ key: event.key, metaKey: event.metaKey, ctrlKey: event.ctrlKey, altKey: event.altKey, editable });
    if (!action || (action === 'open' && dialog.open)) return;
    event.preventDefault();
    if (action === 'toggle' && dialog.open) dialog.close();
    else open();
  });

  // The header hint says ⌘ K in the HTML; other platforms use Ctrl.
  const nav = navigator as Navigator & { userAgentData?: { platform?: string } };
  if (!isApplePlatform(nav.userAgentData?.platform || navigator.platform || '')) {
    document.querySelectorAll('.search-trigger kbd').forEach((k) => (k.textContent = 'Ctrl K'));
  }
}
