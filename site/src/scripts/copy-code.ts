// One delegated listener for every copy button: `.code-block` ones, whether from
// a Markdown fence (Shiki transformer) or `CodeBlock.astro`, and any other
// `[data-copy-text]` root (the agent prompt), whose markup isn't the copied text.
const RESET_MS = 2000;
const timers = new WeakMap<HTMLElement, number>();

document.addEventListener('click', async (event) => {
  const button = (event.target as Element | null)?.closest<HTMLButtonElement>('[data-copy]');
  if (!button) return;
  const root = button.closest<HTMLElement>('.code-block, [data-copy-text]');
  const text = root?.dataset.copyText ?? root?.querySelector('pre')?.innerText;
  if (text === undefined) return;
  const label = button.querySelector('.copy-text');
  // Each button resets to its own label ("Copy", "Copy prompt").
  const idle = (button.dataset.copyLabel ??= label?.textContent ?? 'Copy');
  try {
    await navigator.clipboard.writeText(text.replace(/\n$/, ''));
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
      if (label) label.textContent = idle;
    }, RESET_MS),
  );
});
