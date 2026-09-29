/**
 * Canned bodies for the mini dashboard's text views: container logs (`l`),
 * the config explorer's TOML (`enter`) and its `docker inspect` pane. The
 * config is the landing page's hero config (`pages/index.astro`), serialized
 * the way the real explorer does it (`toml::to_string_pretty`: keys sorted,
 * `extends` kept in the original and dropped when resolved), with the config
 * hash `short_hash` gives the resolved table. Inspect JSON is key-sorted too,
 * as `serde_json` prints it.
 */

export const SANDBOX = 'web';
export const SERVICE = 'postgres';
/** The config root's project id, as in the service container's name. */
export const PROJECT = '3f9c21ab';
export const SERVICE_CONTAINER = `devsandbox-svc-${PROJECT}-${SERVICE}`;
/** `short_hash` of the resolved `[sandbox.web]` (computed with the real serializer). */
export const CONFIG_HASH = 'c61bbaac526146dd';

export const SANDBOX_ORIGINAL = ['extends = "agent"', 'folder = "../web"', 'image = "node:22"', 'services = ["postgres"]'];

/** `template.agent` merged under `sandbox.web` (arrays would concatenate; none overlap here). */
export const SANDBOX_RESOLVED = [
  'caches = ["pnpm"]',
  'folder = "../web"',
  'image = "node:22"',
  'persist-shell-history = true',
  'services = ["postgres"]',
];

/** Services don't `extends`: original and resolved are the same table. */
export const SERVICE_TABLE = ['image = "postgres:16"', 'scope = "global"', '', '[env]', 'POSTGRES_PASSWORD = "dev"'];

/** A pid for the dev server on :3000, per instance (what the Ports tab's PROCESS column shows). */
export function devServerPid(name: string): number {
  const known: Record<string, number> = { web: 412, 'web-2': 398, 'web-3': 405 };
  return known[name] ?? 88;
}

/** What each instance's container logged: the dev server, reloading as its agent edits. */
export function logLines(name: string): string[] {
  const start = [
    `> web@0.1.0 dev /workspaces/${name}`,
    '> vite --port 3000 --host',
    '',
    '  VITE v5.4.8  ready in 402 ms',
    '',
    '  ➜  Local:   http://localhost:3000/',
    '  ➜  Network: http://172.18.0.4:3000/',
  ];
  const edits: Record<string, string[]> = {
    web: [
      '2:18:40 PM [vite] hmr update /src/app.ts',
      '2:24:51 PM [vite] hmr update /src/app.ts, /src/app.css',
      '2:30:12 PM [vite] page reload src/app.ts',
    ],
    'web-2': [
      '2:26:03 PM [vite] hmr update /src/api/users.ts',
      '2:29:47 PM [vite] Internal server error: src/api/users.ts: Unexpected token (41:12)',
      '2:30:05 PM [vite] hmr update /src/api/users.ts',
      '2:31:22 PM [vite] hmr update /src/api/users.ts',
    ],
    'web-3': ['2:27:15 PM [vite] hmr update /src/api/orders.ts', '2:31:58 PM [vite] page reload src/api/orders.ts'],
  };
  return [...start, ...(edits[name] ?? [])];
}

/** `docker inspect` of an instance container, pretty-printed and key-sorted. */
export function instanceInspect(name: string, running: boolean): string[] {
  return [
    '[',
    '  {',
    '    "Config": {',
    '      "Image": "node:22",',
    '      "Labels": {',
    `        "devsandbox.config_hash": "${CONFIG_HASH}",`,
    `        "devsandbox.instance": "${name}",`,
    `        "devsandbox.project": "${PROJECT}",`,
    `        "devsandbox.sandbox": "${SANDBOX}"`,
    '      },',
    `      "WorkingDir": "/workspaces/${name}"`,
    '    },',
    `    "Name": "/devsandbox-${name}",`,
    '    "State": {',
    `      "Running": ${running},`,
    `      "Status": "${running ? 'running' : 'exited'}"`,
    '    }',
    '  }',
    ']',
  ];
}

export function serviceInspect(): string[] {
  return [
    '[',
    '  {',
    '    "Config": {',
    '      "Env": [',
    '        "POSTGRES_PASSWORD=dev"',
    '      ],',
    '      "Image": "postgres:16",',
    '      "Labels": {',
    `        "devsandbox.project": "${PROJECT}",`,
    '        "devsandbox.scope": "global",',
    `        "devsandbox.service": "${SERVICE}"`,
    '      }',
    '    },',
    `    "Name": "/${SERVICE_CONTAINER}",`,
    '    "State": {',
    '      "Running": true,',
    '      "Status": "running"',
    '    }',
    '  }',
    ']',
  ];
}
