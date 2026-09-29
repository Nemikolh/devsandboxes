// One delegated listener for every `.code-block` copy button, whether it came
// from a Markdown fence (Shiki transformer) or `CodeBlock.astro`.
const RESET_MS = 2000;
const timers = new WeakMap<HTMLElement, number>();

document.addEventListener('click', async (event) => {
  const button = (event.target as Element | null)?.closest<HTMLButtonElement>('[data-copy]');
  if (!button) return;
  const pre = button.closest('.code-block')?.querySelector('pre');
  if (!pre) return;
  const label = button.querySelector('.copy-text');
  try {
    await navigator.clipboard.writeText(pre.innerText.replace(/\n$/, ''));
  } catch {
    if (label) label.textContent = 'Failed';
    return;
  }
  button.dataset.copied = '';
  if (label) label.textContent = 'Copied';
  window.clearTimeout(timers.get(button));
  timers.set(
    button,
    window.setTimeout(() => {
      delete button.dataset.copied;
      if (label) label.textContent = 'Copy';
    }, RESET_MS),
  );
});
