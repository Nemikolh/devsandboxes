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
  adr49: { label: 'ADR 0049', path: 'docs/decisions/0049-the-authority-signs-a-config-every-member-runs.md' } as SourceLink,
  adr53: { label: 'ADR 0053', path: 'docs/decisions/0053-the-authority-pins-the-fleets-build.md' } as SourceLink,
  adr56: { label: 'ADR 0056', path: 'docs/decisions/0056-a-gpu-is-placed-where-a-worker-lends-one.md' } as SourceLink,
  src: (label: string, path: string, rev?: string): SourceLink => ({ label, path, rev }),
};

const howLayer = (body: string) => [{ label: 'How', body: md(body) }];

export const why: Card[] = [
  {
    id: 'why-overview',
    tag: '7716cb8 → main',
    title: 'Why change anything?',
    summary: md(
      'At `7716cb8` a machine was something you could drive from here, and little more. Seven problems, and what each one is now:',
      "<br><a href='#why-capacity'>**Machines were remote controls, not capacity.**</a> Ask, and the control plane picks the machine.",
      "<br><a href='#why-laptop'>**A laptop could use other machines but not be used by them.**</a> Whichever side can reach the other connects, and others are reached through it.",
      "<br><a href='#why-one-connection'>**Every use opened a new ssh, with nothing limiting it.**</a> One proven connection per pair; each machine sets its own limits.",
      "<br><a href='#why-always-on'>**Connections only lasted while someone was looking.**</a> They stay up, and the control plane remembers the fleet.",
      "<br><a href='#why-code'>**A sandbox only ran where its config and code were.**</a> devsandbox moves both, work in progress included.",
      "<br><a href='#why-versions'>**Versions were upgraded by hand on every machine.**</a> Members follow the build the authority pinned.",
      "<br><a href='#why-keys'>**SSH keys needed a connection of their own.**</a> Your agent is a grant to a member (optional).",
      "<br><a href='#why-lending'>**Why the lending detour was dropped.**</a> Two kinds of machine became one.",
    ),
    links: [L.plan(BEFORE), L.plan(), L.adr43, L.adr44, L.adr46],
  },
  {
    id: 'why-capacity',
    tag: 'Problem 1',
    title: 'Machines were remote controls, not capacity',
    summary: md(
      'You always picked the box yourself, and nothing knew which box had room.',
      '**Now `run --place` asks the control plane to pick:** every machine reports its CPUs, memory, disk, GPU and running count.',
    ),
    layers: howLayer(
      "A plain `run` stays on the member whose config it names. `run --place` asks the control plane (`fleet.place`), which scores every machine both reach (soft caps, image already built, a small preference for staying put); the member then moves the code there itself. A sandbox requiring a GPU goes only to a worker lending one (or stays home); an optional GPU is a score bonus. A sandbox tied to its host (a bind outside what moves, `${localEnv:…}`, the runtime socket…) or pinned with `placement = \"home\"` stays home. See <a href='#phase-step-9'>Step 9</a>.",
    ),
    links: [L.adr56, L.src('Placement', 'src/serve/fleet/place.rs'), L.src('Candidates', 'src/serve/fleet/placed.rs'), L.src('Ties', 'src/serve/fleet/code/mod.rs'), L.src('Capacity report', 'src/serve/capacity.rs')],
  },
  {
    id: 'why-laptop',
    tag: 'Problem 2',
    title: 'A laptop could use other machines but not be used by them',
    summary: md(
      'Every connection was an ssh login into the other machine, so a server could only use a laptop that accepted ssh logins.',
      '**Now whichever side can reach the other makes the connection, and both sides use it.**',
    ),
    layers: howLayer(
      "The laptop runs `devsandbox machine add <control-plane>`; the control plane then sees it as one of its machines without ever logging into it. The control plane's other machines show on the laptop as `routed via <control-plane>`. See <a href='#phase-step-8'>Step 8</a>, <a href='#phase-step-10b'>Step 10b</a> and <a href='#three-machines'>Three machines</a>.",
    ),
    links: [L.adr43, L.machinesDoc(), L.src('Inbound machines', 'src/serve/machines/inbound.rs'), L.src('Route sockets', 'src/serve/fleet/routes.rs')],
  },
  {
    id: 'why-one-connection',
    tag: 'Problem 3',
    title: 'Every use opened a new ssh connection, with nothing limiting it',
    summary: md(
      'The background link, every verb, terminal, port forward and the key forwarding each started their own ssh, each able to reach all of devsandbox there.',
      '**Now one connection per pair of machines carries everything, and each machine decides how much the others may do.**',
    ),
    layers: howLayer(
      "The default trust, `worker`, lets others manage the sandboxes this machine already has, plus work placed on it under a strict policy (loosened per host with `machine allow`); never its settings, machine list or host maintenance such as `gc`. `full` opens everything, set only on that machine (`machine trust worker|full`). Each end proves its member key on the link, so trust applies to a proven member. See <a href='#phase-step-7'>Step 7</a> and <a href='#phase-step-10a'>Step 10a</a>.",
    ),
    links: [L.machinesDoc(), L.src('Trust table', 'src/serve/fleet/lent.rs'), L.src('Transport', 'src/serve/fleet/transport.rs'), L.src('Key proof', 'src/serve/fleet/proof.rs')],
  },
  {
    id: 'why-always-on',
    tag: 'Problem 4',
    title: 'Connections only lasted while someone was looking',
    summary: md(
      "A link opened when something wanted it and dropped 60 s later; its rows were forgotten on restart.",
      '**Now links stay up while both machines are, and the control plane remembers what it last saw.**',
    ),
    layers: howLayer(
      "Each link keeps both daemons running. A member watches the control plane's federated view, so it sees the whole fleet. The control plane saves the rows (`remotes.json`), so after a restart it shows the fleet before the others reconnect. See <a href='#phase-step-9'>Step 9</a>.",
    ),
    links: [L.src('Idle countdown', 'src/serve/idle.rs'), L.src('Saved rows', 'src/serve/machines/saved.rs'), L.src('Held machines', 'src/serve/machines/service.rs')],
  },
  {
    id: 'why-code',
    tag: 'Problem 5',
    title: 'A sandbox only ran where its config and code were',
    summary: md('Running `web` on another box meant copying its config and checkout there by hand.', '**Now devsandbox moves both.**'),
    layers: howLayer(
      "A placed run sends the config folder as an archive and each folder's branch plus a snapshot of its uncommitted work, straight from its checkout: no remote, no credentials on the worker. Repeat sends carry only the delta, and `devsandbox fetch <machine>/<instance>` brings the work back as hidden refs. Separately, a config the authority runs is signed, and every member installs that version (ADR 0049). See <a href='#phase-step-12'>Step 12</a>.",
    ),
    links: [L.src('Code transfer', 'src/serve/fleet/code/mod.rs'), L.src('Fetch-back', 'src/serve/fleet/code/fetch.rs'), L.adr49, L.status],
  },
  {
    id: 'why-versions',
    tag: 'Problem 6',
    title: 'Versions had to be upgraded by hand on every machine',
    summary: md(
      "If two machines' protocols didn't match, the machine just showed as unreachable.",
      "**Now members follow the build the fleet's authority pinned, downgrades included.**",
    ),
    layers: howLayer(
      "The authority pins its own build in the membership log whenever its daemon starts on a new one. Every other member, the control plane included, compares itself with the pin on each link: a published release is downloaded and checked against its signed checksums; a dev build is copied from any peer that has it, and taken only with `dev-builds = true` or `full` trust on the same OS and CPU. A member that can't follow stays linked, shown `behind: <why>`. ADR 0053.",
    ),
    links: [L.adr53, L.src('Update decision', 'src/serve/fleet/update.rs'), L.status],
  },
  {
    id: 'why-keys',
    tag: 'Problem 7 · optional',
    title: 'SSH keys needed a connection of their own',
    summary: md('Lending your ssh keys to a machine took one more ssh.', "**Now your agent is a grant to a member, over the fleet's own streams.**"),
    layers: howLayer(
      "Off by default. `machine set <m> --agent on` writes a grant into this host's `member.toml`, by `m`'s member key. Any machine can be granted; it reaches your agent over its link or routed, end to end, so no relay can grant or read it. See <a href='#phase-step-10c'>Step 10c</a>.",
    ),
    links: [L.adr36(), L.adr46, L.src('Grants', 'src/serve/fleet/grants.rs'), L.src('Agent routes', 'src/serve/fleet/agent_routes.rs')],
  },
  {
    id: 'why-lending',
    tag: 'Steps 1–6 · superseded',
    title: 'Why the lending detour was dropped',
    summary: md(
      'Steps 1–6 added a second kind of machine: a laptop “lent” itself to a control plane with `fleet join`, which saw only a separate lent folder, while ssh machines kept working the old way.',
    ),
    layers: howLayer(
      "Two ways to add a machine, and a laptop's own sandboxes invisible to the fleet. Steps 7–8 merged them: every machine is added with `machine add`, reached over one connection, and limited by its own trust setting. See <a href='#phase-steps-1-6'>Steps 1–6</a>.",
    ),
    links: [L.plan(), L.adr43, L.src('Lending (removed)', 'src/commands/lend.rs', MIDDLE)],
  },
];

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
          "`machine add` registers an ssh target in `machines.toml`. The daemon links to the machine's daemon over `ssh <m> devsandbox api --stdio`, merges its rows and forwards requests. Verbs, terminals, CLI re-runs, forwards and the agent relay are each a separate ssh. The protocol must match (ADR 0020); remote rows are memory-only; federation is one hop.",
        ),
      },
      {
        label: 'Code',
        body: md('The link and the agent relay are two ssh argvs side by side.'),
        snippets: [
          {
            title: 'src/serve/machines/ssh.rs',
            lang: 'rust',
            rev: BEFORE,
            code: `pub fn api_argv(entry: &Entry, client: &str) -> Result<Vec<OsString>> {
    let args = ["api", "--stdio", "--client", client, "--session-env"];
    argv(entry, &["-T", "-o", "BatchMode=yes"], &args)
}

pub fn agent_argv(entry: &Entry, client: &str) -> Result<Vec<OsString>> {
    argv(entry, &["-T", "-o", "BatchMode=yes"], &["agent-relay", "--client", client])
}`,
          },
        ],
      },
    ],
    links: [L.machinesDoc(BEFORE), L.adr20, L.adr36(BEFORE), L.plan(BEFORE), L.src('ssh argvs', 'src/serve/machines/ssh.rs', BEFORE)],
  },
  {
    id: 'phase-steps-1-6',
    tag: 'Steps 1–6 · e8f1123..3ce588f · superseded',
    title: 'The fleet link, and lending (superseded)',
    summary: md(
      'A frame-mux link carries the API, then PTY, TUNNEL, TRANSFER and AGENT. Laptops **lend** themselves by dialing the control plane; ssh machines stay on per-use ssh. Steps 7–8 replaced this.',
    ),
    layers: [
      {
        label: 'What',
        body: md(
          "A laptop runs `devsandbox fleet join <control-plane>`; its daemon dials the control plane, which lists it as an `offered` machine. Over steps 2–6 the control plane also gets self-update, placement on offered workers with code transfer, terminals, forwards and agent over the link, the remaining commands as API calls, and `member.toml` with `worker`/`full` trust.",
        ),
      },
      {
        label: 'Code',
        body: md('Placement only looked at machines that dialed in.'),
        snippets: [
          {
            title: 'src/serve/fleet/placed.rs',
            lang: 'rust',
            rev: MIDDLE,
            code: `pub fn workers(machines: &[Machine]) -> impl Iterator<Item = &Machine> {
    machines.iter().filter(|m| m.kind == MachineKind::Offered)
}`,
          },
        ],
      },
      {
        label: 'Key commits',
        body: ul([
          '**1 Link + control-plane mode:** `e8f1123`, `ca191f5`, `782b07d`, `e19a8f5`, `c5189f1`, `f59a05d`',
          '**2 Self-update:** `b7fd80b`, `2b2bb3c`, `b214318`',
          '**3 Placement v0 + transfer:** `8d54318`, `3c8b19e`, `230d22a`',
          '**4–5 Everything over the link:** `f87d0f2`, `5435dca`, `1a2aaf4`, `f0320ad`, `67a9dd5`, `8e5707c`, `8b3e040`',
          '**6 Member file + trust:** `9f29d4d`, `952c42e`, `aa25738`, `1d956a0`',
          '**Design pivot:** `8f0ee37` one kind of machine; `5d9cecb` links routed over a graph',
        ]),
      },
    ],
    links: [
      L.fleetDoc(MIDDLE),
      { label: 'ADR 0042', path: 'docs/decisions/0042-fleet-workers-follow-their-hubs-version.md', rev: MIDDLE },
      L.src('Lending worker', 'src/serve/fleet/worker.rs', MIDDLE),
      L.src('lend and fleet join', 'src/commands/lend.rs', MIDDLE),
    ],
  },
  {
    id: 'phase-step-7',
    tag: 'Step 7 · b1cd7d3..23ecf62',
    title: 'One link per machine pair',
    summary: md('Every ssh machine is held as a symmetric `fleet-link --peer` link, always, and every client reaches it through `fleet-<m>.*.sock`.'),
    layers: [
      {
        label: 'What',
        body: md(
          'Both ends bind local sockets and both answer streams, so who dialed no longer decides who can use the link. `Want`, linger, `machines.hold` and the agent-relay thread go away.',
        ),
      },
      {
        label: 'How',
        body: ul([
          '`Transport::dial` is the only way bytes cross (ssh its one impl); opener and answerer share one body.',
          "The dialer (`Dialer::hold`) runs `fleet-link --peer <own name>`, binds `fleet-<m>.sock` and serves the peer's streams in-process.",
          'The dialed end binds `fleet-<dialer>.sock`, hands each stream to its daemon with `link.attach {peer, channel}` and announces the dialer as an inbound neighbor.',
          "Both ends serve under the receiving host's trust table, with fleet root `fleet/<peer>/`.",
        ]),
      },
      {
        label: 'Code',
        body: md('Each connection names its channel in its first byte.'),
        snippets: [
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
        body: ul(['`b1cd7d3` one transport seam', '`4286ca8` link.attach takes a channel', '`6af5483` fleet-link --peer', '`e5b8f78` every ssh machine held as a peer link']),
      },
    ],
    links: [L.plan(), L.fleetDoc(), L.src('Transport', 'src/serve/fleet/transport.rs'), L.src('fleet-link --peer', 'src/commands/fleet_link.rs'), L.src('Dialer', 'src/serve/fleet/dialer.rs')],
  },
  {
    id: 'phase-step-8',
    tag: 'Step 8 · 2e1bd6d..08e8279',
    title: 'One kind of machine',
    summary: md("Every use of every machine goes over its link's channels, inbound neighbors become machines, and lending is deleted."),
    layers: [
      {
        label: 'What',
        body: md(
          'The CLI, dashboard terminals, forwards and `port` reach every machine via `fleet-<m>.*.sock`. The per-use ssh argvs, `agent-relay` and `devsandbox tunnel` are deleted: ssh is left only for the link and `machine add`. A dialed host treats the dialer as a machine (kind `inbound`), so a laptop joins with `machine add <control-plane>`. `fleet join|leave`, `lend`, `offered` entries and `drain` are removed.',
        ),
      },
      {
        label: 'Key commits',
        body: ul([
          '`90c7ee3` CLI re-runs become API calls and PTY dials',
          '`bfc7846` terminals, forwards and port over the link',
          '`a83207c` delete the per-use ssh argvs and the agent relay',
          '`dd4d2ed` a dialed host holds a link to each inbound neighbor',
          '`b9cab24` drop lending, offered machines and drain (−4949 lines)',
        ]),
      },
    ],
    links: [L.plan(), L.machinesDoc(), L.adr43, L.src('CLI calls', 'src/serve/machines/cli/calls.rs'), L.src('Inbound machines', 'src/serve/machines/inbound.rs')],
  },
  {
    id: 'phase-step-9',
    tag: 'Step 9 · a1bb25d..d486014',
    title: 'Membership and the control plane',
    summary: md(
      "The control plane is a member setting, `machine add` makes both ends one fleet, members see the fleet through the control plane, and `run --place` asks it where to go.",
    ),
    layers: [
      {
        label: 'What',
        body: md(
          "`control_plane` in `member.toml` (`self`, or the control plane's name) is named in every link's hello, so two fleets never link. `machine add` joins the target to this fleet, this host to the target's, or starts a fleet of two (`--control-plane` picks the target); two control planes are refused. `machine rm` takes a member out of the fleet; `machine unlink` only drops the link. A typed `run` stays on its member; `run --place` asks the control plane's `fleet.place` and moves the code itself.",
        ),
      },
      {
        label: 'Code',
        body: md('A link between two fleets is refused at the hello.'),
        snippets: [
          {
            title: 'src/serve/fleet/update.rs',
            lang: 'rust',
            code: `pub fn same_fleet(peer: &str, ours: &str, theirs: &str) -> Result<(), String> {
    match (control_plane_of(ours), control_plane_of(theirs)) {
        (Some(ours), Some(theirs)) if ours != theirs => {
            Err(format!("\`{peer}\` is in \`{theirs}\`'s fleet, this host in \`{ours}\`'s: a link between two fleets is refused"))
        }
        _ => Ok(()),
    }
}`,
          },
        ],
      },
      {
        label: 'Key commits',
        body: ul([
          "`98d5c06` the control plane is member.toml's, named in every hello",
          '`581389b` machine add joins fleets',
          "`582b629` a member watches the control plane's federated view",
          '`001ccfa` machine rm through the control plane; machine unlink',
          "`e832892` a member's run asks fleet.place",
          'After: `2c28f0f` placement only with `run --place`; `17e88ea` tied sandboxes stay home; `d9e7ead` `placement = "home"`',
        ]),
      },
    ],
    links: [L.plan(), L.fleetDoc(), L.machinesDoc(), L.src('Join rule', 'src/serve/machines/join.rs'), L.src('Candidates', 'src/serve/fleet/placed.rs')],
  },
  {
    id: 'phase-step-10a',
    tag: 'Step 10a · 2486e9b..bf5cd91',
    title: 'Member identity',
    summary: md("Every member has a key, the fleet's authority signs who belongs in a hash-chained log, and both ends of every link prove their keys against it."),
    layers: [
      {
        label: 'What',
        body: md(
          "Each member holds an ed25519 key in `member.key` (`machine key` prints its fingerprint). The member that made the fleet is its authority and signs every membership change into `membership.json`. `machine add` confirms the joining key's fingerprint (`--yes` off a TTY). An add on another member is an introduction, proposed to the authority. `machine rm` revokes a key, `machine key rotate` replaces one; typed elsewhere, both wait for `machine approve` on the authority. A link that doesn't prove its keys is refused, and no container may bind or build from a path covering the key. ADR 0044.",
        ),
      },
      {
        label: 'Code',
        body: md('What two ends learn when they compare log heads.'),
        snippets: [
          {
            title: 'src/serve/fleet/membership.rs',
            lang: 'rust',
            code: `pub enum Compare {
    Same,
    Behind,
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
          '`fbce32b` a member key',
          "`7015c9a` the authority's hash-chained membership log",
          '`876a3cc` both ends prove their keys after the hellos',
          '`3c45e63` machine rm revokes; `cc080f1` machine key rotate',
          '`6d3421f` refuse binds reaching the member key',
        ]),
      },
    ],
    links: [L.plan(), L.fleetDoc(), L.adr44, L.src('Membership log', 'src/serve/fleet/membership.rs'), L.src('Key proof', 'src/serve/fleet/proof.rs'), L.src('Key guard', 'src/key_guard.rs')],
  },
  {
    id: 'phase-step-10b',
    tag: 'Step 10b · 840787f..f10b5c9',
    title: 'Routing over the graph',
    summary: md('Members report their links, every member keeps the graph, and a member with no link of its own is reached hop by hop, its origin proven per stream.'),
    layers: [
      {
        label: 'What',
        body: md(
          "A member reached only through others is a **routed machine** (`routed via <m>`), with calls, verbs, terminals and forwards over its route. Each member signs a report of its links; `Graph::paths` orders them (at most 4 hops). Each relay checks its next hop and splices; the destination proves the stream's origin by its key and serves it under its trust for that origin, never the relay's. `machine trust worker` warns when a machine the control plane's side controls dials this one. ADR 0045.",
        ),
      },
      {
        label: 'Code',
        body: md('The one way every client reaches a member: its link if up, else its route.'),
        snippets: [
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
    // …`,
          },
        ],
      },
      {
        label: 'Key commits',
        body: ul([
          '`56bbf09` a graph of signed link reports',
          '`d22fa6f` a routed opening, checked at every hop',
          '`5b7ea54` a `route-<member>.sock` per member with no link',
          '`de6c450` routed machines',
          '`b4fdffc` the exposure warning',
        ]),
      },
    ],
    links: [L.plan(), L.fleetDoc(), L.adr43, L.adr45, L.src('Link graph', 'src/serve/fleet/graph.rs'), L.src('Routed opening', 'src/serve/fleet/route.rs')],
  },
  {
    id: 'phase-step-10c',
    tag: 'Step 10c · a87642d..03e3baf',
    title: 'End-to-end sessions and agent grants',
    summary: md('A routed stream is encrypted between its two daemons, so relays carry ciphertext only, and your agent goes to the members you grant, over any path.'),
    layers: [
      {
        label: 'What',
        body: md(
          "Each end of a routed stream sends a fresh key in the opening, signed with its member key; records after it are sealed per direction (x25519, HKDF, ChaCha20-Poly1305), so a relay can't read, change, inject, replay or reflect them (ADR 0046). Agent access is a per-member grant in `member.toml`, by key, set with `machine set <m> --agent on` and published in the link report.",
        ),
      },
      {
        label: 'Key commits',
        body: ul(['`a87642d` the session layer', '`24f68fb` routed streams are end-to-end sessions', '`e0b890c` agent grants per member', '`4e08ea4` agent over any path']),
      },
    ],
    links: [L.plan(), L.fleetDoc(), L.adr46, L.src('Sessions', 'src/serve/fleet/session.rs'), L.src('Grants', 'src/serve/fleet/grants.rs')],
  },
  {
    id: 'phase-step-11',
    tag: 'Step 11 · b8aaed8..45cb4e7',
    title: 'Moving the control plane',
    summary: md('`machine control-plane <m>` moves the role to another member; the links stay as they are.'),
    layers: [
      {
        label: 'What',
        body: md(
          "The move is a `control_plane` entry in the membership log, signed by the authority (typed elsewhere, proposed and approved there). Every reachable member rewrites its `control_plane` and cycles its links; one that was down learns it at its next link's key proof (`proof::control_plane_moved`). Only the role moves: the new control plane's view is the machines it links to.",
        ),
      },
      {
        label: 'Key commits',
        body: ul(['`5b8a979` the move, signed or proposed (decisions 68, 69)', '`b97a0be` a member that was down follows at its next proof']),
      },
    ],
    links: [L.plan(), L.fleetDoc(), L.src('Membership changes', 'src/serve/machines/changes.rs'), L.status],
  },
  {
    id: 'phase-step-12',
    tag: 'Step 12 · cbe1a80..4e40da8',
    title: 'Transfer v1',
    summary: md('devsandbox moves code itself: unpushed and uncommitted work, no remote needed, and brings it back.'),
    layers: [
      {
        label: 'What',
        body: md(
          "A placed run bundles each folder from its own checkout, with a WIP snapshot of its working tree (untracked files included, ignored ones not), into a per-repo cache on the worker. Repos are identified by their root commits; `code.haves` makes a repeat send carry only the delta. The control plane keeps a copy of each repo, best effort. `devsandbox fetch <machine>/<instance>`, also run before an `rm` or `done` typed on the placing host, pulls the branch and WIP back as hidden refs. The user's index, branches and stash are never touched.",
        ),
      },
      {
        label: 'Code',
        body: md('A snapshot travels on a ref of its own.'),
        snippets: [
          {
            title: 'src/serve/fleet/code/repo.rs',
            lang: 'rust',
            code: `pub fn wip_ref(branch: &str) -> String {
    format!("refs/devsandbox/wip/{branch}")
}`,
          },
        ],
      },
      {
        label: 'Key commits',
        body: ul([
          '`0122d17` repo identity, WIP snapshots, delta bundles',
          '`8d54cb2` per-repo caches and haves',
          '`6db6684` send from the checkout with a WIP snapshot, no origin',
          "`306f625` the control plane's copy",
          '`e1479af` `devsandbox fetch`; `fe9a6dc` rm and done fetch back first',
        ]),
      },
    ],
    links: [L.plan(), L.fleetDoc(), L.src('Code transfer', 'src/serve/fleet/code/mod.rs'), L.src('WIP snapshot', 'src/serve/fleet/code/wip.rs'), L.src('Fetch-back', 'src/serve/fleet/code/fetch.rs')],
  },
];

/** A row's drill-down: headless in the table, so title and summary don't show. */
const cmdCard = (id: string, layers: Layer[], links: SourceLink[]): Card => ({ id, title: id, summary: '', layers, links });

export const commands: CommandRow[] = [
  {
    cmd: 'machine add',
    before: md('Registers an ssh target in `machines.toml`; the link opens only while wanted.'),
    after: md('Joins both ends into one fleet, signed into its log; then holds `fleet-link --peer <me>` for good.'),
    change: md("You confirm the machine's key fingerprint and what it lends (GPUs, binds); `--control-plane` makes the target the control plane."),
    card: cmdCard(
      'cmd-machine-add',
      [
        {
          label: 'Today',
          body: md(
            "Install check, then a probe over ssh. The join rule puts both ends in one fleet, refusing two control planes and a taken name. The target's fingerprint is confirmed (`--yes` off a TTY); the add signs a new fleet, an `Add`, or an introduction proposed to the authority. It asks (or takes `--allow …`) what the target lends and applies it there. `--agent` grants this host's agent.",
          ),
        },
      ],
      [L.src('Join rule', 'src/serve/machines/join.rs'), L.src('Install check', 'src/serve/machines/install.rs'), L.src('machine verbs', 'src/commands/machine.rs'), L.machinesDoc()],
    ),
  },
  {
    cmd: 'machine ls',
    before: md('`machines.list`; AGENT column.'),
    after: md('Adds CAPACITY; TARGET `inbound` or `routed via <m>`; AGENT is this host\'s grant.'),
    change: md('Every member: those that dialed you, those reached through others, and their room.'),
  },
  {
    cmd: 'machine set --agent on|off',
    before: md('Starts or kills a background agent-relay ssh.'),
    after: md('A grant in `member.toml`, by the machine\'s key.'),
    change: md('No relay process; consent is per member, by key.'),
  },
  {
    cmd: 'machine unlink',
    before: md('**Not present** (this was `machine rm`).'),
    after: md('Drops this host\'s link and registry entry.'),
    change: md('The machine stays a fleet member.'),
  },
  {
    cmd: 'machine rm',
    before: md('`machines.rm`: drop the registry entry.'),
    after: md('Revokes the member\'s key, then the control plane forgets it.'),
    change: md('Takes the machine out of the fleet. `machine rm <this host>` leaves.'),
  },
  {
    cmd: 'machine key [rotate] · pending|approve|deny',
    before: md('**Not present.**'),
    after: md('The member key; changes proposed by other members, approved on the authority.'),
    change: md('New (step 10a).'),
  },
  {
    cmd: 'machine control-plane',
    before: md('**Not present.**'),
    after: md('Moves the control-plane role by a signed log entry.'),
    change: md('New (step 11). Links unchanged.'),
  },
  {
    cmd: 'machine trust · roots · allow',
    before: md('**Not present.**'),
    after: md('Local only: trust `worker|full`, the roots `worker` reaches, what placed work may use (GPUs, binds…).'),
    change: md('Each machine sets how much others may do on it; `--machine` refuses these.'),
    card: cmdCard(
      'cmd-machine-trust',
      [
        {
          label: 'What it decides',
          body: md(
            "Every request from another machine is served under this host's trust table for the key that peer proved (`fleet::lent`). `member.*` is never served on a peer connection, so only the machine itself changes its trust or allowances. `worker` warns when a machine the control plane's side controls dials this one.",
          ),
        },
      ],
      [L.src('Trust table', 'src/serve/fleet/lent.rs'), L.src('Member file', 'src/serve/fleet/member.rs'), L.machinesDoc()],
    ),
  },
  {
    cmd: 'fleet join|leave, lend',
    before: md('**Not present.**'),
    after: md('**Removed** (`b9cab24`).'),
    change: md('Use `machine add <control-plane>`. There is no `drain`.'),
  },
  {
    cmd: 'run',
    before: md('`--machine` / `m/name`: a verb over a fresh ssh. No placement.'),
    after: md('Runs here. `--machine`: a verb over the link or route. `--place`: the control plane chooses.'),
    change: md('Placement is asked for. A GPU sandbox goes only where one is lent; a tied or `placement = "home"` sandbox stays home.'),
    card: cmdCard(
      'cmd-run',
      [
        {
          label: 'Today',
          body: md(
            "A pinned run is a verb over a fresh `API` stream. `run --place` on a member asks the control plane's `fleet.place` among the machines both reach, then moves the code itself (`TRANSFER`) and forwards the verb, printing `placed on <m> (…)`. Unreachable control plane: runs here with a note.",
          ),
        },
      ],
      [L.src('Placement', 'src/serve/fleet/place.rs'), L.src('Candidates', 'src/serve/fleet/placed.rs'), L.src('Forwarded verbs', 'src/serve/machines/forward.rs'), L.adr56],
    ),
  },
  {
    cmd: 'fetch',
    before: md('**Not present.**'),
    after: md('Pulls a placed instance\'s branch and WIP back as hidden refs.'),
    change: md('New (step 12); `rm` and `done` of a placed instance fetch first.'),
  },
  {
    cmd: 'start / stop / rebuild / rm',
    before: md('A forwarded verb over a fresh ssh.'),
    after: md('A verb over a fresh `API` stream on the link, or routed.'),
    change: md('Trust-gated.'),
  },
  {
    cmd: 'exec',
    before: md('`ssh -t m devsandbox exec …`, re-dialed on exit 255.'),
    after: md('A `PTY` dial through `fleet-<m>.sock` (or the route); same reconnect loop.'),
    change: md('No ssh per terminal. `--service` is new.'),
  },
  {
    cmd: 'logs · inspect · done · rename · ls · ps · stats',
    before: md('An ssh re-run of the command.'),
    after: md('API calls via the daemon, rendered locally.'),
    change: md('Same output; a `worker` machine shows only what you may reach.'),
    card: cmdCard(
      'cmd-logs',
      [
        {
          label: 'Routing today',
          body: md('`calls::use_of` maps each named command to an API call, or to `Use::Link` (exec, port, verbs). A `denied` answer is reworded as coming from the machine.'),
        },
      ],
      [L.src('CLI calls', 'src/serve/machines/cli/calls.rs'), L.src('CLI routing', 'src/serve/machines/cli.rs')],
    ),
  },
  {
    cmd: 'gc --machine',
    before: md('One ssh `gc` per root, with no limits.'),
    after: md('A daemon verb per config root.'),
    change: md('**Refused** unless the machine trusts you `full`.'),
  },
  {
    cmd: 'port · Ports tab',
    before: md('One `ssh … tunnel` per port.'),
    after: md('One `TUNNEL` stream per bridge through the link or route.'),
    change: md('No ssh per port.'),
  },
  {
    cmd: 'agent forwarding',
    before: md('`ssh m devsandbox agent-relay` while wanted.'),
    after: md('`AGENT` streams to each granted member, over a link either way or routed.'),
    change: md('Only the members you grant get your agent.'),
  },
  {
    cmd: 'self-update',
    before: md('None: the protocol must match.'),
    after: md('Members follow the build the authority pinned in the log; one that can\'t is `behind: <why>`.'),
    change: md('Automatic.'),
    card: cmdCard(
      'cmd-update',
      [
        {
          label: 'Today',
          body: md(
            "`update::decide` against the log's latest `Op::Build` pin: download a published release (checked against signed `SHA256SUMS`), receive a dev build from a peer that has it (same sha256 and platform, `dev-builds` or `full`), or stay `behind` with the link served anyway. ADR 0053.",
          ),
        },
      ],
      [L.src('Update decision', 'src/serve/fleet/update.rs'), L.adr53],
    ),
  },
  {
    cmd: 'vscode / inbox on m',
    before: md('Refused.'),
    after: md('Refused.'),
    change: md('Unchanged.'),
  },
];

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
            { name: 'devsandbox serve', serve: true, body: ['link to K: only while wanted (+60 s), rows in memory'] },
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
      { from: 'L', to: 'K', label: '(1) API link', detail: md("The remote `api --stdio` relay is a full client of K's daemon, held while wanted plus 60 s.") },
      { from: 'L', to: 'K', label: '(2) verb', detail: md('A fresh ssh per `run/stop/…`, so a build never holds up the link.') },
      { from: 'L', to: 'K', label: '(3) CLI re-run / tab', detail: md('`ssh [-t] K devsandbox <args>`; a tty exec loops on exit 255.') },
      { from: 'L', to: 'K', label: '(4) forward', detail: md('The far end `devsandbox tunnel` becomes a `devsbd bridge`.') },
      { from: 'L', to: 'K', label: '(5) agent relay', detail: md("On K, `agent-relay` binds `agent-machine_L.sock` and serves L's agent.") },
      { from: 'K', to: 'B', label: '(6) the same five kinds', detail: md("For K's own use. Nothing L does reaches B: federation is one hop.") },
    ],
    note: md('K reachable by ssh from L, B by ssh from K, L accepts no ssh. L ran `machine add K`; K ran `machine add B`.'),
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
          role: 'control plane',
          items: [
            {
              name: 'fleet-link --worker L',
              icon: 'cable',
              body: ['binds', { sockets: [{ name: 'fleet-L.sock' }, { name: 'fleet-L.pty.sock' }, { name: 'fleet-L.tunnel.sock' }, { name: 'fleet-L.transfer.sock' }] }],
            },
            { hop: [{ n: 2, label: 'fleet.offer' }] },
            { name: 'devsandbox serve', serve: true, sub: 'control plane', body: ['entry L offered=true; places runs on L only'] },
          ],
        },
        { id: 'B', role: 'box (ssh machine)', items: [] },
      ],
      gaps: [
        {
          dialer: 'L',
          dialed: 'K',
          links: [{ n: 1, label: 'one ssh, frame mux', held: true, carries: ['K opens API PTY TUNNEL TRANSFER', "L opens AGENT (K's agent)"] }],
        },
        {
          dialer: 'K',
          dialed: 'B',
          links: [{ n: 3, label: 'B still per-use ssh', tone: 'runtime', carries: ['api link (+link.attach{peer})', 'verbs, tty/cli, tunnel, agent-relay'] }],
        },
      ],
    },
    arrows: [
      { from: 'L', to: 'K', label: '(1) fleet link', detail: md("L dials K and answers K's streams under the `lent` table: the fleet root, plus L's own roots from step 6.") },
      { from: 'fleet-link', to: 'K serve', label: '(2) offer', detail: md('`fleet.offer` writes the offered entry; K reaches L via `fleet-L.sock`.') },
      { from: 'K', to: 'B', label: '(3) ssh machine', detail: md('Unchanged per-use ssh. Placement never picks B; `--machine B` reaches it.') },
    ],
    note: md("Superseded. L ran `fleet join K`; K ran `machine add B`. K's agent reaches L; L's own agent never reaches K."),
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
                { sockets: [{ name: 'fleet-K.sock' }, { name: 'route-B.sock', note: 'B via K' }] },
                "IN: K's streams in-process, L's trust, root fleet/K/",
                "member of K's fleet: control_plane = K, key proven on the link",
              ],
            },
          ],
        },
        {
          id: 'K',
          role: 'control plane',
          items: [
            {
              name: 'fleet-link --peer L',
              icon: 'cable',
              body: ['binds', { sockets: [{ name: 'fleet-L.sock' }, { name: 'agent-link-L.sock', note: 'if L grants K' }] }],
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
              sub: 'control plane, authority',
              body: [
                'control_plane = "self"; signs membership.json',
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
              body: ['binds', { sockets: [{ name: 'route-L.sock', note: 'L via K' }, { name: 'agent-route-L.sock', note: 'if L grants B' }] }],
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
              carries: ['either end: API PTY TUNNEL TRANSFER, AGENT by grant', 'ROUTE: L-B streams, sealed end to end'],
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
      { from: 'L', to: 'K', label: '(1) peer link', detail: md("L dials `fleet-link --peer L` on K. After the hellos (each naming its control plane) both ends prove their member keys against `membership.json`, the end behind taking the entries it lacks. L's clients reach K through `fleet-K.sock`; K's streams to L are served in-process under L's trust, fleet root `fleet/K/`.") },
      { from: 'fleet-link', to: 'K serve', label: '(2) attach', detail: md("Each stream goes to K's daemon with `link.attach` (peer, proven key, channel), served under K's trust for L, fleet root `fleet/L/`.") },
      { from: 'fleet-link', to: 'K serve', label: '(3) announce', detail: md("`links.inbound` for the link's life: K's `Machines` then holds L over `fleet-L.sock`, and L is a placement candidate.") },
      { from: 'K', to: 'B', label: '(4) peer link', detail: md("The same as (1) with K the dialer. B binds `agent-link-K.sock` while K serves AGENT to it.") },
      { from: 'L', to: 'B', label: 'routed stream', detail: md("With no link to B, L binds `route-B.sock` from the link graph. Each stream opens on `ROUTE` naming L, B and the path; K checks its next hop and splices; B proves L's key (ADR 0045) and serves it under B's trust for L. Both ends seal every record (ADR 0046).") },
      { from: 'B', to: 'L', label: 'agent grant', detail: md("If L ran `machine set B --agent on`, B probes L over the route and binds `agent-route-L.sock` while L serves. K never sees the agent's bytes.") },
      { from: 'L', to: 'K', label: 'run --place', detail: md("L asks K's `fleet.place`; K asks each candidate both reach for capacity and answers. If B wins, L sends the root and bundles over `route-B.sock` into B's `fleet/L/`, then forwards the verb. K unreachable: it runs on L with a note.") },
    ],
    note: md(
      "K ran `machine add B --agent` (a fleet of two: K its control plane and authority), then L ran `machine add K` (K signs L's `Add`). L sees B through K's federated view, as `routed via K`. Links keep both daemons up.",
    ),
  },
];

export const steps: Card = {
  id: 'left-steps',
  tag: 'Planned · not built',
  title: 'Rollout steps 13–18',
  summary: md("From the plan's rollout; letters are its sizes. Steps 1–12 are on `main`; none of these is."),
  layers: [
    {
      label: 'The steps',
      body: ul([
        "**13 Children on any machine (M).** Re-scoped: an external orchestrator's children, placed and forwarded; waits on the orchestrator interface.",
        '**14 Checkpoints and re-placement (M–L).** Placed work checkpoints to the control plane; a child whose machine is gone restarts elsewhere (`devsandbox move` by hand).',
        '**15 The rest of the fleet view (M).** Nameless commands across the fleet.',
        "**16 Sync layer (L, later).** What git can't carry: build outputs, volumes, caches, live editing.",
        '**17 Control-plane handoff (M).** Moving the role while the old one is alive ships its state to the new one.',
        '**18 Fallback control plane (M, later).** Another member takes over when the control plane has been gone a while.',
      ]),
    },
  ],
  links: [L.plan(), L.status],
};

export const limits: Card = {
  id: 'left-limits',
  tag: 'docs/status.md',
  title: 'Known limitations on main',
  summary: md("The main ones from the status page's machines section; it has the full list."),
  layers: [
    {
      label: 'Limitations',
      body:
        '<p><strong>Placement and moving code</strong></p>' +
        ul([
          "Only `run --place` is placed, and only by the daemon; a dispatcher's children aren't placed (step 13), and the placing host doesn't record placed instances.",
          'Ties (a bind outside what moves, `${localEnv:…}`, `privileged`…) keep a sandbox home whatever a worker allows; a pin can only say home. GPUs are the exception.',
          '`shared-volumes/`, caches and shell history start empty on the worker; `worktree-link` and `worktree-include` are skipped there.',
          'A first send goes whole; worker caches, moved trees and the control plane\'s copies are never pruned.',
          "Fetch-back brings only the `folder` repo; a `--base` naming `origin/…` fails on the worker.",
          "Soft caps are hand-edited and measured against the whole host; `has_image` reads watched snapshots only.",
          "A machine can't stop taking placed work while staying reachable (no `drain`).",
        ]) +
        '<p><strong>Membership and keys</strong></p>' +
        ul([
          'Fleets made before member identity have no log: remake them with `machine add`.',
          'An introduced (capped) member can\'t be removed or rotate its key until the authority adds it.',
          'A member removed while unreachable keeps its entries until reachable; one that leaves is forgotten once the authority approves.',
          'No command sets the member name.',
        ]) +
        '<p><strong>Control plane and routing</strong></p>' +
        ul([
          "`machine control-plane` moves the role only, and needs the authority up. Routed machines' rows come only from the control plane's view, one hop.",
          'A routed stream doesn\'t fail over once open; members more than four hops away are unreachable.',
          "Link reports are versioned by their signer's clock.",
        ]) +
        '<p><strong>Agent and versions</strong></p>' +
        ul([
          'With several hosts granting one machine, processes there use the newest agent socket.',
          "A published pin's download hasn't run against a real release yet. A dev pin is taken only with `dev-builds = true` or `full` trust on the same OS/arch; a member that can't update drops the link at each dial.",
        ]) +
        '<p><strong>Links and machines</strong></p>' +
        ul([
          'An inbound machine exists only while its link is up; its forwards aren\'t restored.',
          'Remote rows live in memory except on the control plane.',
          "Verbs from a linked host get the daemon's env, not a login shell's.",
          'Daemon-side ssh is `BatchMode=yes`: passphrase-prompting keys fail.',
          '`vscode` and `inbox` are refused on machines; nameless commands are local only.',
        ]),
    },
  ],
  links: [L.status, L.machinesDoc(), L.plan()],
};
