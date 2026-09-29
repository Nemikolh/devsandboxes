/**
 * Port of `table` in `src/render.rs`, so the hero's `devsandbox ps` output is
 * laid out exactly like the CLI's: columns padded to the widest cell, two
 * spaces apart, the last column never padded.
 */
export function psTable(headers: string[], rows: string[][]): string[] {
  const all = [headers, ...rows];
  const widths = headers.map((_, i) => Math.max(...all.map((r) => r[i].length)));
  return all.map((row) => row.map((cell, i) => (i + 1 < row.length ? cell.padEnd(widths[i]) : cell)).join('  '));
}
