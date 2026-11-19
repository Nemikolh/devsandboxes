/**
 * Data behind `secrets.mdx` (credentials and access, a proposal). A module
 * rather than MDX exports so `astro check` types every diagram; the
 * architecture collection only globs `*.mdx`, so this file is not a page.
 *
 * Prose goes through `md()`, as in `fleet-data.ts`: `code` becomes escaped
 * `<code>`, `**x**` becomes `<strong>`, and a raw `<` that isn't a known
 * tag throws.
 */
import type { Lane, Step } from '../../components/architecture/Sequence.astro';
import type { State } from '../../components/architecture/States.astro';
import type { MachinesState } from '../../components/architecture/ThreeMachines.astro';

const esc = (s: string) => s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
const TAG = /^<\/?(a|ul|ol|li|strong|em|br|p|code)[\s>]/;

function md(...parts: string[]): string {
  return parts
    .join(' ')
    .split(/(`[^`]*`)/)
    .map((seg, i) => {
      if (i % 2) return `<code>${esc(seg.slice(1, -1))}</code>`;
      for (const m of seg.matchAll(/</g)) {
        if (!TAG.test(seg.slice(m.index))) throw new Error(`secrets-data: raw "<" outside code: ${seg.slice(m.index, m.index + 30)}`);
      }
      return seg.replace(/\*\*(.+?)\*\*/g, '<strong>$1</strong>');
    })
    .join('');
}

/** Where credentials live: today, brokered from a home, delegated to a member. */
export const placement: MachinesState[] = [
  {
    id: 'today',
    label: 'Today',
    diagram: {
      hosts: [
        {
          id: 'L',
          role: 'laptop',
          items: [
            {
              name: 'shared-volumes/',
              icon: 'terminal',
              sub: 'files next to the config',
              body: [{ sockets: [{ name: 'gh/hosts.yml', note: 'token' }, { name: 'sentry/', note: 'token' }, { name: 'zidane-config/', note: 'OAuth' }] }],
            },
            { hop: [{ n: 1, label: 'bind mount, rw, into every sandbox' }], tone: 'runtime' },
            { name: 'sandboxes on L', icon: 'cable', body: ['logged in everywhere', 'each one can read every token'] },
            { name: 'devsandbox serve', serve: true, body: ['relays your ssh-agent to B while L grants it'] },
          ],
        },
        {
          id: 'B',
          role: 'box',
          items: [
            { name: 'devsandbox serve', serve: true },
            { hop: [{ n: 3, label: 'its own shared-volumes/, empty' }], tone: 'runtime' },
            { name: 'sandboxes on B', icon: 'cable', body: ['not logged in', 'log in again: a second copy of every token'] },
          ],
        },
      ],
      gaps: [{ dialer: 'L', dialed: 'B', links: [{ n: 2, label: 'one held link', held: true, carries: ['API PTY TUNNEL', 'AGENT while granted'] }] }],
      caption: 'Tokens are files; whoever can read the file holds the token.',
    },
    arrows: [
      { from: 'shared-volumes', to: 'sandboxes', label: '(1) mounts', detail: md('`template.base.mounts` binds `${sharedVolumes}/gh` and `sentry/` read-write (gh rewrites `hosts.yml` on refresh). Nothing scopes them: every sandbox, every profile, every repo.') },
      { from: 'L', to: 'B', label: '(2) the link', detail: md('Carries the ssh-agent (an `[[agent]]` grant by member key), never a token file.') },
      { from: 'B', to: 'sandboxes', label: '(3) nothing to mount', detail: md("B's `shared-volumes/` is B's own. zidane, gh and sentry-cli start logged out; copying the files over doubles the exposure.") },
    ],
    note: md('The only credential that already works on B is the one devsandbox brokers: the ssh-agent. The proposal generalizes that.'),
  },
  {
    id: 'brokered',
    label: 'Brokered',
    diagram: {
      hosts: [
        {
          id: 'L',
          role: 'laptop · credential home',
          items: [
            {
              name: 'devsandbox serve',
              serve: true,
              sub: 'broker: grants · policy · minting',
              body: ['credentials.toml says who gets what', { sockets: [{ name: 'github', note: 'keyring' }, { name: 'sentry-work', note: 'keyring' }, { name: 'anthropic', note: 'OAuth' }] }],
            },
            { hop: [{ n: 3, label: 'real token, upstream TLS' }], tone: 'agent' },
            { name: 'api.github.com · sentry.io · api.anthropic.com', icon: 'cable' },
          ],
        },
        {
          id: 'B',
          role: 'box · runs web-2',
          items: [
            { name: 'sandbox web-2', icon: 'terminal', body: ['GH_TOKEN, SENTRY_AUTH_TOKEN: phantoms', 'HTTPS_PROXY, base URLs: 127.0.0.1 (devsbd)'] },
            { hop: [{ n: 1, label: 'Open(http-proxy) through the bridge' }], tone: 'helper' },
            { name: 'devsandbox serve', serve: true, sub: 'instance host', body: ["TLS ends here, B's own CA", 'knows which instance asked'] },
          ],
        },
      ],
      gaps: [
        {
          dialer: 'L',
          dialed: 'B',
          links: [
            { n: 2, label: 'CRED streams', held: true, tone: 'agent', carries: ['B opens: signed claim + request', 'L answers: the response'] },
            { label: 'API PTY TUNNEL AGENT', held: true },
          ],
        },
      ],
      caption: 'The sandbox holds phantoms; only L holds tokens.',
    },
    arrows: [
      { from: 'web-2', to: 'B serve', label: '(1) a request', detail: md("gh talks HTTPS to the helper's loopback proxy; the stream arrives on web-2's bridge, so the instance is known without asking the sandbox.") },
      { from: 'B', to: 'L', label: '(2) to the home', detail: md('B forwards the decrypted request with a claim it signs with its member key: instance, sandbox, profile, linked repos. L trusts B for its own instances, as far as L granted B.') },
      { from: 'L', to: 'upstream', label: '(3) the real call', detail: md('L checks the policy, swaps the phantom for a token (minted and downscoped when the provider can) and makes the call itself.') },
    ],
    note: md('Works the same for a sandbox on L: (2) is a local call. L offline means brokered calls from B fail, like the agent today. Every credential can be brokered this way, minted or not.'),
  },
  {
    id: 'delegated',
    label: 'Delegated',
    diagram: {
      hosts: [
        {
          id: 'L',
          role: 'laptop · credential home',
          items: [{ name: 'devsandbox serve', serve: true, sub: 'credential home', body: ['keeps the GitHub App key, OAuth refresh tokens', 'mints for B on demand, revokes on withdraw or rm'] }],
        },
        {
          id: 'B',
          role: 'box · always on',
          items: [
            { name: 'sandbox web-2', icon: 'terminal', body: ['phantoms, as before'] },
            { hop: [{ label: 'Open(http-proxy)' }], tone: 'helper' },
            { name: 'devsandbox serve', serve: true, sub: 'delegate', body: ['a minted token per instance, sealed to its member key', 'expired: one extra hop to L, the request waits, never fails'] },
            { hop: [{ n: 2, label: 'upstream TLS' }], tone: 'agent' },
            { name: 'api.github.com', icon: 'cable' },
          ],
        },
      ],
      gaps: [{ dialer: 'L', dialed: 'B', links: [{ n: 1, label: 'CRED mint', held: true, tone: 'agent', carries: ['B asks: (github, web-2, [acme/web])', 'L answers: a token, exp 1 h · revocations'] }] }],
      caption: 'Only for mintable credentials (GitHub first); static ones are always brokered.',
    },
    arrows: [
      { from: 'L', to: 'B', label: '(1) mint', detail: md('`delegate = ["B"]` on a mintable credential: B asks L for a token narrowed to one instance (its repos, its permissions) when it has none or the last one expired, and holds the request meanwhile. L never sends the app key or a refresh token.') },
      { from: 'B', to: 'upstream', label: '(2) direct', detail: md('B injects and calls upstream itself. A compromised B leaks at most one instance\'s scope, for at most the token\'s life.') },
    ],
    note: md('The Kubernetes NodeRestriction rule, applied to tokens: a member only ever holds what its own instances may use. A static credential (a Sentry token, an API key) can\'t be narrowed, so delegating it would mean copying it: it stays brokered from L.'),
  },
];

const ghLanes: Lane[] = [
  { id: 'gh', label: 'gh pr create', sub: 'sandbox web-2 · on B', tone: 'client' },
  { id: 'daemon', label: 'devsbd daemon', sub: 'container · root', tone: 'helper' },
  { id: 'b', label: 'devsandbox serve', sub: 'B · instance host', tone: 'serve' },
  { id: 'l', label: 'devsandbox serve', sub: 'L · credential home', tone: 'serve' },
  { id: 'up', label: 'api.github.com', sub: 'upstream', tone: 'runtime' },
];

const ghSteps: Step[] = [
  { kind: 'phase', label: 'in the sandbox' },
  { kind: 'act', at: 'gh', label: 'GH_TOKEN=dsbx_ph_9f…', sub: 'a phantom, bound to web-2: worthless anywhere else' },
  { from: 'gh', to: 'daemon', label: 'CONNECT api.github.com:443', sub: "HTTPS_PROXY=127.0.0.1:7890, the helper's listener" },
  { from: 'daemon', to: 'b', label: 'Open(http-proxy) · Data …', sub: "channel 2 (reserved today) · on web-2's bridge", tone: 'helper' },
  { kind: 'act', at: 'b', label: 'terminate TLS', sub: "a leaf for api.github.com from B's CA; the CA key never enters a container" },
  { kind: 'act', at: 'b', label: 'POST /repos/acme/web/pulls', sub: 'phantom → credential github · instance web-2 · profile work' },
  { kind: 'phase', label: 'at the credential home' },
  { from: 'b', to: 'l', label: 'CRED: signed claim + request', sub: 'over the held link, like AGENT streams', tone: 'agent' },
  { kind: 'act', at: 'l', label: 'grant ∩ access ∩ scope → allow', sub: 'github granted to B for work · pr.write on a linked repo' },
  { kind: 'act', at: 'l', label: 'swap the phantom', sub: 'installation token for [acme/web], pull_requests:write, cached ≤ 1 h', tone: 'agent' },
  { from: 'l', to: 'up', label: 'POST /repos/acme/web/pulls', sub: 'Authorization: token ghs_…', tone: 'agent' },
  { from: 'up', to: 'l', label: '201 Created', tone: 'runtime' },
  { from: 'l', to: 'b', label: 'response', sub: 'token-shaped headers scrubbed' },
  { from: 'b', to: 'gh', label: '201, re-encrypted to the leaf' },
  { kind: 'act', at: 'l', label: 'audit line', sub: 'who · credential · route · verdict', tone: 'notify' },
];

const askLanes: Lane[] = [
  { id: 'agent', label: 'zidane', sub: 'agent in web-2', tone: 'client' },
  { id: 'b', label: 'devsandbox serve', sub: 'B · broker', tone: 'serve' },
  { id: 'l', label: 'devsandbox serve', sub: 'L · credential home', tone: 'serve' },
  { id: 'you', label: 'you', sub: 'dashboard modal · popup · CLI', tone: 'notify' },
];

const askSteps: Step[] = [
  { from: 'agent', to: 'b', label: 'POST /repos/acme/web/issues', sub: 'gh issue create, through the helper' },
  { from: 'b', to: 'l', label: 'CRED: claim + request', tone: 'agent' },
  { kind: 'act', at: 'l', label: 'policy → ask', sub: 'issues.write is an ask scope for this sandbox · the repo is linked' },
  { from: 'l', to: 'you', label: 'request: web-2 wants issues.write on acme/web', sub: 'modal: once · this instance · always (a rule) · deny', tone: 'notify' },
  { kind: 'act', at: 'l', label: 'request held', sub: 'up to 120 s, then 403 with the reason in the body' },
  { from: 'you', to: 'l', label: 'this instance', tone: 'notify' },
  { kind: 'act', at: 'l', label: 'remember the answer', sub: '(web-2, github, issues.write) until rm · listed and revocable' },
  { from: 'l', to: 'b', label: 'response', tone: 'agent' },
  { from: 'b', to: 'agent', label: '200' },
  { kind: 'phase', label: 'had you denied' },
  { from: 'b', to: 'agent', label: '403 devsandbox: denied by you', sub: 'the agent reads why; the same request is denied without asking again', tone: 'fail' },
];

const gitLanes: Lane[] = [
  { id: 'git', label: 'git push', sub: 'sandbox web-2', tone: 'client' },
  { id: 'broker', label: 'devsandbox serve', sub: 'broker (home or delegate)', tone: 'serve' },
  { id: 'up', label: 'github.com', sub: 'smart HTTP', tone: 'runtime' },
];

const gitSteps: Step[] = [
  { kind: 'act', at: 'git', label: 'insteadOf → http://127.0.0.1:7891/github.com/', sub: 'set in the system gitconfig by the gh feature · plain HTTP, no CA' },
  { from: 'git', to: 'broker', label: 'GET …/acme/web.git/info/refs?service=git-receive-pack', sub: 'through the helper, like any http-proxy stream' },
  { kind: 'act', at: 'broker', label: 'receive-pack on acme/web', sub: 'git = "read" for this sandbox → 403 here, before any pack' },
  { from: 'broker', to: 'up', label: 'info/refs, with the token', tone: 'agent' },
  { from: 'up', to: 'git', label: 'ref advertisement', tone: 'runtime' },
  { from: 'git', to: 'broker', label: 'POST git-receive-pack', sub: 'pkt-lines first: refs/heads/joan/web-2 · refs/heads/main' },
  { kind: 'act', at: 'broker', label: 'refs/heads/main ∉ branches', sub: 'opt-in: this sandbox set access.github.branches = ["joan/*"]', tone: 'fail' },
  { from: 'broker', to: 'git', label: 'report-status: ng refs/heads/main', sub: 'refused whole, nothing sent upstream; git prints a rejected ref with the reason', tone: 'fail' },
  { kind: 'phase', label: 'pushing joan/web-2 alone' },
  { from: 'git', to: 'up', label: 'the pack, streamed through unchanged', sub: 'the broker read the commands, not the objects', tone: 'agent' },
];

const expiryLanes: Lane[] = [
  { id: 'agent', label: 'zidane → gh', sub: 'web-2 on B, mid-task', tone: 'client' },
  { id: 'b', label: 'devsandbox serve', sub: 'B · delegate', tone: 'serve' },
  { id: 'l', label: 'devsandbox serve', sub: 'L · credential home', tone: 'serve' },
  { id: 'up', label: 'api.github.com', sub: 'upstream', tone: 'runtime' },
];

const expirySteps: Step[] = [
  { from: 'agent', to: 'b', label: 'GET /repos/acme/web/pulls/42', sub: 'the same phantom as hours ago: it lives until rm' },
  { kind: 'act', at: 'b', label: "web-2's token expired", sub: 'or expires within a minute: treated the same' },
  { from: 'b', to: 'l', label: 'CRED mint (github, web-2)', sub: 'the request is held, not failed', tone: 'agent' },
  { kind: 'act', at: 'l', label: 'policy again, then mint', sub: 'a grant withdrawn since is caught here' },
  { from: 'l', to: 'b', label: 'token, exp 1 h', tone: 'agent' },
  { from: 'b', to: 'up', label: 'GET /repos/acme/web/pulls/42', tone: 'agent' },
  { from: 'up', to: 'agent', label: '200, a few ms later than usual', tone: 'runtime' },
  { kind: 'phase', label: 'L unreachable' },
  { from: 'b', to: 'agent', label: '503 devsandbox: home L unreachable', sub: 'Retry-After set · a credential that is only brokered fails the same way, from the first call', tone: 'fail' },
];

export const sequences = { ghLanes, ghSteps, askLanes, askSteps, gitLanes, gitSteps, expiryLanes, expirySteps };

/** One brokered request, from the bridge to the audit line. */
export const decision: State[] = [
  {
    name: 'who',
    sub: 'the bridge, not the sandbox',
    tone: 'helper',
    notes: ['The stream arrived on an instance\'s bridge: that names the instance, its sandbox, profile and member.', 'Nothing the container sends can change it, like `devsbd notify` today.'],
    next: 'instance known',
  },
  {
    name: 'which credential',
    sub: 'phantom or upstream host',
    tone: 'serve',
    notes: ['A phantom names its credential and must belong to this instance.', 'A host no credential claims: tunneled untouched, or refused when the sandbox has an egress list.'],
    side: ['foreign phantom → 403, logged'],
    next: 'credential known',
  },
  {
    name: 'granted?',
    sub: 'home: credentials.toml',
    tone: 'agent',
    notes: ["The credential's home grants it per profile and per member, and caps the scopes.", 'Only the home widens a grant; no config, member, authority or container can.'],
    side: ['not granted → 403 "github is not granted to profile personal"'],
    next: 'granted',
  },
  {
    name: 'asked for?',
    sub: 'config: access.<credential>',
    tone: 'cli',
    notes: ['The repos are the fence: the remotes of `folder` and `folders`, plus any `access.github.repos` adds. Another repo is denied, never asked.', 'Scopes and optional branch rules come from `access` too. The effective set is request ∩ grant, checked at `run` as well.'],
    side: ['repo not linked → 403 "acme/infra is not linked to web"'],
    next: 'within the request',
  },
  {
    name: 'rule',
    sub: "the feature's scope table",
    tone: 'serve',
    notes: ['The route (`POST /repos/{repo}/pulls`, a GraphQL operation, `git-receive-pack`) maps to a named scope.', 'allow, ask or deny; deny wins; an unknown route is the scope `other`, ask by default.'],
    side: ['ask → a permission request, held', 'deny → 403 with the reason'],
    next: 'allowed',
  },
  {
    name: 'inject',
    sub: 'mint or attach',
    tone: 'agent',
    notes: ['Mint a token narrowed to the instance (GitHub App: its repos, its permissions) when the provider can; else attach the stored one. Expiry is refreshed here, never seen by the tool.', 'The response goes back with token-shaped headers removed.'],
    next: 'done',
  },
  {
    name: 'record',
    sub: 'audit',
    tone: 'notify',
    notes: ['One line per call: instance, credential, route, verdict, who decided.', 'Shown per instance in the dashboard, next to its pending and answered requests.'],
  },
];

/** Who owns what, from the tool down to the host. */
export const ownership: State[] = [
  {
    name: 'the tool',
    sub: 'gh · git · sentry-cli · zidane',
    tone: 'client',
    notes: ['Unchanged, in the common case. Reads its usual env and config: a token var, a base URL, `HTTPS_PROXY`, `insteadOf`, the system CA bundle.', 'Believes it is logged in.'],
    side: ['never: a real token'],
    next: 'env, files and a loopback port',
  },
  {
    name: 'the feature',
    sub: 'ghcr.io/devsandboxes/features/gh',
    tone: 'cli',
    notes: [
      'Installs the tool and declares a claim in `customizations.devsandbox.credentials`: upstream hosts, where the token goes, the phantom env and files, the login flow.',
      'Ships the scope table: named scopes (`pr.write`, `git.push`) over routes. Data only, no code on the host.',
      'Handles its tool\'s oddities only: a CA knob devsandbox doesn\'t set, a token file the tool insists on.',
    ],
    side: ['never: a policy decision', 'never: host code'],
    next: 'a claim, read at build',
  },
  {
    name: 'devsandbox',
    sub: 'serve · devsbd · the CLI',
    tone: 'serve',
    notes: [
      'Instance identity, the per-member CA and its trust in every container (system bundle, the common env vars), the loopback proxy, the broker, the policy engine, permission requests, the audit log.',
      'Login (`devsandbox login github`), storage, minting adapters (GitHub App, OAuth refresh, cloud STS), routing and delegation over the fleet.',
    ],
    side: ['never: knowledge of one tool'],
    next: 'a store',
  },
  {
    name: 'the host',
    sub: 'keyring · 1Password · Vault',
    tone: 'runtime',
    notes: ['Where secrets rest: the OS keyring by default, an external store through a source (`op://…`), like External Secrets in Kubernetes.'],
  },
];
