// One delegated listener for the show-all toggle of `collapsed` code fences
// (`lib/code-chrome.ts`). Collapsing only clips the `<pre>`, so copy still
// reads every line.
import { COLLAPSE_LABEL, expandLabel } from '../lib/code-chrome';

document.addEventListener('click', (event) => {
  const button = (event.target as Element | null)?.closest<HTMLButtonElement>('[data-collapse-toggle]');
  const block = button?.closest<HTMLElement>('.code-block');
  if (!button || !block) return;
  const collapse = button.getAttribute('aria-expanded') === 'true';
  const before = button.getBoundingClientRect().top;
  block.toggleAttribute('data-collapsed', collapse);
  button.setAttribute('aria-expanded', String(!collapse));
  const label = button.querySelector('.collapse-text');
  if (label) label.textContent = collapse ? expandLabel(Number(block.dataset.lines)) : COLLAPSE_LABEL;
  // "Show less" at the end of a long block: keep the toggle under the pointer
  // instead of leaving the reader far past the block. Instant (the page is
  // `scroll-behavior: smooth`): a jump the reader doesn't see, not motion.
  if (collapse) window.scrollBy({ top: button.getBoundingClientRect().top - before, behavior: 'instant' });
});
