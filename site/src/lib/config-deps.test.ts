import { mkdtempSync, rmSync, writeFileSync, mkdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, describe, expect, it } from 'vitest';
import { configDeps, configDepsStamp, localImports } from './config-deps';

describe('localImports', () => {
  it('finds relative imports, re-exports and side-effect imports', () => {
    const src = [
      "import { a } from './a.ts';",
      "import type { B } from '../b';",
      "export { c } from './c';",
      "import './d';",
      "import x from 'pkg';",
      "import { y } from 'node:fs';",
    ].join('\n');
    expect(localImports(src)).toEqual(['./a.ts', '../b', './c', './d']);
  });
});

describe('configDeps', () => {
  let dir: string;
  afterEach(() => rmSync(dir, { recursive: true, force: true }));

  it('walks the local import graph, resolving extensionless and index imports', () => {
    dir = mkdtempSync(join(tmpdir(), 'config-deps-'));
    mkdirSync(join(dir, 'lib/sub'), { recursive: true });
    writeFileSync(join(dir, 'config.mjs'), "import { a } from './lib/a.ts';\nimport 'pkg';");
    writeFileSync(join(dir, 'lib/a.ts'), "import { b } from './b';\nimport { s } from './sub';\nimport { gone } from './gone';");
    writeFileSync(join(dir, 'lib/b.ts'), "import { a } from './a.ts';");
    writeFileSync(join(dir, 'lib/sub/index.ts'), '');
    expect(configDeps(join(dir, 'config.mjs'))).toEqual(
      ['lib/a.ts', 'lib/b.ts', 'lib/sub/index.ts'].map((f) => join(dir, f)),
    );
  });

  it('stamps a name that changes with the deps contents', () => {
    dir = mkdtempSync(join(tmpdir(), 'config-deps-'));
    const f = join(dir, 'a.ts');
    writeFileSync(f, 'one');
    const before = configDepsStamp([f]).name;
    expect(configDepsStamp([f]).name).toBe(before);
    writeFileSync(f, 'two');
    expect(configDepsStamp([f]).name).not.toBe(before);
    expect(before).toMatch(/^devsandboxes:config-deps-[0-9a-f]{12}$/);
  });
});
