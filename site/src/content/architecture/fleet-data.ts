/**
 * Data behind `fleet.mdx` (machines to fleet). A module rather than MDX
 * exports so `astro check` types every card; the architecture collection
 * only globs `*.mdx`, so this file is not a page.
 *
 * Prose goes through `md()`: `code` becomes escaped `<code>`, `**x**`
 * becomes `<strong>`, and any other raw `<` that isn't a known tag throws,
 * so `fleet-<m>.sock` can't silently vanish into the DOM.
 */
import type { Card, Layer, SourceLink } from '../../components/architecture/drill';
import type { CommandRow } from '../../components/architecture/CommandTable.astro';
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
        if (!TAG.test(seg.slice(m.index))) throw new Error(`fleet-data: raw "<" outside code: ${seg.slice(m.index, m.index + 30)}`);
      }
      return seg.replace(/\*\*(.+?)\*\*/g, '<strong>$1</strong>');
    })
    .join('');
}

const ul = (items: string[]) => `<ul>${items.map((i) => `<li>${md(i)}</li>`).join('')}</ul>`;

const BEFORE = '7716cb8';
const MIDDLE = '3ce588f';

const L = {
  plan: (rev?: string): SourceLink => ({ label: rev ? 'Fleet plan (design only)' : 'Fleet plan', path: 'docs/plans/active/fleet.md', rev }),
  machinesDoc: (rev?: string): SourceLink => ({ label: 'Machines', path: 'docs/architecture/daemon/machines.md', rev }),
  fleetDoc: (rev?: string): SourceLink => ({ label: 'Fleet', path: 'docs/architecture/daemon/fleet.md', rev }),
  status: { label: 'Status', path: 'docs/status.md' } as SourceLink,
  adr20: { label: 'ADR 0020', path: 'docs/decisions/0020-machines-over-plain-ssh-with-protocol-match.md', rev: BEFORE } as SourceLink,
  adr36: (rev?: string): SourceLink => ({ label: 'ADR 0036', path: 'docs/decisions/0036-machines-get-the-agent-over-our-relay-never-ssh-a.md', rev }),
  adr43: { label: 'ADR 0043', path: 'docs/decisions/0043-every-machine-is-a-fleet-member.md' } as SourceLink,
  adr44: { label: 'ADR 0044', path: 'docs/decisions/0044-one-fleet-authority-signs-membership.md' } as SourceLink,
  adr45: { label: 'ADR 0045', path: 'docs/decisions/0045-a-routed-stream-proves-its-origin-per-stream.md' } as SourceLink,
  adr46: { label: 'ADR 0046', path: 'docs/decisions/0046-routed-streams-are-end-to-end-sessions.md' } as SourceLink,
  src: (label: string, path: string, rev?: string): SourceLink => ({ label, path, rev }),
};

// ------------------------------------------------------------------ Why

const howLayer = (body: string) => [{ label: 'How', body: md(body) }];

export const why: Card[] = [
  {
    id: 'why-overview',
    tag: '7716cb8 → main',
    title: 'Why change anything?',
    summary: md(
      'At `7716cb8` a machine was something you could drive from here, and little more. Seven problems, and what each one is now:',
      "<br><a href='#why-capacity'>**Machines were remote controls, not capacity.**</a> Ask, and the hub picks the machine for you.",
      "<br><a href='#why-laptop'>**A laptop could use other machines but not be used by them.**</a> Whichever side can reach the other connects, both sides use it, and machines with no link between them reach each other through those that have one.",
      "<br><a href='#why-one-connection'>**Every use opened a new ssh connection, with nothing limiting it.**</a> One connection per pair of machines carries everything, both ends prove who they are on it, and each machine sets its own limits.",
      "<br><a href='#why-always-on'>**Connections only lasted while someone was looking.**</a> They stay up, every member sees the fleet through the hub, and the hub remembers what it last saw.",
      "<br><a href='#why-code'>**A sandbox only ran where its config and code were.**</a> The hub sends what's needed.",
      "<br><a href='#why-versions'>**Versions had to be upgraded by hand on every machine.**</a> Machines follow the hub's exact build.",
      "<br><a href='#why-keys'>**SSH keys needed a connection of their own.**</a> Your agent is a grant to a member, reaching it over the fleet's own streams (optional).",
      "<br><a href='#why-lending'>**Why the lending detour was dropped.**</a> Two kinds of machine became one.",
    ),
    links: [L.plan(BEFORE), L.plan(), L.adr43, L.adr44, L.adr46],
  },
  {
    id: 'why-capacity',
    tag: 'Problem 1',
    title: 'Machines were remote controls, not capacity',
    summary: md(
      'Adding a machine let you drive it from here, but you always picked the box yourself, and nothing knew which box had room.',
      '**Now `run --place` asks the hub to pick the machine:** every machine reports its CPUs, memory, disk and running count.',
    ),
    layers: howLayer(
      "A plain `run` stays on the member whose config it names. `run --place`, typed on any member, asks the hub (`fleet.place`), which scores every machine both of them reach, by a link or a route (soft caps, image already built, a small preference for staying put); the member then moves the code there over its own links. With the hub out of reach it runs here, with a note. A sandbox tied to its host (a bind outside what moves, `${localEnv:…}`, the runtime socket…) or pinned with `placement = \"home\"` stays home. See <a href='#phase-step-9'>Step 9</a>.",
    ),
    links: [L.plan(BEFORE), L.src('Placement', 'src/serve/fleet/place.rs'), L.src('Candidates', 'src/serve/fleet/placed.rs'), L.src('Ties', 'src/serve/fleet/code/mod.rs'), L.src('Capacity report', 'src/serve/capacity.rs')],
  },
  {
    id: 'why-laptop',
    tag: 'Problem 2',
    title: 'A laptop could use other machines but not be used by them',
    summary: md(
      'Every connection was an ssh login into the other machine, so a server could only use a laptop if the laptop accepted ssh logins from it. The design rules that out on purpose.',
      '**Now whichever side can reach the other makes the connection, and both sides use it.**',
    ),
    layers: howLayer(
      "The laptop runs `devsandbox machine add <hub>`. From then on the hub sees the laptop as one of its machines and can run work there, while the laptop never accepts an ssh login. The hub's other machines show on the laptop as `routed via <hub>`, and its commands reach them through the hub. See <a href='#phase-step-8'>Step 8</a>, <a href='#phase-step-10b'>Step 10b</a> and <a href='#three-machines'>Three machines</a>.",
    ),
    links: [L.adr43, L.machinesDoc(), L.src('Inbound machines', 'src/serve/machines/inbound.rs'), L.src('Route sockets', 'src/serve/fleet/routes.rs')],
  },
  {
    id: 'why-one-connection',
    tag: 'Problem 3',
    title: 'Every use opened a new ssh connection, with nothing limiting it',
    summary: md(
      'The background connection, every `run` or `stop`, every terminal, every port forward and the key forwarding each started their own ssh, and each could reach all of devsandbox on that machine.',
      '**Now one connection per pair of machines carries everything, and each machine decides how much the others may do.**',
    ),
    layers: howLayer(
      "The default, `worker`, lets other machines run and manage the sandboxes this machine already has, plus work placed on it, kept in a separate folder under a strict policy; never its settings, its machine list or host maintenance such as `gc`. `full` opens everything, and you set it only on that machine (`devsandbox machine trust worker|full`). Each end proves its member key on the link, so trust applies to a proven member, not a name. See <a href='#phase-step-7'>Step 7</a>, <a href='#phase-step-10a'>Step 10a</a> and <a href='#command-by-command'>Command by command</a>.",
    ),
    links: [L.machinesDoc(), L.src('Trust table', 'src/serve/fleet/lent.rs'), L.src('Transport', 'src/serve/fleet/transport.rs'), L.src('Key proof', 'src/serve/fleet/proof.rs')],
  },
  {
    id: 'why-always-on',
    tag: 'Problem 4',
    title: 'Connections only lasted while someone was looking',
    summary: md(
      "A machine's connection opened when a dashboard or command wanted it and dropped 60 s later, and its rows were forgotten on restart.",
      '**Now connections stay up for as long as both machines are up, and the hub remembers what it last saw.**',
    ),
    layers: howLayer(
      "Each machine a host connects to keeps that host's daemon running. A member watches the hub's federated view, so the hub's other machines show there too. The hub saves the other machines' rows (`remotes.json`), so after a restart it shows the fleet before they reconnect. See <a href='#phase-step-9'>Step 9</a>.",
    ),
    links: [L.src('Idle countdown', 'src/serve/idle.rs'), L.src('Saved rows', 'src/serve/machines/saved.rs'), L.src('Held machines', 'src/serve/machines/service.rs')],
  },
  {
    id: 'why-code',
    tag: 'Problem 5',
    title: 'A sandbox only ran where its config and code were',
    summary: md(
      'Running `web` on another box meant copying its config and checkout there by hand.',
      "**Now the hub sends what's needed.**",
    ),
    layers: howLayer(
      "When a run is placed elsewhere, the host it was typed on sends the config folder as an archive and each repo's branch, which it fetched from the git remote with its own credentials, so the other machine needs none. Only committed, pushed work moves for now; repos with no remote or unpushed work wait for step 12.",
    ),
    links: [L.src('Code transfer', 'src/serve/fleet/code/mod.rs'), L.src('File transfer', 'src/serve/fleet/transfer.rs'), L.status],
  },
  {
    id: 'why-versions',
    tag: 'Problem 6',
    title: 'Versions had to be upgraded by hand on every machine',
    summary: md(
      "If two machines' protocols didn't match, the machine just showed as unreachable.",
      "**Now machines follow the hub's exact build, downgrades included.**",
    ),
    layers: howLayer(
      'A machine that connects to the hub and sees another build updates itself (a release is downloaded, a dev build is copied over the connection) and restarts its daemon. No build is marked published yet, so only the dev-build copy works today, and only on a machine that accepts dev builds (`dev-builds = true` or `full` trust) and runs the same OS and CPU type.',
    ),
    links: [L.adr20, L.src('Update decision', 'src/serve/fleet/update.rs'), L.status],
  },
  {
    id: 'why-keys',
    tag: 'Problem 7 · optional',
    title: 'SSH keys needed a connection of their own',
    summary: md(
      'Lending your ssh keys to a machine took one more ssh, running beside the rest.',
      "**Now your agent is a grant to a member, and travels over the fleet's own streams.**",
    ),
    layers: howLayer(
      "Off by default. `machine set <m> --agent on` (or `machine add --agent`) writes a grant into this host's `member.toml`, by `m`'s member key, following its rotations. Any machine can be granted (registered, inbound or routed): it reaches your agent over its link to you, whichever end dialed, or routed through others, end to end, so no relay can grant or read it. See <a href='#phase-step-10c'>Step 10c</a>.",
    ),
    links: [L.adr36(), L.adr46, L.src('Grants', 'src/serve/fleet/grants.rs'), L.src('Agent routes', 'src/serve/fleet/agent_routes.rs'), L.src('Agent relay (removed)', 'src/commands/agent_relay.rs', BEFORE)],
  },
  {
    id: 'why-lending',
    tag: 'Steps 1–6 · superseded',
    title: 'Why the lending detour was dropped',
    summary: md(
      'Steps 1–6 added a second kind of machine: a laptop “lent” itself to a hub with `fleet join`, the hub could only see a separate lent folder, and ssh machines kept working the old way.',
    ),
    layers: howLayer(
      "That meant two ways to add a machine, and a laptop's own sandboxes were invisible to the rest of the fleet. Steps 7–8 merged the two: every machine is added with `machine add` and reached over one connection, and how much others may do is that machine's own trust setting. See <a href='#phase-steps-1-6'>Steps 1–6</a>.",
    ),
    links: [L.plan(), L.adr43, L.src('Lending (removed)', 'src/commands/lend.rs', MIDDLE)],
  },
];

// ---------------------------------------------------------- The migration

export const phases: Card[] = [
  {
    id: 'phase-before',
    tag: 'Before · 7716cb8',
    title: 'Machines over plain ssh',
    summary: md('A machine is a remote you drive over a fresh ssh per use, plus a link held while wanted.'),
    layers: [
      {
        label: 'What',
        body: md(
          '`machine add` registers an ssh target in `machines.toml`. The local daemon links to the machine\'s daemon over `ssh <m> devsandbox api --stdio`, merges its rows into snapshots and forwards requests. Everything else (verbs, terminals, CLI re-runs, forwards, agent) is a separate ssh.',
        ),
      },
      {
        label: 'How',
        body: ul([
          '`Machines` runs one `serve-machine-<name>` thread per machine. The link connects only while wanted (a `machines: true` watcher, a queued call, a `Want`) plus `Timing::linger` 60 s.',
          'Quick calls share the held link. Each forwarded verb spawns its own `ssh_dial` (`api_argv`).',
          'CLI named commands on a machine are re-run as `ssh [-t] <target> devsandbox <args>` (`cli_argv`). With `agent` on, the command holds the machine with `machines.hold`.',
          'A separate `serve-machine-<name>-agent` thread runs `ssh <m> devsandbox agent-relay --client machine:<host>`, which binds `agent-<client>.sock` on the machine (ADR 0036).',
          'The hello says `0.0.0` and the protocol must match (ADR 0020). Remote rows are memory-only. Federation is one hop.',
        ]),
      },
      {
        label: 'Modules',
        body: ul([
          '`src/serve/machines/ssh.rs`: `api_argv`, `tty_argv`, `tty_argv_with`, `cli_argv`, `tunnel_argv`, `agent_argv`, `script_argv`, `ssh_dial`, `ssh_agent_dial`, `skip_lines`',
          '`machines/service.rs`: `Machines`, `Want`, `Timing::linger`, `agent_loop`; `machines/agent.rs`: `serve`, `SENTINEL`; `machines/link.rs`: `client_name` = `machine:<hostname>`',
          '`machines/cli.rs`: `route`, `plan`, `machine_use`, `MachineUse::{Once,PerRoot,Verb,Port,Named,Refused}`, `hold`, `hold_params`, `reconnect_loop`, `session_dials`, `spawn_ssh_owning_signals`; `machines/forward.rs`: `QUICK`, `verb`',
          '`src/commands/agent_relay.rs`; `src/serve/relay.rs` (`--session-env`, `SessionEnv`, `write_sentinel`); `src/commands/tunnel.rs` (`remote_opener`); `src/serve/forwards.rs` (`batch_tunnel_argv`); `src/tui/remote.rs` (`tty_argv` tabs); `src/serve/endpoint.rs` (`agent_socket`, `agent_sockets`)',
        ]),
      },
      {
        label: 'Code',
        body: md('The link and the agent relay are two ssh argvs side by side; a link is held only while wanted.'),
        snippets: [
          {
            title: 'src/serve/machines/ssh.rs',
            lang: 'rust',
            rev: BEFORE,
            code: `pub fn api_argv(entry: &Entry, client: &str) -> Result<Vec<OsString>> {
    let args = ["api", "--stdio", "--client", client, "--session-env"];
    argv(entry, &["-T", "-o", "BatchMode=yes"], &args)
}`,
          },
          {
            title: 'src/serve/machines/ssh.rs',
            lang: 'rust',
            rev: BEFORE,
            code: `pub fn agent_argv(entry: &Entry, client: &str) -> Result<Vec<OsString>> {
    argv(entry, &["-T", "-o", "BatchMode=yes"], &["agent-relay", "--client", client])
}`,
          },
          {
            title: 'src/serve/machines/service.rs',
            lang: 'rust',
            rev: BEFORE,
            code: `//! A link connects only while wanted: while some local connection
//! watches with \`machines: true\` ([\`Hooks::watching\`]), a forwarded call
//! is queued, or a [\`Want\`] holds the machine (a forwarded verb), and for
//! [\`Timing::linger\`] after. Connected, it subscribes to`,
          },
        ],
      },
      {
        label: 'Key commits',
        body:
          md('None in range: this is the starting point. The prerequisites that landed just after it:') +
          ul([
            '`b2547f1` docs(plans): order resumable-exec, helper-rpc and fleet…',
            "`30c14bb` docs(plans): …the user's fleet decisions",
            '`65f14ff` feat(devsbd): an RPC channel the host opens on the bridge…',
            "`bed361d` feat(serve): a HelperClient over each bridge's RPC channel…",
            "`5a03e86` feat(devsbd): session attach, ls and rm over RPC… (what step 4's PTY rides)",
          ]),
      },
    ],
    links: [
      L.machinesDoc(BEFORE),
      L.adr20,
      L.adr36(BEFORE),
      L.plan(BEFORE),
      L.src('ssh argvs', 'src/serve/machines/ssh.rs', BEFORE),
      L.src('Agent relay', 'src/commands/agent_relay.rs', BEFORE),
    ],
  },
  {
    id: 'phase-steps-1-6',
    tag: 'Steps 1–6 · e8f1123..3ce588f · superseded',
    title: 'The fleet link, and lending (superseded)',
    summary: md(
      'A frame-mux link carries the API (later PTY/TUNNEL/TRANSFER/AGENT). Laptops **lend** themselves by dialing the hub. ssh machines stay on per-use ssh. Two kinds of machine; steps 7–8 replaced this.',
    ),
    layers: [
      {
        label: 'What',
        body: md(
          'A laptop runs `devsandbox fleet join <hub>` (or `lend on`). Its daemon dials the hub, and the hub lists it as an `offered` machine reached only over that link, with no ssh into the laptop. Hub mode (`[serve] hub = true` in `daemon.config.toml`) never idles, saves remote rows and places runs on offered workers. Over steps 2–6 the hub also gets: self-update, placement and code transfer, terminals/forwards/agent over the link, the remaining commands as API calls, and `member.toml` with `worker`/`full` trust.',
        ),
      },
      {
        label: 'How',
        body: ul([
          "The worker's `serve-lend` thread (`fleet/worker.rs`, `Lending`) holds `ssh <hub> devsandbox fleet-link --worker <name> --root <fleet root> [--drain]` (`worker::link_argv`) while `lend.toml` says `on`|`drain`. It is the answerer of `fleet::LINK` (sentinel `devsandbox-fleet 1`).",
          'On the hub, `fleet-link --worker` is the opener: it binds `fleet-<worker>.sock` + `.pty/.tunnel/.transfer` and sends `fleet.offer` (`fleet/offer.rs`), which writes an `offered = true` entry (`machines::Kind::Offered`).',
          'Hub streams are served on the worker by the confined table (`fleet::lent`: first the `Worker` table over the fleet root only; step 6 made it the trust table, which also admits the member\'s own roots).',
          'Placement considered only offered workers (`placed::workers` filtered `MachineKind::Offered`). The worker binds `agent-fleet-<hub>.sock` so the **hub\'s** agent reaches the worker\'s instances.',
          'ssh machines kept `api_argv`/`tty_argv`/`cli_argv`/`tunnel_argv`/`agent_argv`. In step 6 their API relay got `link.attach {peer}` so the trust table applied to that link (`aa25738`), but the per-use argvs still bypassed trust.',
        ]),
      },
      {
        label: 'Modules (at 3ce588f)',
        body: ul([
          '`src/serve/fleet/`: `mod.rs` (`LINK`), `link.rs`, `worker.rs` (`Lending`, `link_argv`), `lender.rs`, `lend.rs`, `offer.rs` (`fleet.offer`, `fleet.agent`), `lent.rs` (Worker/trust table), `member.rs`, `update.rs`, `place.rs`, `placed.rs`, `transfer.rs`, `code/`, `pty.rs`, `pty_client.rs`, `tunnel.rs`, `agent.rs`',
          '`src/commands/lend.rs` (`lend show|on|off|drain`, `fleet join|leave`); `src/commands/fleet_link.rs` (`--worker/--root/--drain`)',
          '`src/serve/machines/mod.rs` (`Kind::{Ssh,Offered}`); `src/serve/capacity.rs`; `src/worker_policy.rs`; `src/serve/machines/saved.rs` (`remotes.json`)',
        ]),
      },
      {
        label: 'Code',
        body: md('The laptop dials the hub; placement only looks at machines that dialed in.'),
        snippets: [
          {
            title: 'src/serve/fleet/worker.rs',
            lang: 'rust',
            rev: MIDDLE,
            code: `pub fn link_argv(hub: &str, name: &str, root: &Path, draining: bool) -> Result<Vec<OsString>> {
    check_name(name)?;
    let root = root.to_str().with_context(|| format!("fleet root {} is not UTF-8", root.display()))?;
    let mut args = vec!["fleet-link", "--worker", name, "--root", root];
    if draining {
        args.push("--drain");
    }
    ssh::target_argv(hub, &["-T", "-o", "BatchMode=yes"], "devsandbox", &args)
}`,
          },
          {
            title: 'src/serve/fleet/placed.rs',
            lang: 'rust',
            rev: MIDDLE,
            code: `/// The machines placement may pick besides this host: offered fleet
/// workers only, the hosts code moves to (through their link). An ssh
/// machine is reached with \`--machine\` or a qualified name.
pub fn workers(machines: &[Machine]) -> impl Iterator<Item = &Machine> {
    machines.iter().filter(|m| m.kind == MachineKind::Offered)
}`,
          },
        ],
      },
      {
        label: 'Key commits',
        body: ul([
          '**1 Fleet link + hub mode:** `e8f1123` one link primitive for host-to-host muxes…; `ca191f5` a hub accepts worker offers as offered machines…; `782b07d` a lending worker dials its hub and serves it a confined Worker table…; `e19a8f5` lend and fleet join|leave…; `3a65d5b` check every container a fleet-root config creates against the worker\'s lending policy…; `c5189f1` hub mode keeps the daemon and every worker link up and saves remote rows…; `f59a05d` each daemon reports capacity…',
          "**2 Self-update:** `b7fd80b` a TRANSFER channel on the fleet link, size- and hash-checked…; `2b2bb3c` a worker follows its hub's exact published build, downgrades included…; `b214318` a hub on a dev build pushes its own binary…",
          "**3 Placement v0 + transfer:** `2c10a2d` key image tags and feature build dirs by config root…; `8d54318` a pure place over soft caps…; `3c8b19e` move a sandbox's config root as a tar and its repos as hub-cloned bundles…; `230d22a` run places a sandbox on the roomiest lending worker…",
          "**4 Interactive over the link:** `f87d0f2` a link serves a table of channels and either end opens any the peer serves…; `de317bc` a worker serves PTY streams as helper session attaches…; `5435dca` exec and dashboard terminals on an offered worker attach over the fleet link's PTY channel…; `1a2aaf4` port forwards… ride the fleet link's TUNNEL channel…; `f0320ad` the hub's ssh agent reaches an offered worker's instances over… AGENT…; `67a9dd5` logs, inspect, done, rename, ls and status on an offered worker run as API calls…",
          '**5 The rest over the link:** `8e5707c` ps and stats…; `f0308d9` service ls and a followed build log…; `9abbf50` a non-tty exec… as a piped PTY stream…; `8b3e040` service terminals… and exec --service…',
          "**6 Member file + trust:** `9f29d4d` member.toml holds a member's name, trust, roots…; `952c42e` …so trust, not lending, decides what a remote caller reaches; `2281189` machine add records the picked roots…; `aa25738` an ssh machine's api relay attaches its connection… with link.attach…; `1d956a0` gc on an offered worker is a daemon verb its member's full trust admits and worker trust refuses…; `3ce588f` docs: mark step 6 shipped.",
          '**Design pivot, inside this range:** `8f0ee37` one kind of machine…; `5d9cecb` links are the ssh pairs machine add makes, routed over a graph…; `709466f` split one kind of machine into steps 5-9…',
        ]),
      },
    ],
    links: [
      L.fleetDoc(MIDDLE),
      { label: 'ADR 0042', path: 'docs/decisions/0042-fleet-workers-follow-their-hubs-version.md', rev: MIDDLE },
      L.src('Lending worker', 'src/serve/fleet/worker.rs', MIDDLE),
      L.src('lend and fleet join', 'src/commands/lend.rs', MIDDLE),
      L.src('Offers', 'src/serve/fleet/offer.rs', MIDDLE),
    ],
  },
  {
    id: 'phase-step-7',
    tag: 'Step 7 · b1cd7d3..23ecf62',
    title: 'One link per machine pair',
    summary: md(
      'Every ssh machine is held as a symmetric `fleet-link --peer` link, always, and every client reaches it through `fleet-<m>.*.sock`.',
    ),
    layers: [
      {
        label: 'What',
        body: md(
          'The dialing end and the dialed end both bind local sockets and both answer streams, so "who dialed" no longer decides who can use the link. ssh machines move onto that link. `Want`, linger, `machines.hold` and the separate agent-relay thread go away. Lending\'s links and the per-use ssh argvs stay until step 8.',
        ),
      },
      {
        label: 'How',
        body: ul([
          '`fleet/transport.rs` `Transport::dial(target, program, args) -> Duplex` is the only way bytes cross (ssh its one impl). `link::opener_with`/`answerer_with` share one post-handshake body over an `Endpoint` (OUT sockets + `Serves` table).',
          "The dialer (`fleet::dialer::Dialer::hold`, thread `serve-machine-<m>-link`) dials `fleet-link --peer <own member name>`, binds `fleet-<m>.{sock,pty,tunnel,transfer}.sock`, and serves the peer's streams in-process (`Shared::open_lent`).",
          'The dialed end (`commands::fleet_link::run_peer`) binds `fleet-<dialer>.*.sock` (+ `agent-link-<dialer>.sock` while the dialer serves AGENT) and hands each stream to its daemon with `link.attach {peer, channel}` as the first line (`attach_stream`). It announces the dialer as an inbound neighbor (`links.inbound`).',
          "Both sides end in `Shared::serve_channel` under the receiving host's trust table, with fleet root `fleet/<peer>/`. Versions follow the hello's `hub` flag whichever end dialed.",
          "A daemon with machines still idled out after step 7 alone; step 8 made each dialed link a holder (`idle::Holders::links`).",
        ]),
      },
      {
        label: 'Modules',
        body: ul([
          '`src/serve/fleet/transport.rs` (`Transport`, `Transport::ssh`); `src/serve/fleet/link.rs` (`Endpoint`, `Serves`, `Outlet`, `connect`, `opener_with`, `answerer_with`, `run`, `splice`); `src/serve/fleet/dialer.rs` (`Dialer::hold`, `link_args`, `member_name`, `dial`, `Open`)',
          '`src/commands/fleet_link.rs` (`fleet_link_peer`, `run_peer`, `attach_stream`, `Announce`, `follows`, `push_build`)',
          '`src/serve/daemon.rs` (`link_attach`, `links_inbound`, `machine_dialer`, `Shared::serve_channel`, `Shared::open_lent`, `agent_for`); `src/serve/fleet/lent.rs` (`Peer`, `Channel`, `attach_params`, `peer_fleet_root`)',
          '`src/serve/machines/inbound.rs` (`Neighbors`, `Guard`); `src/serve/endpoint.rs` (`fleet_socket`, `link_agent_socket`)',
        ]),
      },
      {
        label: 'Code',
        body: md('One transport seam, and the dialed end binding one socket as the dialer does; each connection names its channel in its first byte.'),
        snippets: [
          {
            title: 'src/serve/fleet/transport.rs',
            lang: 'rust',
            code: `pub fn ssh() -> Transport {
    Transport(Arc::new(|target, program, args| ssh::spawn(&ssh_argv(target, program, args)?)))
}`,
          },
          {
            title: 'src/serve/fleet/transport.rs',
            lang: 'rust',
            code: `fn ssh_argv(target: &str, program: &str, args: &[String]) -> Result<Vec<OsString>> {
    ssh::target_argv(target, &["-T", "-o", "BatchMode=yes"], program, args)
}`,
          },
          {
            title: 'src/commands/fleet_link.rs',
            lang: 'rust',
            code: `    let socket = endpoint::fleet_socket(&dir, peer);`,
          },
          {
            title: 'src/serve/fleet/link.rs',
            lang: 'rust',
            code: `pub fn connect(socket: &Path, channel: u8) -> io::Result<UnixStream> {
    let mut stream = UnixStream::connect(socket)?;
    stream.write_all(&[channel])?;
    Ok(stream)
}`,
          },
        ],
      },
      {
        label: 'Key commits',
        body: ul([
          '`8660be9` docs(fleet): split one link per machine pair from one kind of machine… decisions 23-26',
          '`b1cd7d3` refactor(fleet): links dial through one transport seam and both link roles share one body…',
          "`4286ca8` feat(fleet): link.attach takes a channel and hands the connection to that channel's server under the peer's trust…",
          "`6af5483` feat(fleet): fleet-link --peer binds the dialer's sockets, hands its streams to the daemon with link.attach and announces it as an inbound neighbor…",
          '`e5b8f78` feat(machine): every ssh machine is held as a fleet-link --peer link whose sockets every client dials, its agent over AGENT…',
          '`23ecf62` docs(fleet): mark step 7 shipped',
        ]),
      },
    ],
    links: [
      L.plan(),
      L.fleetDoc(),
      { label: 'API reference', path: 'docs/reference/api.md' },
      L.src('Transport', 'src/serve/fleet/transport.rs'),
      L.src('fleet-link --peer', 'src/commands/fleet_link.rs'),
      L.src('Dialer', 'src/serve/fleet/dialer.rs'),
    ],
  },
  {
    id: 'phase-step-8',
    tag: 'Step 8 · 2e1bd6d..08e8279',
    title: 'One kind of machine',
    summary: md(
      "Every use of every machine goes over its link's channels, inbound neighbors become machines, and lending is deleted.",
    ),
    layers: [
      {
        label: 'What',
        body: md(
          'The CLI, dashboard terminals, Ports-tab forwards and `port` reach every machine via `fleet-<m>.*.sock`. The per-use ssh argvs, `agent-relay`, `devsandbox tunnel` and `--session-env` are deleted, leaving ssh only for the link and `machine add`. A host that was dialed treats the dialer as a machine (kind `inbound`), so a laptop joins with `machine add <hub>`. `fleet join|leave`, `lend`, `lend.toml`, `offered` entries and `drain` are removed.',
        ),
      },
      {
        label: 'How',
        body: ul([
          '**8a:** `machines/cli.rs` `plan`/`route` + `cli/calls.rs` (`use_of`) make named commands API calls with `machine` and make `exec` a `PTY` dial (`fleet::pty_client::dial`).',
          '**8b:** `tui/remote.rs` tabs run local `devsandbox exec … -- <m>/<name>` (`on_machine`), and `forwards.rs` `via()` / `port.rs` always `tunnel::Via::Fleet`.',
          '**8c:** delete `tty_argv*`, `cli_argv`, `tunnel_argv`, `api_argv`, `agent_argv`, `machines/agent.rs`, `commands/agent_relay.rs`, and the sentinel inside link API streams (`Over::Link`).',
          '**8d:** `Machines` holds a link over `fleet-<n>.sock` per announced inbound neighbor, and `placed::workers` returns every machine.',
          '**8e:** delete lending (`fleet/worker.rs`, `lender.rs`, `lend.rs`, `offer.rs`, `commands/lend.rs`, `Kind::Offered`). `machines.toml` with `offered = true` now fails to parse.',
        ]),
      },
      {
        label: 'Modules',
        body: ul([
          '`src/serve/machines/cli.rs` (`plan`, `route`, `machine_use`, `MachineUse::Calls`, `reconnect_loop`, `pipe_mode`, `service_mode`, `attach_mode`, `foreign_link_sockets`); `src/serve/machines/cli/calls.rs` (`use_of`, `Use`, `Refused`, `answer_error`, `named`, `per_root`, `listing`, `follow_build_log`)',
          '`src/serve/machines/service.rs` (`Hold`, `Held`, `with_inbound`, `carried`); `src/serve/machines/inbound.rs`; `src/serve/machines/mod.rs` (`Kind::{Ssh,Inbound}`, `MachineKind`); `src/serve/machines/ssh.rs` (only `probe_argv`, `script_argv`, `target_argv`)',
          '`src/tui/remote.rs` (`on_machine`, `service_argv`); `src/serve/forwards.rs` (`via`); `src/commands/port.rs`, `src/commands/tunnel.rs` (`Via::Fleet`, `remote_opener`)',
          '`src/serve/fleet/placed.rs` (`workers`); `src/serve/idle.rs` (`Holders::links`)',
        ]),
      },
      {
        label: 'Code',
        body: md("Placement's candidates are every machine; a client reaches a machine only through its link's socket."),
        snippets: [
          {
            title: 'src/serve/fleet/placed.rs',
            lang: 'rust',
            code: `/// The machines placement may pick besides this host: every one, whatever
/// its kind (the candidates are every neighbor); [\`place::reachable\`]
/// keeps those whose link is up.
pub fn workers(machines: &[Machine]) -> impl Iterator<Item = &Machine> {
    machines.iter()
}`,
          },
          {
            title: 'src/serve/fleet/dialer.rs',
            lang: 'rust',
            code: `pub fn dial(socket_dir: PathBuf) -> Dial {
    Arc::new(move |entry: &Entry| {
        let path = endpoint::fleet_socket(&socket_dir, &entry.name);
        let stream =
            endpoint::Stream::connect(&path).map_err(|e| anyhow::anyhow!("its link isn't up ({}: {e})", path.display()))?;
        Ok(Duplex::socket(stream))
    })
}`,
          },
        ],
      },
      {
        label: 'Key commits',
        body: ul([
          '`2e1bd6d` docs(fleet): break step 8 into 8a-8f and record decisions 27-30…',
          '`90c7ee3` feat(machine): CLI re-runs on every machine are API calls and PTY dials over its link…',
          "`bfc7846` feat(machine): dashboard terminals, forwards and port reach every machine over its link's PTY and TUNNEL channels…",
          "`a83207c` refactor(machine): delete the per-use ssh argvs, the agent relay, devsandbox tunnel's ssh half and the relay's session env…",
          '`dd4d2ed` feat(machine): a dialed host holds a link to each inbound neighbor and a hub places on every connected machine…',
          '`b9cab24` feat(fleet): drop lending, offered machines and drain, and count each dialed link as a holder… (54 files, −4949 lines)',
          '`08e8279` docs(fleet): mark step 8 shipped',
        ]),
      },
    ],
    links: [
      L.plan(),
      L.machinesDoc(),
      L.fleetDoc(),
      L.adr43,
      L.src('CLI calls', 'src/serve/machines/cli/calls.rs'),
      L.src('Inbound machines', 'src/serve/machines/inbound.rs'),
    ],
  },
  {
    id: 'phase-step-9',
    tag: 'Step 9 · a1bb25d..d486014',
    title: 'Membership and the hub',
    summary: md(
      "The hub is a member setting, `machine add` makes both ends one fleet, every member sees the fleet through the hub, and a member's `run --place` asks the hub where to go.",
    ),
    layers: [
      {
        label: 'What',
        body: md(
          '`hub` in `member.toml` (`self` on the hub, else the hub\'s name) replaces `[serve] hub = true`, and every link\'s hello names it, so two fleets never link. `machine add` joins the target to this host\'s fleet, this host to the target\'s, or starts a fleet of two (`--hub` makes the target its hub); two hubs and a name the fleet has are refused. A member watches the hub\'s federated view, so it sees the hub\'s other machines. `machine rm` takes a member out of the fleet; the old `rm` is `machine unlink`. After step 9, a typed `run` stays on the member whose config it names and only `run --place` asks the hub (decisions 38, 40).',
        ),
      },
      {
        label: 'How',
        body: ul([
          '**9a:** the daemon reads `hub` at start and on `member.set`; a change cycles its links (`service::Handle::cycle`) and sends `links.cycle` to each `fleet-link` it\'s dialed on. The hello is `<build> <hub|member> <hub name|->`, checked by `update::same_fleet`.',
          '**9b:** `machine add` reads both ends\' `member.get` (the target\'s over the add-time probe), applies the join table (`machines::join`) and pins both members\' names; `self` is reserved.',
          "**9c:** a member watches the hub's fleet root for it with `machines: true` (served on the hub only; the asking member's rows never sent back). Machines seen that way are `via` the hub, one hop (`snapshots::fleet_view`).",
          '**9d:** `machine rm` runs `fleet.leave` / `fleet.forget` from the hub (`machines::leave`, admitted by `lent::admit_fleet`); a member\'s is sent there as `fleet.rm`.',
          "**9e:** a member's run asks the hub's `fleet.place`, which answers `{on: requester|hub|machine, machine, lines}`, and moves its code there over its own links. Later, `2c28f0f` made placement opt-in (`run --place`), `17e88ea` keeps a tied sandbox home (`fleet::code::ties`) and `d9e7ead` adds `placement = \"home\"`.",
        ]),
      },
      {
        label: 'Modules',
        body: ul([
          '`src/serve/fleet/member.rs` (`Member::hub`); `src/serve/fleet/update.rs` (`same_fleet`, `hub_of`); `src/serve/machines/service.rs` (`Handle::cycle`)',
          '`src/serve/machines/join.rs`; `src/serve/machines/leave.rs`; `src/serve/fleet/lent.rs` (`admit_fleet`)',
          '`src/serve/snapshots.rs` (`fleet_view`, `merge`); `src/serve/fleet/placed.rs` (`PlaceOn`, `Placed`, `member_workers`); `src/serve/fleet/code/mod.rs` (`ties`)',
        ]),
      },
      {
        label: 'Code',
        body: md("The hub is one field of the member file; a link between two fleets is refused at the hello; the hub's answer says where a run goes."),
        snippets: [
          {
            title: 'src/serve/fleet/member.rs',
            lang: 'rust',
            code: `    /// The fleet's hub ([\`Member::hub\`]): \`self\` on the hub, else the hub's
    /// member name; empty is in no fleet (fleet plan decision 1).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub hub: String,`,
          },
          {
            title: 'src/serve/fleet/update.rs',
            lang: 'rust',
            code: `pub fn same_fleet(peer: &str, ours: &str, theirs: &str) -> Result<(), String> {
    match (hub_of(ours), hub_of(theirs)) {
        (Some(ours), Some(theirs)) if ours != theirs => {
            Err(format!("\`{peer}\` is in \`{theirs}\`'s fleet, this host in \`{ours}\`'s: a link between two fleets is refused"))
        }
        _ => Ok(()),
    }
}`,
          },
          {
            title: 'src/serve/fleet/placed.rs',
            lang: 'rust',
            code: `pub enum PlaceOn {
    /// The member that asked.
    Requester,
    /// The hub itself.
    Hub,
    /// Another machine (\`PlaceAnswer::machine\`).
    Machine,
}`,
          },
        ],
      },
      {
        label: 'Key commits',
        body: ul([
          '`a1bb25d` docs(fleet): break step 9 into 9a-9f and record decisions 31-35…',
          "`98d5c06` feat(fleet): the hub is member.toml's hub, named in every link's hello and followed at runtime…",
          "`581389b` feat(machine): machine add joins the target to this host's fleet, this host to the target's, or starts a fleet of two, refusing two hubs and a name the fleet has…",
          "`582b629` feat(fleet): a member watches its hub's federated view and shows the hub's other machines as view-only via rows…",
          "`001ccfa` feat(machine): machine rm takes a member out of the fleet through the hub's fleet.leave/forget… machine unlink keeps the old…",
          "`e832892` feat(fleet): a member's run asks the hub's fleet.place where to go and moves its code there over its own links…",
          '`d486014` docs(fleet): mark step 9 shipped',
          '**After, decisions 36-44:** `fe72296` docs; `4af90c0` machine add takes no config roots when none are picked…; `d4455ec` machine add --hub…; `2c28f0f` a typed run stays on the member whose config it names and only run --place asks the hub…; `17e88ea` run --place keeps a sandbox home when its config ties it there…; `d9e7ead` placement = "home"…',
        ]),
      },
    ],
    links: [L.plan(), L.fleetDoc(), L.machinesDoc(), L.src('Join rule', 'src/serve/machines/join.rs'), L.src('Leaving', 'src/serve/machines/leave.rs'), L.src('Candidates', 'src/serve/fleet/placed.rs')],
  },
  {
    id: 'phase-step-10a',
    tag: 'Step 10a · 2486e9b..bf5cd91',
    title: 'Member identity',
    summary: md(
      'Every member has a key, the fleet\'s authority signs who belongs in a hash-chained log, and both ends of every link prove their keys against it.',
    ),
    layers: [
      {
        label: 'What',
        body: md(
          "Each member holds an ed25519 key in `member.key` (0600; `machine key` prints its fingerprint). The member that made the fleet is its authority: it signs every membership change into `membership.json`. `machine add` shows the joining key's fingerprint to confirm (`--yes` off a TTY). An add on another member is an introduction, capped by its introducer and proposed to the authority. `machine rm` revokes the key and `machine key rotate` replaces it: signed on the authority, elsewhere proposed and approved there with `machine pending|approve|deny`. A link that doesn't prove its keys is refused, and no container may bind or build from a path covering the key. ADR 0044.",
        ),
      },
      {
        label: 'How',
        body: ul([
          '**10a.1:** `KeyStore`, its file `member.key`; `Statement {signer, key, kind, payload_sha256, version, sig}`, older versions refused.',
          '**10a.2:** the log is `Genesis`, `Add`, `Revoke`, `Rotate` (countersigned by the old key) and `Hub` entries, each signed by the authority; `apply` verifies them, `compare` says same, behind, ahead or fork. Introductions sit beside the chain.',
          '**10a.3–10a.4:** `machine add` signs `Genesis` + `Add` (a new fleet), `Add` (on the authority) or an introduction. After the hellos the link runs `fleet::proof` on the raw pipe: names, nonces and heads, a `sync` round carrying the entries the end behind lacks, then each end signs the transcript. A fork refuses the link; `fleet::LINK`\'s sentinel is at version 2.',
          '**10a.5:** `lent::Peer` carries the proven key and its introducer; `lent::admit_fleet` admits the hub-only calls by the hub\'s key in the log, not the announced name.',
          '**10a.6–10a.7:** changes are pushed to every machine the authority holds and flooded on (`fleet.sync`); a rotation\'s `member.key.next` is promoted once a log lists it (`membership_api::settle`, `Shared::key_rotated`). `key_guard` refuses binds and build paths covering the key.',
        ]),
      },
      {
        label: 'Modules',
        body: ul([
          '`src/serve/fleet/identity.rs` (`KeyStore`, `Statement`); `src/serve/fleet/membership.rs` (`Op`, `Log`, `Compare`, introductions); `src/serve/fleet/membership_api.rs` (`membership.get|adopt|sign`, `sync`, `settle`)',
          '`src/serve/fleet/proof.rs` (the key proof, `Identity`, `resolve`); `src/serve/fleet/proposals.rs` (`proposals.json`); `src/serve/machines/changes.rs` (`revoke`, `can_rotate`, `propose`)',
          '`src/key_guard.rs`; `src/commands/machine.rs` (`key`, `pending`, `approve`, `deny`)',
        ]),
      },
      {
        label: 'Code',
        body: md('What the authority signs, and what two ends learn when they compare heads.'),
        snippets: [
          {
            title: 'src/serve/fleet/membership.rs',
            lang: 'rust',
            code: `    Add { name: String, key: PublicKey },
    /// \`name\`, whose key is \`key\`, leaves; \`key\` is revoked for good.
    Revoke { name: String, key: PublicKey },
    /// \`name\`'s key goes from \`old\` to \`new\`; \`by_old\` is \`old\`'s
    /// signature over the change ([\`countersign_rotate\`]), so the
    /// …
    Rotate { name: String, old: PublicKey, new: PublicKey, by_old: Signature },
    /// The hub is now the member \`name\`.
    Hub { name: String },`,
          },
          {
            title: 'src/serve/fleet/membership.rs',
            lang: 'rust',
            code: `pub enum Compare {
    Same,
    /// Theirs is longer: we lack its later entries ([\`Log::extend\`] when
    /// they arrive confirms they continue ours).
    Behind,
    /// Ours continues theirs: they lack our later entries
    /// ([\`Log::entries_after\`]).
    Ahead,
    /// The logs differ: \`another_fleet\` when their genesis isn't ours,
    /// else one authority signed two histories.
    Fork { another_fleet: bool },
}`,
          },
        ],
      },
      {
        label: 'Key commits',
        body: ul([
          '`c0b31aa` docs(fleet): make H the only authority, cap introduced members by their introducer and add end-to-end sessions as step 10c…',
          '`2486e9b` docs(fleet): break step 10a into eight sub-steps and record decisions 48-52…',
          '`fbce32b` feat(fleet): every member has an ed25519 key in a 0600 member.key…',
          "`7015c9a` feat(fleet): a hash-chained membership log signed by the fleet's authority…",
          "`418a3ea` feat(machine): machine add signs the joining machine into the fleet's membership log…",
          '`876a3cc` feat(fleet): both ends of a link prove their member keys against the membership log after the hellos…',
          "`ca3943b` feat(fleet): a link peer carries the key it proved and its introducer, the hub-only fleet calls are admitted by the hub's key…",
          "`3c45e63` feat(fleet): machine rm revokes the member's key…; `cc080f1` machine key rotate…",
          '`6d3421f` feat(run): refuse any container bind or build path that reaches the member key…',
          '`bf5cd91` docs(fleet): close step 10a with ADR 0044…',
        ]),
      },
    ],
    links: [L.plan(), L.fleetDoc(), L.adr44, L.src('Membership log', 'src/serve/fleet/membership.rs'), L.src('Key proof', 'src/serve/fleet/proof.rs'), L.src('Key guard', 'src/key_guard.rs')],
  },
  {
    id: 'phase-step-10b',
    tag: 'Step 10b · 840787f..f10b5c9',
    title: 'Routing over the graph',
    summary: md(
      'Members report their links, every member keeps the graph, and a member with no link of its own is reached hop by hop, its origin proven per stream.',
    ),
    layers: [
      {
        label: 'What',
        body: md(
          "A member reached only through others is a **routed machine** (`machine ls`: `routed via <m>`), taking quick calls, verbs, terminals and forwards over its route. Proposals, removals and `run --place` reach their member over any path. Each relay checks its next hop is up and splices; the destination proves the stream's origin by its key and serves it under its trust for that origin, never the relay's. `machine trust worker` warns, and `machine ls` and the Machines tab show, when a machine the hub's side controls dials this one (`exposed via …`). ADR 0043 accepted (0041 removed), ADR 0045.",
        ),
      },
      {
        label: 'How',
        body: ul([
          '**10b.1–10b.2:** each member signs a report of its links (an `identity::Statement` of kind `links`, versioned by its clock) and sends `fleet.report` to the hub; `fleet.graph {reports}` is both the push and the link-up exchange, and every member floods changes on. `Graph::paths` orders direct, fewest hops, always-on hops, oldest; at most `MAX_HOPS` (4).',
          "**10b.3–10b.4:** a `ROUTE` stream opens with a `Header {dest, origin, channel, path, ttl}`; each relay runs `relay_header` against its own log and `came_from`, and the origin and destination prove their keys over fresh nonces.",
          '**10b.5:** the daemon binds `route-<m>.sock` for each member with no link up here; each accept tries the paths in order (`routes::first_open`). `route::connect` tries `fleet-<m>.sock` first, then the route socket; every link-socket caller uses it.',
          '**10b.6–10b.8:** routed entries in `Machines` (`snapshots::routed`, `via` the first hop); `placed::reachable_workers`; `graph::exposure` for the warning.',
        ]),
      },
      {
        label: 'Modules',
        body: ul([
          '`src/serve/fleet/graph.rs` (`Graph`, `paths`, `MAX_HOPS`, `exposure`); `src/serve/fleet/reports.rs` (`fleet.report`, `fleet.graph`)',
          '`src/serve/fleet/route.rs` (`Header`, `relay_header`, `came_from`, `connect`, `OPENING`); `src/serve/fleet/routes.rs` (`first_open`); `src/serve/endpoint.rs` (`route_socket`)',
          '`src/serve/daemon.rs` (`Shared::serve_route`, `Shared::serve_routed`); `src/serve/snapshots.rs` (`routed`); `src/serve/fleet/placed.rs` (`reachable_workers`); `src/commands/machine.rs` (`exposure_warning`)',
        ]),
      },
      {
        label: 'Code',
        body: md("A routed stream's header, and the one way every client reaches a member: its link if up, else its route."),
        snippets: [
          {
            title: 'src/serve/fleet/route.rs',
            lang: 'rust',
            code: `pub struct Header {
    pub dest: String,
    pub origin: String,
    /// The channel served at the destination.
    pub channel: Channel,
    /// The hops after the origin, ending in \`dest\`.
    pub path: Vec<String>,
    pub ttl: u8,
}`,
          },
          {
            title: 'src/serve/fleet/route.rs',
            lang: 'rust',
            code: `pub fn connect(dir: &Path, name: &str, channel: u8) -> io::Result<UnixStream> {
    let direct = endpoint::fleet_socket(dir, name);
    let first = match super::link::connect(&direct, channel) {
        Ok(stream) => return Ok(stream),
        Err(e) => e,
    };
    let routed = endpoint::route_socket(dir, name);
    super::link::connect(&routed, channel).map_err(|e| {
        let why = format!("no link to \`{name}\` is up ({}: {first}) nor a route to it ({}: {e})", direct.display(), routed.display());
        io::Error::new(first.kind(), why)
    })
}`,
          },
        ],
      },
      {
        label: 'Key commits',
        body: ul([
          '`840787f` docs(fleet): break step 10b into nine sub-steps and record decisions 53-60…',
          "`56bbf09` feat(fleet): a link graph of members' own signed link reports…",
          '`156214d` feat(fleet): every member signs a report of its links on change and sends it to the hub, which pushes the graph…',
          "`d22fa6f` feat(fleet): a routed stream's opening on a ROUTE channel, its header checked at every hop and its origin and destination proving their keys…",
          '`abfe3a2` feat(fleet): both ends of a link relay a routed stream to a next hop that\'s up and, as its destination, serve the inner channel under this host\'s trust…',
          '`5b7ea54` feat(fleet): a `route-<member>.sock` for every member with no link up here…',
          '`de6c450` feat(machine): a member reached only through others is a routed machine…',
          '`08ac478` feat(fleet): proposals, removals, forgets and run --place reach their member over any path…',
          "`b4fdffc` feat(machine): machine trust worker warns, and machine ls and the Machines tab show, when a machine the hub's side controls dials this one…",
          '`f10b5c9` docs(fleet): close step 10b with ADR 0045…, 0043 accepted and 0041 removed…',
        ]),
      },
    ],
    links: [L.plan(), L.fleetDoc(), L.machinesDoc(), L.adr43, L.adr45, L.src('Link graph', 'src/serve/fleet/graph.rs'), L.src('Routed opening', 'src/serve/fleet/route.rs')],
  },
  {
    id: 'phase-step-10c',
    tag: 'Step 10c · a87642d..03e3baf',
    title: 'End-to-end sessions and agent grants',
    summary: md(
      "A routed stream is encrypted between its two daemons, so relays carry ciphertext only, and your agent goes to the members you grant, over any path.",
    ),
    layers: [
      {
        label: 'What',
        body: md(
          "Each end of a routed stream sends a fresh key in the opening, signed with its member key; the records after it are sealed per direction, so a relay can't read, change, inject into, replay or reflect them (ADR 0046). Agent access is a per-member grant in `member.toml` (`[[agent]]`, by key, following rotations), set with `machine set <m> --agent on` on any machine and published in the member's link report; `machines.toml` has no agent flag. A granted member reaches the agent over its link, whichever end dialed, or routed.",
        ),
      },
      {
        label: 'How',
        body: ul([
          '**10c.1:** `fleet::session`: x25519 ephemerals, HKDF-SHA256 per-direction keys, ChaCha20-Poly1305 records numbered by a counter, an authenticated END per direction; `pump` between the plaintext and sealed streams.',
          '**10c.2:** `Shared::serve_routed` (origin) and `Shared::serve_route` (destination) pump through the session; relays only splice.',
          "**10c.3:** `daemon::agent_for` serves `AGENT` only when `fleet::grants::granted` holds for the peer's proven key; the dialed end's `fleet-link --peer` serves `AGENT` too.",
          "**10c.4:** a host probes each member whose report lists it as granted (`fleet::agent_routes`, one `serve-agent-route` thread each) through `route::connect`, binding `agent-route-<m>.sock` while that member serves; an inbound neighbor's `fleet-link --peer` binds `agent-link-<m>.sock` as before. Bridges pick the newest `agent-*.sock`.",
        ]),
      },
      {
        label: 'Modules',
        body: ul([
          '`src/serve/fleet/session.rs` (`Ephemeral`, `Keys::derive`, `Sealer`, `Opener`, `pump`); `src/serve/fleet/route.rs` (the opening\'s keys)',
          '`src/serve/fleet/grants.rs` (`granted`, `granted_names`); `src/serve/fleet/member.rs` (`Grant`, `Member::agent`); `src/serve/fleet/agent_routes.rs`; `src/serve/fleet/agent.rs` (`PROBE`)',
          '`src/serve/daemon.rs` (`agent_for`, `Shared::fleet_peer`); `src/serve/endpoint.rs` (`route_agent_socket`, `link_agent_socket`, `agent_sockets`)',
        ]),
      },
      {
        label: 'Code',
        body: md('Sealed records with an authenticated end, and the agent served only by grant to a proven key.'),
        snippets: [
          {
            title: 'src/serve/fleet/session.rs',
            lang: 'rust',
            code: `    pub fn seal_data(&mut self, data: &[u8]) -> Result<()> {
        for chunk in data.chunks(MAX_RECORD) {
            self.record(DATA, chunk)?;
        }
        self.w.flush()?;
        Ok(())
    }

    /// This direction's END, flushed; nothing is sealed after it.
    pub fn seal_end(&mut self) -> Result<()> {
        self.record(END, &[])?;
        self.ended = true;
        self.w.flush()?;
        Ok(())
    }`,
          },
          {
            title: 'src/serve/daemon.rs',
            lang: 'rust',
            code: `fn agent_for(member: Option<&Path>, peer: &str, key: Option<&fleet::identity::PublicKey>, host_agent: &HostAgent) -> Option<PathBuf> {
    let (member, key) = (member?, key?);
    let grants = fleet::member::load(member).ok()??.agent;
    let stored = fleet::membership_api::load(member).ok()??;
    fleet::grants::granted(&grants, &stored, peer, key).then(|| host_agent()).flatten()
}`,
          },
        ],
      },
      {
        label: 'Key commits',
        body: ul([
          '`a87642d` feat(fleet): session layer for routed streams (ephemeral x25519, per-direction hkdf keys, chacha20poly1305 records with an authenticated end)…',
          '`24f68fb` feat(fleet): routed streams are end-to-end sessions between their two daemons…; adr 0046',
          "`e0b890c` feat(fleet): agent access is a per-member grant in member.toml, by key and following rotations…",
          '`4e08ea4` feat(fleet): an agent reaches a member it granted over any path, routed or a link the member dialed, through an agent-route socket…',
          '`03e3baf` docs(fleet): close step 10c…',
        ]),
      },
    ],
    links: [L.plan(), L.fleetDoc(), L.adr46, L.adr36(), L.src('Sessions', 'src/serve/fleet/session.rs'), L.src('Grants', 'src/serve/fleet/grants.rs'), L.src('Agent routes', 'src/serve/fleet/agent_routes.rs')],
  },
];

// ------------------------------------------------------ Command by command

/** A row's drill-down: headless in the table, so title and summary don't show. */
const cmdCard = (id: string, layers: Layer[], links: SourceLink[]): Card => ({ id, title: id, summary: '', layers, links });

export const commands: CommandRow[] = [
  {
    cmd: 'machine add',
    before: md('Registers an ssh target in `machines.toml`; the link opens only while wanted.'),
    after: md('Joins both ends into one fleet, signed into its membership log; then the daemon holds `ssh <m> devsandbox fleet-link --peer <me>` for good.'),
    change: md("Adding a machine is a standing link and a fleet membership. You confirm the machine's key fingerprint; `--hub` makes the target the hub."),
    card: cmdCard(
      'cmd-machine-add',
      [
        {
          label: 'Today',
          body: md(
            "Install check (`script_argv`), probe = `ssh … api --stdio --sentinel` (`probe_argv`/`probe_dial`), roots recorded via `add_cli_argv` (none is fine, `4af90c0`). The join rule (`machines::join`, step 9) puts both ends in one fleet, refusing two hubs and a taken name. The target's fingerprint is confirmed (`--yes` off a TTY) and passed as `machines.add {key}`; the add signs `Genesis` + `Add`, an `Add`, or an introduction proposed to the authority (step 10a). `--agent` grants this host's agent to it. Then the daemon holds the link always.",
          ),
        },
        {
          label: 'Steps 1–6',
          body: md('Also recorded the picked roots on the machine (`machine roots add` over a one-off ssh, `2281189`).'),
        },
      ],
      [L.src('ssh argvs', 'src/serve/machines/ssh.rs'), L.src('Join rule', 'src/serve/machines/join.rs'), L.src('Install check', 'src/serve/machines/install.rs'), L.src('machine verbs', 'src/commands/machine.rs'), L.machinesDoc()],
    ),
  },
  {
    cmd: 'machine ls',
    before: md('`machines.list`; AGENT column.'),
    after: md('Adds CAPACITY; TARGET `inbound` or `routed via <m>`; AGENT is this host\'s grant; names marked `introduced by …` / `exposed via …`.'),
    change: md('You see every member: those that dialed you, those reached through others, and how much room each has.'),
  },
  {
    cmd: 'machine set --agent on|off',
    before: md('Starts or kills a background agent-relay ssh.'),
    after: md('A grant in this host\'s `member.toml`, by the machine\'s member key; any machine, inbound or routed.'),
    change: md('No relay process. Consent is per member, by key, and no relay can give it.'),
  },
  {
    cmd: 'machine unlink',
    before: md('**Not present** (this was `machine rm`).'),
    after: md('Drops this host\'s link and registry entry; refused for an inbound machine.'),
    change: md('The machine stays a fleet member; nothing on it changes.'),
  },
  {
    cmd: 'machine rm',
    before: md('`machines.rm`: drop the registry entry.'),
    after: md('Revokes the member\'s key (signed on the authority, else proposed), then the hub\'s `fleet.leave` / `fleet.forget`.'),
    change: md('Takes the machine out of the fleet. `machine rm <this host>` leaves.'),
  },
  {
    cmd: 'machine key [rotate]',
    before: md('**Not present.**'),
    after: md('Prints this member\'s key and fingerprint; `rotate` replaces the key through the log.'),
    change: md('New (step 10a). Compare fingerprints when adding a machine.'),
  },
  {
    cmd: 'machine pending|approve|deny',
    before: md('**Not present.**'),
    after: md('On the authority: the membership changes other members proposed.'),
    change: md('New (step 10a). Adds, removals and rotations typed elsewhere wait here.'),
  },
  {
    cmd: 'machine trust worker|full',
    before: md('**Not present.**'),
    after: md('Local `member.set {trust}` only; `--machine` refuses it. `worker` warns when the hub\'s side can reach in anyway.'),
    change: md('New: each machine sets how much others may do on it. `worker` is the default.'),
    card: cmdCard(
      'cmd-machine-trust',
      [
        {
          label: 'What it decides',
          body: md(
            'Every request from another machine is served under this host\'s trust table for that peer, a key it proved on the link or, routed, at the destination (`fleet::lent::admit`/`decide`). `member.*` is never served on a peer connection, so only the machine itself can change its trust. Added in step 6 (`9f29d4d`, `952c42e`). Since `b4fdffc`, `worker` warns when a machine the hub\'s side controls dials this one (`graph::exposure`, `member.get`\'s `exposed_by`).',
          ),
        },
      ],
      [L.src('Trust table', 'src/serve/fleet/lent.rs'), L.src('Member file', 'src/serve/fleet/member.rs'), L.machinesDoc()],
    ),
  },
  {
    cmd: 'machine roots [add|rm]',
    before: md('**Not present.**'),
    after: md('`member.get` / `member.set {roots_add, roots_rm}`.'),
    change: md('New: which of this host\'s config roots others may reach under `worker`.'),
  },
  {
    cmd: 'fleet join|leave, lend',
    before: md('**Not present.**'),
    after: md('**Removed** (`b9cab24`).'),
    change: md('Use `machine add <hub>` instead. There is no `drain` any more.'),
    card: cmdCard(
      'cmd-lend',
      [
        {
          label: 'Steps 1–6',
          body: md(
            '`lend [show|on|off|drain]` and `fleet join|leave` (`e19a8f5`) over `lend.get`/`lend.set`. Join wrote `lend.toml`, and the daemon dialed the hub with `fleet-link --worker`.',
          ),
        },
        {
          label: 'Removal',
          body: md('`b9cab24` deleted lending with offered machines and drain. A `machines.toml` with `offered = true` now fails to parse.'),
        },
      ],
      [L.src('lend and fleet join', 'src/commands/lend.rs', MIDDLE), L.src('Lending worker', 'src/serve/fleet/worker.rs', MIDDLE), L.status],
    ),
  },
  {
    cmd: 'run',
    before: md('`--machine` / `m/name`: a verb over a fresh ssh. No placement.'),
    after: md('Runs here. `--machine`: a verb over the link or route. `run --place`: the hub chooses.'),
    change: md('Placement is asked for, from any member. A sandbox tied to its host, or `placement = "home"`, stays home.'),
    card: cmdCard(
      'cmd-run',
      [
        {
          label: 'Today',
          body: md(
            "A pinned run is a verb over a fresh `API` stream (`forward::verb`, through `route::connect`). A typed `run` stays on the member whose config it names (decision 38). `run --place` on the hub places across every machine it reaches; on a member it asks the hub's `fleet.place` (`PlaceOn::{Requester,Hub,Machine}`) among the machines both reach, then moves the code itself (`code::prepare`, `TRANSFER`) and forwards the verb, printing `placed on <m> (…)`. Unreachable hub: runs here with a note. `--place` refuses `--machine`; `--here` is gone.",
          ),
        },
        {
          label: 'Steps 1–6',
          body: md('A hub with offered workers placed unpinned runs on them only (`placed_run`); ssh machines still took a fresh ssh.'),
        },
      ],
      [L.src('Placement', 'src/serve/fleet/place.rs'), L.src('Candidates', 'src/serve/fleet/placed.rs'), L.src('Forwarded verbs', 'src/serve/machines/forward.rs')],
    ),
  },
  {
    cmd: 'start / stop / rebuild / rm',
    before: md('A forwarded verb over a fresh ssh.'),
    after: md('A verb over a fresh `API` stream on the link, or routed.'),
    change: md('Trust-gated: under `worker`, `--all` only if every instance there is reachable.'),
  },
  {
    cmd: 'exec (tty)',
    before: md('`ssh -t m devsandbox exec …`, re-dialed on exit 255.'),
    after: md('A `PTY` dial through `fleet-<m>.sock` (`route-<m>.sock` for a routed machine); same reconnect loop.'),
    change: md('No ssh per terminal. A lost link exits 255 and reconnects.'),
    card: cmdCard(
      'cmd-exec',
      [
        {
          label: 'Today',
          body: md(
            'The CLI process dials `PTY` through `fleet-<m>.sock` (`fleet::pty_client::dial`); the machine\'s daemon calls the helper\'s `session.attach` over RPC. The reconnect loop is kept (`Reconnect`, `dials`).',
          ),
        },
        {
          label: 'Before',
          body: md('`ssh -t m devsandbox exec --reserve-255 --session=<id> …`, re-dialed with `--attach` on 255 (`reconnect_loop`, `cli_argv`). Steps 1–6 used the PTY dial for offered workers only.'),
        },
      ],
      [L.src('PTY client', 'src/serve/fleet/pty_client.rs'), L.src('CLI routing', 'src/serve/machines/cli.rs'), L.src('CLI routing', 'src/serve/machines/cli.rs', BEFORE)],
    ),
  },
  {
    cmd: 'exec (piped)',
    before: md('`ssh -T m devsandbox exec …`.'),
    after: md('One piped `PTY` dial, never re-dialed.'),
    change: '—',
  },
  {
    cmd: 'exec --service',
    before: md('**Flag not present**; only the dashboard\'s service tab.'),
    after: md('One service `PTY` dial; runtime exec under a host pty on the machine.'),
    change: md('New CLI flag (`8b3e040`). Tty only.'),
  },
  {
    cmd: 'exec --attach / --sessions',
    before: md('An ssh re-run of the same flags.'),
    after: md('`--sessions` is a quick call; `--attach [ID]` one attach-only dial.'),
    change: '—',
  },
  {
    cmd: 'logs',
    before: md('An ssh re-run of `devsandbox logs`.'),
    after: md('`containers.logs` via the daemon, rendered locally.'),
    change: md('Same output, rendered with the same functions.'),
    card: cmdCard(
      'cmd-logs',
      [
        {
          label: 'Routing today',
          body: md(
            '`machine_use` returns `MachineUse::Calls` for `ps|stats|ls|status|service ls` and `Verb` for `gc|service rebuild|--all`. `calls::use_of` maps named commands to `Use::{Logs,BuildLog,Inspect,Done,Rename,…}` or `Use::Link` (exec/port/verbs). A `denied` answer is reworded as coming from the machine (`calls::answer_error`).',
          ),
          snippets: [
            {
              title: 'src/serve/machines/cli/calls.rs',
              lang: 'rust',
              code: `pub(crate) fn use_of(command: &Command) -> Use {
    match command {
        Command::Logs { build: true, follow, .. } => Use::BuildLog { follow: *follow },
        Command::Logs { lines, .. } => Use::Logs { lines: *lines },
        Command::Inspect { json, .. } => Use::Inspect { json: *json },
        Command::Done { .. } => Use::Done(true),
        Command::Undone { .. } => Use::Done(false),
        Command::Rename { new_name, .. } => Use::Rename(new_name.clone()),
        Command::Ls { json } => Use::Ls { json: *json },
        Command::Status { .. } => Use::Status,
        Command::Ps { all, json } => Use::Ps { all: *all, json: *json },
        Command::Stats { json } => Use::Stats { json: *json },
        Command::Service { cmd: ServiceCommand::Ls { json } } => Use::ServiceLs { json: *json },
        Command::Vscode { .. } => Use::Refused(Refused::Vscode),
        _ => Use::Link,
    }`,
            },
          ],
        },
        {
          label: 'Before',
          body: md('`route` → `plan`: `Plan::Machine` → `run_ssh(ssh::cli_argv(...))`, which `exec`s ssh (or spawns it while holding `machines.hold` when `agent` is on).'),
          snippets: [
            {
              title: 'src/serve/machines/cli.rs',
              lang: 'rust',
              rev: BEFORE,
              code: `        Command::Ps { all, json } => MachineUse::Once(args(&["ps"], [flag(*all, "--all"), flag(*json, "--json")])),
        Command::Stats { json } => MachineUse::Once(args(&["stats"], [flag(*json, "--json")])),
        Command::Ls { json } => MachineUse::PerRoot { args: args(&["ls"], [flag(*json, "--json")]), json: *json },
        Command::Status { json: _ } => MachineUse::PerRoot { args: args(&["status", "--json"], []), json: true },
        Command::Gc { force } => MachineUse::PerRoot { args: args(&["gc"], [flag(*force, "--force")]), json: false },`,
            },
          ],
        },
      ],
      [L.src('CLI calls', 'src/serve/machines/cli/calls.rs'), L.src('CLI routing', 'src/serve/machines/cli.rs'), L.src('CLI routing', 'src/serve/machines/cli.rs', BEFORE)],
    ),
  },
  {
    cmd: 'logs --build --follow',
    before: md('An ssh re-run.'),
    after: md('`instances.buildLog` paged by `offset`.'),
    change: md('Doesn\'t follow a rotation into `build.1.log` mid-follow.'),
  },
  {
    cmd: 'inspect',
    before: md('An ssh re-run.'),
    after: md('`containers.inspect`.'),
    change: md('No browser-sandbox line.'),
  },
  {
    cmd: 'done / undone / rename',
    before: md('An ssh re-run.'),
    after: md('`instances.done` / `instances.rename`.'),
    change: '—',
  },
  {
    cmd: 'ls / status --machine',
    before: md('One ssh per registered root, `==> m:<root>` banners.'),
    after: md('`instances.list` per config root, rendered locally.'),
    change: md('Fleet roots list only the roots their instances name.'),
  },
  {
    cmd: 'ps / stats --machine',
    before: md('One ssh `ps` / `stats`.'),
    after: md('`containers.list` / `containers.stats`, cut to what trust reaches.'),
    change: md('A `worker` machine shows only the containers you may reach.'),
  },
  {
    cmd: 'service ls --machine',
    before: md('One ssh per root.'),
    after: md('`instances.list` at `depth: listing` per root, `used_by` cut.'),
    change: '—',
  },
  {
    cmd: 'service rebuild --machine',
    before: md('A verb.'),
    after: md('A verb over the link.'),
    change: '—',
  },
  {
    cmd: 'gc --machine',
    before: md('One ssh `gc [--force]` per root, a full shell with no limits.'),
    after: md('A daemon verb per config root, refused under `worker`.'),
    change: md('**Refused** unless the machine trusts you `full`.'),
    card: cmdCard(
      'cmd-gc',
      [
        {
          label: 'The refusal',
          body: md('`gc` is a daemon verb per config root (`CliVerb::Gc`); `lent::decide` refuses it under `worker`. Steps 1–6 had this for offered workers only (`1d956a0`); ssh machines still ran it over ssh.'),
          snippets: [
            {
              title: 'src/serve/fleet/lent.rs',
              lang: 'rust',
              code: `        "gc" => return Err(ApiError::denied(format!(
            "\`gc\` is host maintenance: not open to remote callers under this member's \`worker\` trust ({FULL_HINT})"
        ))),`,
            },
          ],
        },
      ],
      [L.src('Trust table', 'src/serve/fleet/lent.rs'), L.src('CLI routing', 'src/serve/machines/cli.rs')],
    ),
  },
  {
    cmd: 'port',
    before: md('One `ssh m devsandbox -C <root> tunnel …` per port.'),
    after: md('One `TUNNEL` stream per bridge through the link or route.'),
    change: md('No ssh per port. The hidden `devsandbox tunnel` command is gone.'),
    card: cmdCard(
      'cmd-port',
      [
        {
          label: 'Today',
          body: md('`tunnel::Via::Fleet`, `remote_opener`; a nameless `--service` resolves through `MachineUse::Port`. Steps 1–6 used `TUNNEL` for offered workers only (`1a2aaf4`).'),
        },
        {
          label: 'Before',
          body: md('`port_remote`: one `ssh m devsandbox -C <root> tunnel --instance=… --port N` per port (`tunnel_argv`, no BatchMode), becoming a `devsbd bridge` on the machine.'),
        },
      ],
      [L.src('Tunnels', 'src/commands/tunnel.rs'), L.src('port', 'src/commands/port.rs'), L.src('Tunnels', 'src/commands/tunnel.rs', BEFORE)],
    ),
  },
  {
    cmd: 'dashboard terminal tabs',
    before: md('`ssh -t m devsandbox exec …` per tab.'),
    after: md('Each tab runs local `devsandbox exec … -- <m>/<name>`, a `PTY` dial.'),
    change: md('A global service with no using instance has no terminal.'),
  },
  {
    cmd: 'Ports tab forwards',
    before: md('One `ssh … tunnel` per bridge.'),
    after: md('`TUNNEL` streams on `fleet-<m>.sock`.'),
    change: md('An inbound machine\'s forwards vanish when it disconnects and aren\'t restored.'),
  },
  {
    cmd: 'agent forwarding',
    before: md('`ssh m devsandbox agent-relay` while wanted.'),
    after: md('`AGENT` streams to each granted member: over a link either way, or routed.'),
    change: md('One less ssh. Only the members you grant get your agent, whoever dialed and whatever relays it.'),
    card: cmdCard(
      'cmd-agent',
      [
        {
          label: 'Today',
          body: md(
            "Whether to serve is read per stream from this host's `[[agent]]` grants, against the key the peer proved (`daemon::agent_for`, `fleet::grants::granted`). A dialed end binds `agent-link-<dialer>.sock` while the dialer serves `AGENT` (probed every 2 s, `fleet::agent::PROBE`). Otherwise a host probes each member whose link report lists it (`fleet::agent_routes`) through `route::connect` and binds `agent-route-<m>.sock` while it serves; routed, the stream is end to end (step 10c).",
          ),
          snippets: [
            {
              title: 'src/serve/daemon.rs',
              lang: 'rust',
              code: `fn agent_for(member: Option<&Path>, peer: &str, key: Option<&fleet::identity::PublicKey>, host_agent: &HostAgent) -> Option<PathBuf> {
    let (member, key) = (member?, key?);
    let grants = fleet::member::load(member).ok()??.agent;
    let stored = fleet::membership_api::load(member).ok()??;
    fleet::grants::granted(&grants, &stored, peer, key).then(|| host_agent()).flatten()
}`,
            },
          ],
        },
        {
          label: 'Before, and steps 1–6',
          body: md(
            'Before: `ssh m devsandbox agent-relay --client machine:<host>` bound `agent-machine_<host>.sock` on the machine, and `--session-env` set verbs\' `SSH_AUTH_SOCK`. Steps 1–6 flipped it for offered workers: the **hub\'s** agent reached the worker via `agent-fleet-<hub>.sock`. Today\'s direction is not a continuation of that.',
          ),
        },
      ],
      [L.src('Agent gate', 'src/serve/daemon.rs'), L.src('Grants', 'src/serve/fleet/grants.rs'), L.src('Agent routes', 'src/serve/fleet/agent_routes.rs'), L.src('Agent streams', 'src/serve/fleet/agent.rs'), L.adr36(), L.src('Agent relay', 'src/commands/agent_relay.rs', BEFORE)],
    ),
  },
  {
    cmd: 'self-update',
    before: md('None: the protocol must match; `machine add` copies this binary if versions differ.'),
    after: md('Either end follows its hub, as each hello names it, whichever dialed.'),
    change: md('Automatic in design; today only a dev build copy works.'),
    card: cmdCard(
      'cmd-update',
      [
        {
          label: 'Today',
          body: md(
            '`commands::fleet_link::follows`, `update::decide`: `Download` (published) / `Receive` (dev, same platform, `dev-builds` or `full`) / `Stop(why)`, with a forced handoff (`HelloParams::force`). The hello is `<build> <hub|member> <hub name|->`, and `update::same_fleet` refuses a link between two fleets (step 9). No build is published yet, so `Download` never runs. Steps 1–6 had a worker follow its hub (`2b2bb3c`, `b214318`).',
          ),
        },
      ],
      [L.src('Update decision', 'src/serve/fleet/update.rs'), L.src('fleet-link', 'src/commands/fleet_link.rs'), L.adr20],
    ),
  },
  {
    cmd: 'vscode / inbox on m',
    before: md('Refused.'),
    after: md('Refused (`calls::refusal`).'),
    change: md('Unchanged.'),
  },
  {
    cmd: 'hidden commands',
    before: md('`Tunnel`, `AgentRelay`; `api --stdio --session-env`.'),
    after: md('`FleetLink {peer}`, `FleetHello`; `api --stdio --sentinel` (probe only).'),
    change: md('Nothing to type; fewer things running on the far end.'),
    card: cmdCard(
      'cmd-hidden',
      [
        {
          label: 'By revision',
          body: md(
            '`7716cb8` has `Tunnel` and `AgentRelay`. `3ce588f` adds `FleetLink {worker, root, drain}`, `FleetHello`, `Lend`, `Fleet`. `main` has `FleetLink {peer}` and `FleetHello` only. `api --stdio` lost `--session-env` and gained hidden `--sentinel` (probe only).',
          ),
        },
      ],
      [L.src('CLI', 'src/main.rs', BEFORE), L.src('CLI', 'src/main.rs', MIDDLE), L.src('CLI', 'src/main.rs')],
    ),
  },
];

// ---------------------------------------------------------- Three machines

export const machines: MachinesState[] = [
  {
    id: 'before',
    label: 'Before',
    diagram: {
      hosts: [
        {
          id: 'L',
          role: 'laptop',
          items: [
            {
              name: 'devsandbox serve',
              serve: true,
              body: ['link to K: only while wanted (+60 s), rows in memory'],
            },
            { name: 'CLI · dashboard tabs', client: true },
          ],
        },
        {
          id: 'K',
          role: 'server',
          items: [
            {
              name: 'login shell + devsandbox',
              icon: 'terminal',
              sub: 'one per ssh above',
              body: [{ sockets: [{ name: 'agent-machine_L.sock', note: 'relay' }] }],
            },
            { hop: [{ label: 'api relay' }] },
            { name: 'devsandbox serve', serve: true, sub: 'full' },
          ],
        },
        { id: 'B', role: 'box', items: [] },
      ],
      gaps: [
        {
          dialer: 'L',
          dialed: 'K',
          links: [
            { n: 1, label: 'ssh K devsandbox api --stdio --client machine:L --session-env' },
            { n: 2, label: 'one more such ssh per verb' },
            { n: 3, label: 'ssh -t K devsandbox exec|logs|..', tone: 'cli' },
            { n: 4, label: 'ssh K devsandbox -C <root> tunnel', tone: 'helper' },
            { n: 5, label: 'ssh K devsandbox agent-relay', tone: 'agent' },
          ],
        },
        {
          dialer: 'K',
          dialed: 'B',
          links: [{ n: 6, label: "the same five ssh kinds, K → B, for K's own use", tone: 'runtime' }],
        },
      ],
      caption: "No arrow K → L; L doesn't see B.",
    },
    arrows: [
      { from: 'L', to: 'K', label: '(1) API link', detail: md("L's `serve-machine-K` thread spawns `ssh_dial` = `api_argv`. The remote `api --stdio` relay is a full local client of K's daemon (`relay.rs`; `--session-env` swaps verb env for K's login env). Held while wanted + `Timing::linger` (`service.rs`).") },
      { from: 'L', to: 'K', label: '(2) verb', detail: md('`forward::verb` dials a fresh ssh per `run/stop/…`, so a build never holds up the link.') },
      { from: 'L', to: 'K', label: '(3) CLI re-run / tab', detail: md('`cli::route` `exec`s `ssh::cli_argv` (or `tty_argv` from `tui/remote.rs`). With agent on it holds `machines.hold`. A tty exec loops on 255 (`reconnect_loop`).') },
      { from: 'L', to: 'K', label: '(4) forward', detail: md('`port_remote` / `forwards.rs` `batch_tunnel_argv` → `tunnel_argv`; the far end `devsandbox tunnel` becomes a `devsbd bridge`.') },
      { from: 'L', to: 'K', label: '(5) agent relay', detail: md("`agent_loop` spawns `agent_argv`. On K, `commands::agent_relay` binds `agent-machine_L.sock` and serves `SSH_AGENT` streams from L's agent (`machines::agent::serve`).") },
      { from: 'K', to: 'B', label: '(6) the same five kinds', detail: md('For K\'s own use. Nothing L does reaches B: federation is one hop.') },
    ],
    note: md('Assumes K is reachable by ssh from L, B by ssh from K, and L accepts no ssh. L ran `machine add K`; K ran `machine add B`.'),
  },
  {
    id: 'middle',
    label: 'Steps 1–6 (lending)',
    diagram: {
      hosts: [
        {
          id: 'L',
          role: 'laptop (lending)',
          items: [
            {
              name: 'devsandbox serve',
              serve: true,
              body: [
                'serve-lend: holds ssh K devsandbox fleet-link --worker L --root <froot>',
                "answers K's streams (Peer)",
                { sockets: [{ name: 'agent-fleet-K.sock', note: "K's agent" }] },
              ],
            },
          ],
        },
        {
          id: 'K',
          role: 'hub',
          items: [
            {
              name: 'fleet-link --worker L',
              icon: 'cable',
              body: [
                'binds',
                {
                  sockets: [
                    { name: 'fleet-L.sock' },
                    { name: 'fleet-L.pty.sock' },
                    { name: 'fleet-L.tunnel.sock' },
                    { name: 'fleet-L.transfer.sock' },
                  ],
                },
              ],
            },
            { hop: [{ n: 2, label: 'fleet.offer' }] },
            {
              name: 'devsandbox serve',
              serve: true,
              sub: 'hub',
              body: ['entry L offered=true; places runs on L only'],
            },
          ],
        },
        { id: 'B', role: 'box (ssh machine)', items: [] },
      ],
      gaps: [
        {
          dialer: 'L',
          dialed: 'K',
          links: [
            {
              n: 1,
              label: 'one ssh, frame mux',
              held: true,
              carries: ['K opens API PTY TUNNEL TRANSFER', "L opens AGENT (K's agent)"],
            },
          ],
        },
        {
          dialer: 'K',
          dialed: 'B',
          links: [
            {
              n: 3,
              label: 'B still per-use ssh',
              tone: 'runtime',
              carries: ['api link (+link.attach{peer})', 'verbs, tty/cli, tunnel, agent-relay'],
            },
          ],
        },
      ],
    },
    arrows: [
      { from: 'L', to: 'K', label: '(1) fleet link', detail: md("`fleet/worker.rs` `Lending` holds `worker::link_argv` and is the answerer of `fleet::LINK`. K's streams are served on L by the `lent` table: the fleet root, plus L's own roots from step 6 (`952c42e`).") },
      { from: 'fleet-link', to: 'K serve', label: '(2) offer', detail: md('`fleet.offer {worker, version, root, draining}` (`fleet/offer.rs`) writes the offered entry. K dials L\'s API via `fleet-L.sock`.') },
      { from: 'K', to: 'B', label: '(3) ssh machine', detail: md("Unchanged per-use argvs. From step 6 the API relay attaches as K (`aa25738`), so B's trust table governs that link but not the other ssh uses. Placement never picks B (`placed::workers` = offered only); `--machine B` or `B/x` reaches it.") },
    ],
    note: md("Superseded. L ran `fleet join K` (`lend.toml` `on`); K ran `machine add B` and runs `[serve] hub = true`. Agent direction: K's agent reaches L (`f0320ad`); L's own agent never reaches K."),
  },
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
              name: 'devsandbox serve',
              serve: true,
              body: [
                'serve-machine-K-link holds ssh K devsandbox fleet-link --peer L',
                'binds',
                {
                  sockets: [
                    { name: 'fleet-K.sock' },
                    { name: 'route-B.sock', note: 'B via K' },
                  ],
                },
                "IN: K's streams in-process, L's trust, root fleet/K/",
                'member of K\'s fleet: hub = K, key proven on the link',
              ],
            },
          ],
        },
        {
          id: 'K',
          role: 'hub',
          items: [
            {
              name: 'fleet-link --peer L',
              icon: 'cable',
              body: [
                'binds',
                {
                  sockets: [
                    { name: 'fleet-L.sock' },
                    { name: 'agent-link-L.sock', note: 'if L grants K' },
                  ],
                },
              ],
            },
            {
              hop: [
                { n: 2, label: 'each stream: link.attach{peer:L,chan}' },
                { n: 3, label: 'links.inbound: L' },
              ],
            },
            {
              name: 'devsandbox serve',
              serve: true,
              sub: 'hub, authority',
              body: [
                'hub = "self"; signs membership.json',
                'machine L (inbound) via fleet-L.sock',
                'serve-machine-B-link holds ssh B devsandbox fleet-link --peer K; binds fleet-B.sock',
                'relays L-B routed streams: ciphertext only',
              ],
            },
          ],
        },
        {
          id: 'B',
          role: 'box',
          items: [
            {
              name: 'fleet-link --peer K',
              icon: 'cable',
              body: ['binds', { sockets: [{ name: 'fleet-K.sock' }, { name: 'agent-link-K.sock', note: "K's grant" }] }],
            },
            { hop: [{ label: 'each stream: link.attach' }] },
            {
              name: 'devsandbox serve',
              serve: true,
              sub: 'trust per proven member',
              body: [
                'binds',
                { sockets: [{ name: 'route-L.sock', note: 'L via K' }, { name: 'agent-route-L.sock', note: 'if L grants B' }] },
              ],
            },
          ],
        },
      ],
      gaps: [
        {
          dialer: 'L',
          dialed: 'K',
          links: [
            {
              n: 1,
              label: 'the L-K link (one ssh)',
              held: true,
              carries: [
                'either end: API PTY TUNNEL TRANSFER, AGENT by grant',
                'ROUTE: L-B streams, sealed end to end',
              ],
            },
          ],
        },
        {
          dialer: 'K',
          dialed: 'B',
          links: [{ n: 4, label: 'the K-B link, same shape', held: true, carries: ['ROUTE: K relays L-B'] }],
        },
      ],
      caption: 'L sees B as a routed machine, via K; K relays what it cannot read.',
    },
    arrows: [
      { from: 'L', to: 'K', label: '(1) peer link', detail: md("`fleet::dialer::Dialer::hold` dials `Transport::ssh` with `link_args(\"L\")`. L is the answerer (reads sentinel `devsandbox-fleet 2`). After the hellos (each naming its hub, `update::same_fleet`) both ends prove their member keys against `membership.json` (`fleet::proof`), the end behind taking the entries it lacks. L-side clients (CLI, dashboard, L's daemon) reach K through `fleet-K.sock` (the channel in each connection's first byte). K's streams to L go in-process to `Shared::open_lent` → `serve_channel` with `Peer {name: K, fleet_root: fleet/K/}`.") },
      { from: 'fleet-link', to: 'K serve', label: '(2) attach', detail: md("`commands::fleet_link::attach_stream` connects K's daemon socket, sends `link.attach` with the peer, its proven key and the channel, then splices. K's `daemon::link_attach` (own uid only) → `Shared::serve_channel` under K's trust for L, fleet root `fleet/L/`.") },
      { from: 'fleet-link', to: 'K serve', label: '(3) announce', detail: md("`links.inbound {peer: L, version, hub}`, held for the link's life (`Announce`, `inbound::Guard`). K's `Machines` then holds a link to L over `fleet-L.sock` (`service::carried`) and asks L's roots once (`daemon::inbound_roots` → `roots.list`, the fleet root flagged `fleet: true`). L is now a placement candidate.") },
      { from: 'K', to: 'B', label: '(4) peer link', detail: md("The same as (1) with K the dialer. On B, `fleet-link --peer K` binds `fleet-K.sock`, and `agent-link-K.sock` while K serves AGENT (K's `member.toml` grants B, `daemon::agent_for` by B's proven key). B's bridges pick that socket via `endpoint::agent_sockets`.") },
      { from: 'L', to: 'B', label: 'routed stream', detail: md("Each member signs a report of its links and sends it to K (`fleet.report`), which pushes the graph (`fleet.graph`). With no link to B, L's daemon binds `route-B.sock` (`fleet::routes`), and `route::connect` falls back to it, so CLI calls, verbs, terminals and forwards to B work as for K. Each stream opens on `ROUTE` with a header naming L, B and the path; K checks its next hop and splices; B proves L's key over a fresh nonce (ADR 0045) and serves it under B's trust for L. Both ends agree fresh keys in the opening and seal every record after it (`fleet::session`, ADR 0046).") },
      { from: 'B', to: 'L', label: 'agent grant', detail: md("If L ran `machine set B --agent on`, L's link report lists B; B probes L through `route::connect` (`fleet::agent_routes`) and binds `agent-route-L.sock` while L serves, its streams sealed like any routed one. K never sees the agent's bytes.") },
      { from: 'L', to: 'K', label: 'run --place', detail: md("On L, `run --place` asks K's `fleet.place`. K asks each candidate both reach for `capacity.get` (2 s, `CAPACITY_WAIT`) and answers `{on: requester|hub|machine}`. If B wins, L sends the root tar + bundles as `TRANSFER` streams over `route-B.sock` into B's `fleet/L/`, then forwards the verb. K unreachable: it runs on L with a note.") },
    ],
    note: md(
      "K ran `machine add B --agent` (a fleet of two: K its hub and authority, B granted K's agent), then L ran `machine add K` (L joins K's fleet; K signs L's `Add`). L sees K's rows and, through K's federated view, B's, as `routed via K`. L's daemon stays up while it dials K (`idle::Holders::links`); K never idles.",
    ),
  },
];

// ----------------------------------------------------------- What's left

export const steps: Card = {
  id: 'left-steps',
  tag: 'Planned · not built',
  title: 'Rollout steps 11–18',
  summary: md("From the plan's rollout; letters are its sizes. Steps 1–10 are on `main`; none of these is."),
  layers: [
    {
      label: 'The steps',
      body: ul([
        '**11 Hub swap, pointer (S–M).** `machine hub <m>` moves the hub role to another machine without changing any links; every reachable member rewrites its `hub`, a late one learns it on reconnect.',
        '**12 Transfer v1 (L).** devsandbox moves code itself, including repos with no remote and unpushed work (WIP snapshots, bundles routed over the graph, repo caches, the hub\'s mirror, fetch-back).',
        '**13 Dispatcher children on any machine (M).** Children a dispatcher creates can run on any machine, with the hub recording where each one lives.',
        '**14 Checkpoints and re-placement (M–L).** Placed work is checkpointed to the hub, and a child whose machine disappears is restarted elsewhere from its last checkpoint (`devsandbox move` by hand).',
        '**15 The rest of the fleet view (M).** The Inbox across machines, and commands that take no name working across the fleet.',
        "**16 Sync layer (L, later).** Moves what git can't (build outputs, volumes, caches, live editing), and then shared build caches and services that span machines.",
        '**17 Hub handoff (M).** Swapping the hub while the old one is alive ships its state (repo cache, rows, records, checkpoints) to the new one.',
        '**18 Fallback hub (M, later).** Another machine takes over the hub role when the hub has been gone a while. Who decides that, and what happens when the old hub returns, are still open.',
      ]),
    },
  ],
  links: [L.plan(), L.status],
};

export const limits: Card = {
  id: 'left-limits',
  tag: 'docs/status.md',
  title: 'Known limitations on main',
  summary: md("What doesn't work yet, from the status page's machines section."),
  layers: [
    {
      label: 'Limitations',
      body:
        '<p><strong>Placement and moving code</strong></p>' +
        ul([
          "Only `run --place` is placed, and only by the daemon: with no daemon, or `--in-process`, it runs here with a note. A dispatcher's children aren't placed yet (step 13).",
          'A member waits up to 2 s to reach the hub and 6 s for its answer; the hub asks each candidate for capacity (up to 2 s) first.',
          "The placing host doesn't record placed instances. A hang-up while code moves doesn't cancel it, a new `--branch` starts at the hub checkout's tip, and a `--base` naming `origin/…` fails on the worker.",
          'Only committed, pushed work moves (one branch per folder\'s `origin`). Everything is resent every time, and caches and moved trees are never pruned.',
          "Anything tying a sandbox to its host keeps it home, whatever the worker allows; a pin can only say home (`placement = \"home\"`).",
          "Placement's `has_image` reads only watched snapshots (a routed machine never has the image), and `has_repo` is never filled.",
          'Soft caps are hand-edited and measured against the whole host, and capacity rides snapshots only while a root is watched.',
          "A machine can't stop taking placed work while staying reachable: `drain` went with lending.",
        ]) +
        '<p><strong>Membership and keys</strong></p>' +
        ul([
          'Fleets made before member identity have no membership log: their links are refused until the fleet is made again with `machine add`.',
          "A member that's only introduced (capped) can't be removed with `machine rm` or rotate its key until the authority adds it. An introduction the target signs for this host when it joins isn't proposed to the authority.",
          "A member removed while unreachable keeps its hub and entries until it's reachable again (run `machine rm` again, or on it). Members the hub reaches by no path keep its entry, though its links are refused once the revoke reaches them.",
          'A member that leaves is forgotten by the others only once the authority approves its revoke.',
          'No command sets the member name (`member.set {name}` or a hand edit).',
        ]) +
        '<p><strong>Routing</strong></p>' +
        ul([
          "Routed machines' rows come only from the hub's view, one hop; a member it doesn't list has no roots in `machine ls` and isn't on the Machines tab.",
          "A routed stream doesn't fail over once open (its client opens it again), and a member more than four hops away is unreachable.",
          "Link reports are versioned by their signer's clock: a member whose clock went back has its reports refused until it passes its last one.",
          'A link gone stays counted as exposure until both ends have reported since.',
        ]) +
        '<p><strong>Agent</strong></p>' +
        ul([
          "A grant to an inbound or routed machine takes effect once this host's link report reaches it; while it's listed, that machine probes this host every 2 s.",
          "With several hosts granting one machine, processes there use the newest agent socket, not their caller's.",
          "A verb sent to a machine that reaches your agent through `agent-route-…` doesn't get it as `SSH_AUTH_SOCK`, so host git there doesn't use it (its instances do).",
        ]) +
        '<p><strong>Versions</strong></p>' +
        ul([
          'No build is published yet, so a member never downloads. It takes the hub\'s binary only with `dev-builds = true` or `full` trust and the same OS/arch; otherwise it drops the link and is refused again on every redial. Dev builds from one commit with different local changes count as the same build.',
          'A member that won\'t follow its hub drops the link at every dial. If the hub was the one dialed, it only sees the inbound machine come and go.',
          "A member waiting for the hub's binary gives up only if the transfer doesn't start within 60 s.",
        ]) +
        '<p><strong>Links and machines</strong></p>' +
        ul([
          'An inbound machine exists only while its link is up: when the laptop sleeps, its rows and forwards vanish, and the forwards aren\'t restored.',
          "Remote rows live in memory except on the hub, so a fresh daemon shows machines without rows until they connect.",
          "Verbs from a linked host run with the daemon's own process env (systemd/LaunchAgent), not a login shell's, so `PATH` additions are missing.",
          'Machines added before roots were recorded have none in `member.toml`, so `worker` trust refuses those roots.',
          'Terminals on helper-less or non-root-helper instances, and service terminals, are not resumable over the link.',
          'Daemon-side ssh is `BatchMode=yes`, so passphrase-prompting keys fail.',
          '`vscode` and `inbox` are refused on machines, and nameless commands are local only.',
        ]),
    },
  ],
  links: [L.status, L.machinesDoc(), L.plan()],
};
