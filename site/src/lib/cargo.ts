/** `version` from the `[package]` table of a Cargo.toml (not a workspace member's). */
export function packageVersion(cargoToml: string): string {
  let inPackage = false;
  for (const line of cargoToml.split('\n')) {
    const header = line.match(/^\s*\[([^\]]+)\]\s*$/);
    if (header) {
      inPackage = header[1].trim() === 'package';
      continue;
    }
    const m = inPackage && line.match(/^\s*version\s*=\s*"([^"]+)"/);
    if (m) return m[1];
  }
  throw new Error('Cargo.toml: no [package] version');
}
