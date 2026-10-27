/// <reference types="node" />

// Mirrors the serde output of the Rust CLI's `--json` verbs (src/snapshot.rs,
// src/runtime/mod.rs). Rust `Option<T>` serializes as `T | null`.

/** JSON envelope version this package understands. */
export declare const SCHEMA: 1;

// ---------------------------------------------------------------------------
// Payloads

/**
 * Container liveness; `text` is the runtime's raw status (e.g. "Up 3 minutes").
 * `unknown`: the runtime wasn't asked (`instances.list` with `depth: 'disk'`);
 * never in `status()`.
 */
export type ContainerStatus =
  | { state: 'running'; text: string }
  | { state: 'exited'; text: string }
  | { state: 'missing' }
  | { state: 'unknown' };

/**
 * Where an instance's creation stands while it isn't ready. `step` is
 * `preparing`, `building`, `creating` or a lifecycle step's name
 * (`postCreateCommand`, …); a failed one is kept until `rebuild` retries it
 * or `rm` removes it (`error` is `interrupted` when its process died).
 */
export type PhaseRow =
  | { state: 'in-flight'; step: string }
  | { state: 'failed'; step: string; error: string };

/** One instance (`status().instances`). */
export interface InstanceRow {
  name: string;
  sandbox: string;
  container: string;
  status: ContainerStatus;
  uptime_secs: number;
  cpu: string | null;
  mem: string | null;
  folder: string;
  worktree: boolean;
  services: string[];
  workspace: string;
  remote_user: string | null;
  remote_env_len: number;
  base_folder: string;
  /** Container no longer matches the current config/dockerfile; `rebuild` recreates it. */
  drift: boolean;
  /** Marked done (`done()`); kept as is until removed. */
  done: boolean;
  /** Still being created, or failed; absent once ready. Shown instead of `status` while set. */
  phase?: PhaseRow;
  /** The config root it was made from; `null` for instances older than that record. */
  root: string | null;
  /** The machine it's on, in a federated snapshot (`instances.watch` with `machines`); absent for this host's. */
  machine?: string;
}

/** One `[sandbox.*]` from devsandboxes.toml (`ls()`, `status().sandboxes`). */
export interface SandboxRow {
  name: string;
  /** `image X`, `dockerfile Y`, or `?` when the sandbox failed to resolve. */
  source: string;
  folder: string | null;
  services: string[];
  /** `extends` template chain. */
  extends: string[];
  config_hash: string;
  build_hash: string;
  /** Config-validation problems (folder / mount resolution). */
  issues: string[];
  /** What in its config uses `/root` and breaks under `identity = "host"` (`config.findings`, this sandbox's). */
  identity: IdentityNote[];
  /** Has a `dispatcher` table (docs/architecture/automations/dispatchers.md). */
  dispatcher: boolean;
  /** The resolved `worktree-branch`; `null` when unset or unresolved. */
  worktree_branch: string | null;
  /** The config root it was read from. */
  root: string | null;
  /** As {@link InstanceRow.machine}. */
  machine?: string;
}

/** One identity finding on a {@link SandboxRow}. */
export interface IdentityNote {
  /** The finding's id (as `config.findings`' `id`, what `config.ack` takes). */
  id: string;
  /** The update as one line, or the path to fix by hand. */
  text: string;
  /** Acked ("continue as is") for this config root. */
  acked: boolean;
}

/** One `[services.*]` from devsandboxes.toml (`service.ls()`, `status().services`). */
export interface ServiceRow {
  name: string;
  scope: 'global' | 'isolated';
  source: string;
  ports: string[];
  /** Backing containers as `[containerName, status]` pairs. */
  containers: Array<[string, ContainerStatus]>;
  /** Instances whose sandbox references this service. */
  used_by: string[];
  env_len: number;
  command: string | null;
  config_hash: string;
  drift: boolean;
  /** The config root it was read from. */
  root: string | null;
  /** As {@link InstanceRow.machine}. */
  machine?: string;
}

/** Full snapshot (`status()`), the same data the TUI renders. */
export interface Snapshot {
  instances: InstanceRow[];
  sandboxes: SandboxRow[];
  services: ServiceRow[];
  sandbox_count: number;
  runtime_name: 'docker' | 'podman' | 'container';
  runtime_version: string | null;
  /** Collection problem (e.g. runtime down); rows still come from local state. */
  error: string | null;
}

/** How much of the runtime `instances.list` asks: `full` (the default) adds cpu/mem. */
export type SnapshotDepth = 'disk' | 'listing' | 'full';

/** `instances.list`: the {@link Snapshot} plus the dispatchers' children. */
export interface InstancesList extends Snapshot {
  /** Dispatcher `instance_id` → thread key → child instance name. */
  thread_children: Record<string, Record<string, string>>;
}

/**
 * `instances.snapshot`: a watched root's {@link InstancesList}, pushed on every
 * poll (about every 2 s; cpu/mem refreshed at most every 7 s and carried over
 * in between), so its arrival says how fresh it is.
 */
export interface InstancesSnapshot extends InstancesList {
  /** The config root, as `instances.watch` named it. */
  dir: string;
  /**
   * With `instances.watch`'s `machines`: every registered machine, sorted by
   * name. The rows then include each machine's (tagged `machine`), last known
   * while it isn't `connected`.
   */
  machines?: SnapshotMachine[];
}

/** Where a machine's link stands. `idle`: not wanted, so not connected. */
export type LinkState = 'idle' | 'connecting' | 'connected' | 'unreachable';

/** One machine of a federated {@link InstancesSnapshot}. */
export interface SnapshotMachine {
  name: string;
  /** What ssh takes, or an `ssh://` URL. */
  target: string;
  state: LinkState;
  /** Why it's `unreachable` (or a root it refused). */
  error: string | null;
  /** Its devsandbox semver, once reached. */
  version: string | null;
  /** Its runtime (`runtime_name`), once a snapshot came. */
  runtime: string | null;
  /** The remote devsandbox as shell text; `null` is `devsandbox` on its `PATH` (for an ssh argv). */
  command: string | null;
  /** This host's ssh-agent is relayed to it (`machines.set`). */
  agent: boolean;
}

/**
 * The instance `run()` just created (`run --json`). Fields match
 * {@link InstanceRow} where they overlap, except `worktree`, which is the path.
 */
export interface RunRecord {
  /** Instance name; what every `name` argument accepts. */
  name: string;
  /** Persistent id; differs from `name` after a rename. */
  instance_id: string;
  sandbox: string;
  container: string;
  /** Workspace folder inside the container. */
  workspace: string;
  /** Host folder mounted as the workspace (the worktree for a worktree instance). */
  folder: string;
  /** Host base repo folder the instance derives from. */
  base_folder: string;
  /** Host worktree path; `null` when the instance runs on `base_folder`. */
  worktree: string | null;
  /** Worktree branch; `null` without a worktree. */
  branch: string | null;
}

/** One container from the runtime listing (`ps()`). */
export interface ContainerRow {
  name: string;
  image: string;
  /** Human status (`Up 3 minutes`, `Exited (0) 1 hour ago`, …). */
  status: string;
  /** `running`, or anything else (stopped/exited/created…). */
  state: string;
  labels: Record<string, string>;
  host_ports: string[];
  /** The container's instance is marked done (`false` for service containers). */
  done: boolean;
  /** The container's instance is still being created, or failed; absent once ready. */
  phase?: PhaseRow;
}

/** Resource usage of one running container (`stats()`), pre-rendered. */
export interface StatsRow {
  name: string;
  cpu: string;
  mem: string;
}

// ---------------------------------------------------------------------------
// Options & results

export interface CommonOptions {
  /** Config root containing devsandboxes.toml (the CLI's `-C`). Defaults to `cwd`. */
  dir?: string;
  cwd?: string;
  env?: NodeJS.ProcessEnv;
  signal?: AbortSignal;
  /** `inherit` streams the CLI's progress output to this process's stderr. Default `pipe`. */
  stderr?: 'pipe' | 'inherit';
}

export interface CliOptions extends CommonOptions {
  /** Written to the process's stdin; stdin is closed otherwise. */
  input?: string | Buffer;
  /** Reject with {@link DevsandboxError} on a non-zero exit. Default `true`. */
  reject?: boolean;
}

export interface CliResult {
  exitCode: number | null;
  signal: NodeJS.Signals | null;
  stdout: string;
  /** Empty when `stderr: 'inherit'`. */
  stderr: string;
}

/** An instance name (or sandbox / folder name), or every instance. */
export type Target = string | { all: true };

/** Rejection for a failed CLI invocation. */
export declare class DevsandboxError extends Error {
  readonly name: 'DevsandboxError';
  readonly args: string[];
  readonly exitCode: number | null;
  readonly signal: NodeJS.Signals | null;
  readonly stdout: string;
  readonly stderr: string;
}

// ---------------------------------------------------------------------------
// API

/** Absolute path of the native binary for this platform (`DEVSANDBOX_BINARY` overrides). */
export declare function binaryPath(): string;

/** Run the CLI with raw arguments. */
export declare function cli(args: readonly string[], opts?: CliOptions): Promise<CliResult>;

/** Sandbox configs in devsandboxes.toml. */
export declare function ls(opts?: CommonOptions): Promise<SandboxRow[]>;
/** Running devsandbox containers (`all` includes stopped ones). */
export declare function ps(opts?: CommonOptions & { all?: boolean }): Promise<ContainerRow[]>;
/** CPU/memory of running devsandbox containers. */
export declare function stats(opts?: CommonOptions): Promise<StatsRow[]>;
/** Sandboxes, instances, and services in one snapshot. */
export declare function status(opts?: CommonOptions): Promise<Snapshot>;
/** The runtime's own inspect document (docker/podman: an array of containers). */
export declare function inspect<T = unknown>(name: string, opts?: CommonOptions): Promise<T>;

export interface RunOptions extends CommonOptions {
  /** Instance name; generated when omitted. */
  name?: string;
  /** Worktree branch (supports `${instance}`). */
  branch?: string;
  /** Start point for the worktree branch, e.g. `origin/develop`. */
  base?: string;
}
/** Create and start an instance of `sandbox`; resolves with where it lives. */
export declare function run(sandbox: string, opts?: RunOptions): Promise<RunRecord>;
export declare function start(target: Target, opts?: CommonOptions): Promise<void>;
export declare function stop(target: Target, opts?: CommonOptions): Promise<void>;
/** Recreate from current config when drifted (`force`: even without drift). */
export declare function rebuild(target: Target, opts?: CommonOptions & { force?: boolean }): Promise<void>;
export declare function rename(name: string, newName: string, opts?: CommonOptions): Promise<void>;
/** Mark an instance done: kept as is (container, worktree), shown dimmed until removed. */
export declare function done(name: string, opts?: CommonOptions): Promise<void>;
/** Clear an instance's done mark. */
export declare function undone(name: string, opts?: CommonOptions): Promise<void>;
/**
 * Remove container, worktree, and state entry. Rejects, removing nothing,
 * when the worktree has uncommitted or untracked changes, unless `force`.
 */
export declare function rm(
  name: string,
  opts?: CommonOptions & {
    /**
     * The worktree branch `run` created: `true` deletes it (`git branch -D`,
     * unmerged commits too), `false` or omitted keeps it. Branches `run`
     * reused are always kept.
     */
    deleteBranch?: boolean;
    /**
     * Remove anyway: uncommitted or untracked worktree changes are
     * discarded, and a failed step (logged on stderr) is skipped so the
     * instance still leaves state; what failed stays on disk.
     */
    force?: boolean;
  },
): Promise<void>;
/** Remove unreferenced services (`force`: also orphaned shell-history files). */
export declare function gc(opts?: CommonOptions & { force?: boolean }): Promise<void>;

/** Last `lines` (default 50) lines of a container's merged stdout/stderr. */
export declare function logs(name: string, opts?: CommonOptions & { lines?: number }): Promise<string>;

export interface ExecOptions extends CommonOptions {
  /** Piped to the command's stdin (passes `-i`). */
  input?: string | Buffer;
}
/**
 * Run `command` in an instance. Resolves with its exit code rather than
 * rejecting on non-zero.
 */
export declare function exec(name: string, command: readonly string[], opts?: ExecOptions): Promise<CliResult>;

export interface ExecArgvOptions {
  /** Pass `-t` (allocate a pseudo-TTY). */
  tty?: boolean;
  /** Pass `-i` (keep stdin open). */
  interactive?: boolean;
}
/** A spawnable `devsandbox exec` invocation: `spawn(file, args)`, no shell quoting. */
export interface ExecInvocation {
  /** The native binary, as `binaryPath()` returns it. */
  file: string;
  args: string[];
}
/**
 * Argv for running `cmd` in an instance under your own process or pty
 * (node-pty, `child_process.spawn`). Without `cmd`, a login shell (zsh, bash,
 * or sh). With neither flag set the CLI picks: `-i`, plus `-t` when its stdin
 * is a TTY (as under a pty); setting either passes exactly what you set, so
 * set both for an interactive shell.
 */
export declare function execArgv(name: string, cmd?: readonly string[], opts?: ExecArgvOptions): ExecInvocation;

export declare const service: {
  ls(opts?: CommonOptions): Promise<ServiceRow[]>;
  /** Recreate a service's containers and rewire running sandboxes in place. */
  rebuild(name: string, opts?: CommonOptions): Promise<void>;
};

// ---------------------------------------------------------------------------
// Daemon API

// The host daemon's JSON-lines API (docs/reference/api.md), over `devsandbox api
// --stdio`. Mirrors the serde structs in src/serve/{api,proto,forwards}.rs;
// a Rust test (src/serve/dts.rs) fails when a wire struct and its interface
// here disagree.

/** The API protocol number this package speaks; `connect()` rejects any other. */
export declare const PROTOCOL: 1;

/** `hello` params: what `connect()` sends first. */
export interface HelloParams {
  /** The client's devsandbox version; `connect()` sends `0.0.0` (the relay already handled any version handoff). */
  version: string;
  /** Unix mtime of a dev build; 0 otherwise. */
  build: number;
  /** Who's calling (`npm:<name>`), for `serve.log`. */
  client: string;
}

/** The daemon's answer to `hello` (`api.daemon`). */
export interface HelloResult {
  version: string;
  build: number;
  /** The API protocol number; see {@link PROTOCOL}. */
  protocol: number;
  /** The client is newer: the daemon is exiting. */
  handoff?: boolean;
}

/** An error response's body: `code` is stable, `message` is for humans. */
export interface ApiErrorBody {
  code: string;
  message: string;
  /** Details some codes carry: {@link VerbFailedData} for `failed`. */
  data?: unknown;
}

/** What a mutation answers. */
export interface OkResult {
  ok: true;
}

/** A thread state; `needs-you` is waiting on the user. */
export type ThreadState = 'needs-you' | 'active' | 'done';

/** An `inbox.threads.list` view, as in the dashboard. */
export type InboxView = 'needs-you' | 'active' | 'done' | 'all';

/** A notification topic for `subscribe`. */
export type ApiTopic = 'inbox' | 'instances' | 'forwards';

/** One Inbox thread (`inbox.list()`), and the head of {@link ThreadDetail}. */
export interface ThreadSummary {
  /** Store id, stable for the thread's life: what `{ thread: id }` takes. */
  id: number;
  /** Owner's `instance_id`. */
  owner: string;
  /** Owner's instance name when it last wrote. */
  owner_name: string;
  /** Thread key; `null` on an unkeyed notification. */
  key: string | null;
  /** `thread` (`devsbd thread put`) or `notify` (`devsbd notify` records). */
  kind: 'thread' | 'notify';
  /** `null` on a notification. */
  state: ThreadState | null;
  /** The owner's status line. */
  status: string | null;
  /** A thread's title; a notification's newest message, first line. */
  title: string;
  /** A notification's newest level. */
  level: 'info' | 'warn' | 'error' | null;
  unread: boolean;
  /** The owner was removed: read-only history. */
  archived: boolean;
  /** In the `needs-you` view (what badges count). */
  needs_you: boolean;
  /** Unix seconds of the last change. */
  changed_at: number;
}

/** A thread with everything the dashboard's pane shows (`inbox.get()`). */
export interface ThreadDetail extends ThreadSummary {
  /** `http(s)` link. */
  link: string | null;
  /** The key, among the owner's dispatcher children, of the child it's about; `null`: the owner itself. */
  child: string | null;
  /** Set when the thread takes replies (`inbox.reply()`). */
  compose: ThreadCompose | null;
  actions: ThreadAction[];
  /** An owner thread's feed, oldest first (first-insert order). */
  feed: FeedItem[];
  /** A notification's records, newest first. */
  notes: ThreadNote[];
  /** Events the owner hasn't acked yet. */
  events_pending: number;
}

/** How a thread takes free-text replies. */
export interface ThreadCompose {
  placeholder: string | null;
  /** What sending does now, shown under the box. */
  hint: string | null;
}

/** An owner's action button (`inbox.act()`). */
export interface ThreadAction {
  id: string;
  label: string;
  /** Pressing it also sets the thread done. */
  done: boolean;
  /** The host verb it runs in the client that shows it; the API runs none. */
  host: 'vscode' | 'terminal' | 'logs' | 'forward' | 'open' | 'rm' | null;
  /** `false`: host-only, which `inbox.act()` refuses. */
  sends_event: boolean;
}

/** One item of a thread's feed, by `type`. */
export type FeedItem = FeedMessage | FeedReply | FeedAction | FeedSubmission | FeedMarker;

/** What every feed item carries. */
export interface FeedItemBase {
  /** Arrival order across the whole Inbox. */
  seq: number;
  /** Unix seconds of first insert. */
  at: number;
}

/** A message from the thread's owner. */
export interface FeedMessage extends FeedItemBase {
  type: 'message';
  /** The owner's id for it, unique in the thread. */
  id: string;
  blocks: MessageBlock[];
  /** Replaced in place since it was first sent. */
  edited: boolean;
  /** The owner took it back. */
  withdrawn: boolean;
}

/** The user's reply (`inbox.reply()`). */
export interface FeedReply extends FeedItemBase {
  type: 'reply';
  text: string;
}

/** The user pressed an action (`inbox.act()`). */
export interface FeedAction extends FeedItemBase {
  type: 'action';
  /** The action's id. */
  action: string;
  /** Its label when pressed. */
  label: string;
}

/** The user submitted a form (`inbox.form.submit`). */
export interface FeedSubmission extends FeedItemBase {
  type: 'submission';
  /** The message holding the form. */
  message: string;
  /** The form's id. */
  form: string;
  /** Every question's answer; `null` for an optional one left unanswered, or a follow-up that doesn't apply. */
  answers: Record<string, FormAnswer | null>;
}

/** A one-line marker: the user's done/reopen, the owner's state or status change. */
export interface FeedMarker extends FeedItemBase {
  type: 'marker';
  marker: 'done' | 'reopen' | 'state' | 'status';
  /** `state`/`status`: the value before; `null` otherwise (or no status). */
  from: string | null;
  /** `state`/`status`: the value after; `null` otherwise (or no status). */
  to: string | null;
}

/** One block of a message, by `type`. */
export type MessageBlock = MarkdownBlock | FieldsBlock | FormBlock;

/** Markdown text. */
export interface MarkdownBlock {
  type: 'markdown';
  text: string;
}

/** A compact key/value list, shown as aligned `label  value` rows. */
export interface FieldsBlock {
  type: 'fields';
  items: MessageField[];
}

/** One row of a `fields` block. */
export interface MessageField {
  label: string;
  value: string;
}

/**
 * Questions for the user, answered once (`inbox.form.submit`). A submitted
 * form is frozen: re-sends of its message never change it.
 */
export interface FormBlock {
  type: 'form';
  id: string;
  title: string | null;
  /** The submit button's label. */
  submit: string;
  questions: FormQuestion[];
  /** `withdrawn`: the owner withdrew the message or dropped the form. */
  state: 'open' | 'submitted' | 'withdrawn';
  /** Saved partial answers (`inbox.form.saveDraft`) of an open form; `{}` otherwise. */
  draft: Record<string, FormAnswer>;
  /** Once submitted, every question's answer (`null`: optional and unanswered, or a follow-up that doesn't apply); `null` before. */
  answers: Record<string, FormAnswer | null> | null;
}

/** One question of a form, by `type`. */
export type FormQuestion = ChoiceQuestion | TextQuestion | ConfirmQuestion;

/** An answer: an option id (choice), option ids (`multiple` choice), text, or a confirm's yes/no. */
export type FormAnswer = string | string[] | boolean;

/** What every question carries. */
export interface QuestionBase {
  /** Unique in the form; what answers are keyed by. */
  id: string;
  /** Inline markdown, one line. */
  label: string;
  /** Markdown shown above the input. */
  context: string | null;
  /** Must be answered to submit (text: not blank; `multiple`: one pick at least). */
  required: boolean;
  /** A follow-up of this earlier question, shown under it; `null`: top-level. */
  under: string | null;
  /**
   * With `under` a choice: the option this follow-up is for. It applies only
   * while that option is picked (else hidden, never required, answered `null`).
   */
  when: string | null;
}

/** Pick one option, or several when `multiple`. */
export interface ChoiceQuestion extends QuestionBase {
  type: 'choice';
  options: ChoiceOption[];
  multiple: boolean;
  /** An option id, or ids when `multiple`. */
  default: string | string[] | null;
}

/** One option of a choice question. */
export interface ChoiceOption {
  id: string;
  label: string;
  description: string | null;
}

/** Free text. */
export interface TextQuestion extends QuestionBase {
  type: 'text';
  placeholder: string | null;
  /** Prefilled, editable. */
  default: string | null;
  multiline: boolean;
  /** Longest answer, in bytes. */
  max: number;
}

/** Yes or no. */
export interface ConfirmQuestion extends QuestionBase {
  type: 'confirm';
  /** Label of the yes choice; `null`: the client's own. */
  yes: string | null;
  /** Label of the no choice; `null`: the client's own. */
  no: string | null;
  default: boolean | null;
}

/** `inbox.form.*` params: the message holding the form, and answers by question id. */
export type FormParams = ThreadAddress & { message: string; answers?: Record<string, FormAnswer> };

/** One record of a notification thread. */
export interface ThreadNote {
  id: number;
  level: 'info' | 'warn' | 'error';
  msg: string;
  link: string | null;
  /** Unix seconds. */
  at: number;
}

/** A thread by store id, or an owner thread by owner (instance name or id) and key. */
export type ThreadAddress = { thread: number } | { owner: string; key: string };

/** One notification thread by store id, or every one. */
export type NotifyTarget = { thread: number } | { all: true };

/** One daemon port forward (`forwards.list()`). */
export interface ForwardRow {
  /** Daemon-wide id: what `forwards.rm()` takes. */
  id: number;
  /** The canonical config root; with `machine`, the root on that machine. */
  dir: string;
  /** The bound host address, e.g. `127.0.0.1:3000`. */
  local: string;
  /** Route label, e.g. `api:3000`; empty until the route first resolves. A remote forward's is `<machine>/<label>`, best effort until the machine first answers. */
  target: string;
  /** The listening process (`node (pid 412)`). */
  process: string | null;
  /** `active`, `connecting`, or `error: <reason>`. */
  state: string;
  /** Open connections. */
  conns: number;
  /** From a sandbox's `forwardPorts`, not `forwards.add()`. */
  configured: boolean;
  /** The machine whose instance or service it reaches (`forwards.add()` with `machine`); absent for this host's. */
  machine?: string;
}

/** `forwards.add()` params; `instance`, `service`, or both. */
export interface ForwardAddParams {
  /** Config root (the CLI's `-C`); resolved against `process.cwd()`. With `machine`, the absolute root on that machine. */
  dir: string;
  /** Instance name, as the CLI takes it (never prompts). */
  instance?: string;
  /** Forward one of its services instead. */
  service?: string;
  /** Host address to bind. Default `127.0.0.1`. */
  address?: string;
  /** `port` (that host port or the next free one up) or `host:port` (exactly). */
  spec: string;
  /** A registered machine's instance or service (then `dir` is its config root there and `instance` its name there); the forward still binds on this host. */
  machine?: string;
}

/** A forward `forwards.add()` bound. */
export interface ForwardAdded {
  id: number;
  local: string;
  /** Route label, best effort this early. */
  target: string;
}

/** `instances.resolve`'s answer: every instance the name could mean, sorted (several: the caller picks). */
export interface InstancesResolved {
  /** With `machines`: a machine's matches qualified, `<machine>/<name>`. */
  matches: string[];
  /** `matches` in order, with where each lives (absent from older daemons). */
  found?: ResolvedInstance[];
}

/** One {@link InstancesResolved} match. */
export interface ResolvedInstance {
  /** Its name on its own host (unqualified). */
  name: string;
  /** `null`: this host. */
  machine: string | null;
  /** Its config root on its host; `null` when not recorded. */
  root: string | null;
  /** Its sandbox; `null` from an older machine. */
  sandbox: string | null;
}

/** `instances.cancel`'s answer: the in-flight verb's pid was SIGINTed. */
export interface InstanceCancelled {
  ok: true;
  pid: number;
}

/** `instances.rename`'s answer: the new (trimmed) name. */
export interface InstanceRenamed {
  instance: string;
}

/** `instances.done`'s answer: the instance's done flag as it now is. */
export interface InstanceDone {
  instance: string;
  done: boolean;
}

/** One `sandboxes.list` entry (not the snapshot's {@link SandboxRow}). */
export interface SandboxEntry {
  name: string;
  /** With `sandboxes.list`'s `machines`: the machine defining it; absent for this host's. */
  machine?: string;
  /** The config root defining it: always in `sandboxes.list`, absent in `roots.list`. */
  root?: string;
}

/** One `roots.list` entry: a config root this host knows of. */
export interface RootCandidate {
  /** Absolute, canonicalized (a root that's gone: as recorded). */
  root: string;
  /** Sorted; `[]` when `error` is set or there's no config file. */
  sandboxes: SandboxEntry[];
  /** Why the root's sandboxes can't be read (gone, a config that doesn't load). */
  error: string | null;
}

/** `machines.probe`'s answer: the machine's devsandbox and its candidate roots. */
export interface MachineProbe {
  version: string;
  protocol: number;
  roots: RootCandidate[];
}

/** `machines.add`'s answer. */
export interface MachineAdded {
  name: string;
  /** As saved: the machine's canonical paths, deduplicated. */
  roots: string[];
  /** The machine's devsandbox semver. */
  version: string;
  /** Set when that semver differs from this daemon's. */
  warning: string | null;
}

/** `machines.get`'s answer: what an ssh command to the machine needs. */
export interface MachineInfo {
  name: string;
  /** What ssh takes, or an `ssh://` URL. */
  target: string;
  /** The remote devsandbox as shell text; `null` is `devsandbox` on its `PATH`. */
  command: string | null;
  /** Config roots on the machine. */
  roots: string[];
  /** This host's ssh-agent is relayed to it (`machines.set`). */
  agent: boolean;
}

/** `machines.hold`'s answer. */
export interface MachineHeld {
  ok: true;
  /** The machine's agent relay is up; always `false` for a machine with `agent` off. */
  agent: boolean;
}

/** One registered machine (`machines.list`). */
export interface MachineRow {
  name: string;
  /** What ssh takes, or an `ssh://` URL. */
  target: string;
  /** Config roots on the machine. */
  roots: string[];
  /** The remote devsandbox as shell text; `null` is `devsandbox` on its `PATH`. */
  command: string | null;
  /** This host's ssh-agent is relayed to it (`machines.set`). */
  agent: boolean;
  state: LinkState;
  /** Why it's `unreachable` (or a root it refused). */
  error: string | null;
  /** The machine's devsandbox semver, once reached. */
  version: string | null;
}

/** One process of an instance, as `ps` printed it. */
export interface ProcessRow {
  pid: string;
  ppid: string;
  args: string;
}

/** One tracked run of an instance's helper (`devsbd run ls`). */
export interface RunRow {
  id: string;
  /** `running`, `lost`, or with its code: `exited 4`, `killed 9`. */
  state: string;
  /** RFC 3339. */
  started: string;
  /** The command, space-joined. */
  argv: string;
}

/** A running instance's processes and runs (`instances.procs`). */
export interface ProcsListing {
  processes: ProcessRow[];
  /** The pids are the container's own, so the process can be signalled; false for the runtime's host-side `top` fallback. */
  signalable: boolean;
  /** Oldest first; `[]` without a helper that tracks runs. */
  runs: RunRow[];
  /** The helper's sessions; absent without a helper that has them. */
  sessions?: SessionCount;
}

/** An instance's helper sessions (`devsandbox exec`'s resumable shells and commands). */
export interface SessionCount {
  running: number;
  /** Running with no client attached. */
  detached: number;
}

/** Why an instance has no listing: no such instance, not running, or the listing failed. */
export interface ProcsError {
  error: string;
}

/** One instance's `instances.procs` entry. */
export type InstanceProcs = ProcsListing | ProcsError;

/** `instances.buildLog`'s answer: the end of the instance's build log. */
export interface BuildLogTail {
  text: string;
  /** Earlier lines were left out. */
  cut: boolean;
}

/** `containers.logs`'s answer: the container's last output lines. */
export interface ContainerLogs {
  text: string;
}

/** `sandboxes.get`'s answer: the sandbox's config as TOML text. */
export interface SandboxConfig {
  /** The table as written, `extends` included. */
  original: string;
  /** Merged with what it `extends`, or why it doesn't merge. */
  resolved: string;
  /** The merged table's config hash; empty when it doesn't merge. */
  hash: string;
}

/** `services.get`'s answer: the service's table as TOML text (services don't `extends`). */
export interface ServiceConfig {
  original: string;
}

/** `config.findings`' answer: what in a config root's file breaks under `identity = "host"`. */
export interface ConfigFindings {
  file: string;
  /** A `yaml` file isn't rewritten: `config.migrate` answers `unsupported`. */
  format: 'toml' | 'yaml';
  /** What `config.migrate` takes back, to refuse a file changed meanwhile (`conflict`). */
  fileHash: string;
  findings: IdentityFinding[];
}

/** One identity finding of a config. */
export interface IdentityFinding {
  /** Stable while its table, key and value are unchanged. */
  id: string;
  /** Where it's written: `template.<name>` | `sandbox.<name>`. */
  table: string;
  kind: 'root-target' | 'root-env' | 'root-path' | 'root-user';
  key: string;
  value: string;
  /** The update as a line for a person; `null`: fixed by hand. */
  edit: string | null;
  /** The sandboxes it applies to; empty for a template no sandbox uses. */
  sandboxes: string[];
  /** The user chose to continue as is (`config.ack`). */
  acked: boolean;
}

/** `config.migrate`'s answer. */
export interface ConfigMigrated {
  /** Findings fixed; 0 wrote nothing. */
  edits: number;
  /** This root's instances whose config hash the rewrite changed (they need a rebuild). */
  drifted: string[];
}

/** `config.ack`'s answer: every acked finding id of the root, sorted. */
export interface ConfigAcked {
  acked: string[];
}

/** What `forwards.rm()` stopped. */
export interface ForwardRemoved {
  ok: true;
  local: string;
  /** A configured forward stays stopped until its owner runs again. */
  configured: boolean;
}

/** `inbox.changed` params: the store changed, re-fetch. */
export interface InboxChanged {
  /** Grows per change this daemon saw; compare within one connection only. */
  generation: number;
}

/** `inbox.shown` params: a container message worth a status line. */
export interface InboxShown {
  instance: string;
  /** The dashboard's status-line text. */
  line: string;
}

/** `forwards.status` params: a one-line forward status. */
export interface ForwardsStatus {
  line: string;
}

/**
 * What every verb (`run`, `rebuild`, `start`, `stop`, `rm`, `services.rebuild`) takes: the config
 * root and the env the verb runs with (the caller's whole env, never the
 * daemon's).
 */
export interface VerbParams {
  /** Absolute config root (the CLI's `-C`); with `machine`, a path on that machine. */
  root: string;
  env: Record<string, string>;
  /**
   * Run it on that machine (a fresh ssh); answered as it answers there. A
   * hang-up cancels it there unless `detach`, and so does this daemon exiting.
   */
  machine?: string;
}

/** A terminal's size in cells. */
export interface TtySize {
  cols: number;
  rows: number;
}

/** `run` params: the CLI's flags. */
export interface RunParams extends VerbParams {
  sandbox: string;
  name?: string | null;
  branch?: string | null;
  base?: string | null;
  /** The run record arrives on `stdout` (and in a detached answer's `record`). */
  json?: boolean;
  /** Probe the runtime's user-namespace mode again instead of using the cached result (`--reprobe`). */
  reprobe?: boolean;
  /** Answer once the instance is registered. */
  detach?: boolean;
  /**
   * Run the verb's commands on a pty of this size (each side 1 to 1000), so
   * output is coloured and `docker build` shows its TTY progress, as on a
   * terminal; `stdout` then carries both streams, with `\r\n` line ends and
   * terminal escapes. The build log records the size. Absent: pipes, plain output.
   */
  tty?: TtySize | null;
}

/** `rebuild` params: `instance` or `all: true`. */
export interface RebuildParams extends VerbParams {
  instance?: string | null;
  all?: boolean;
  force?: boolean;
  /** As {@link RunParams.reprobe}. */
  reprobe?: boolean;
  detach?: boolean;
  /** As {@link RunParams.tty}. */
  tty?: TtySize | null;
}

/** `start` params: `instance` or `all: true`. */
export interface StartParams extends VerbParams {
  instance?: string | null;
  all?: boolean;
  detach?: boolean;
  /** As {@link RunParams.tty}. */
  tty?: TtySize | null;
}

/** `stop` params: `instance` or `all: true`. */
export interface StopParams extends VerbParams {
  instance?: string | null;
  all?: boolean;
  /** A hang-up before the answer leaves it running instead of cancelling it; the answer still comes at the exit. */
  detach?: boolean;
}

/** `rm` params. */
export interface RmParams extends VerbParams {
  instance: string;
  /** `true` deletes the branch, `false` keeps it; absent: merged or gone branches go, local work stays. */
  deleteBranch?: boolean | null;
  force?: boolean;
  /** Where the branch question would come up, answer `confirm` (the question as its `message`) having removed nothing; resend with `deleteBranch`. */
  ask?: boolean;
  /** A hang-up before the answer leaves it running instead of cancelling it; the answer still comes at the exit. */
  detach?: boolean;
}

/** `services.rebuild` params: the service's name in the config root. */
export interface ServicesRebuildParams extends VerbParams {
  name: string;
  /** A hang-up before the answer leaves it running instead of cancelling it; the answer still comes at the exit. */
  detach?: boolean;
}

/** A verb's answer once it exited 0. */
export interface VerbResult {
  /** The instance it acted on; `null` for `all` or when unknown. */
  instance: string | null;
  exit: 0;
}

/** A `detach` verb's early answer; the verb runs on. */
export interface VerbDetached {
  instance: string | null;
  detached: true;
  /** `run` with `json`: the in-flight instance's record. */
  record?: RunRecord;
}

/** `data` of a `failed` error: the verb exited non-zero. */
export interface VerbFailedData {
  exit: number;
  instance: string | null;
}

/** `output` params: a chunk of a verb's stdout or stderr. */
export interface VerbOutput {
  /** The verb request's id. */
  request: number;
  stream: 'stdout' | 'stderr';
  /** UTF-8 (lossy), as read. */
  data: string;
}

/** `progress` params: a verb's instance changed phase. */
export interface VerbProgress {
  request: number;
  instance: string;
  /** `preparing`, `building`, `creating`, a lifecycle step, or `failed`. */
  phase: string;
}

/** `closing` params: the daemon is exiting; connect again (which starts a new one). */
export interface Closing {
  reason: 'handoff' | 'idle' | 'shutdown';
}

/** Any daemon notification, known or not (the `notification` event). */
export interface ApiNotification {
  method: string;
  params: unknown;
}

/** Why a connection closed (the `close` event). */
export interface ApiClose {
  /** The relay's exit code: 0 after `close()`, 75 when the daemon hung up. */
  code: number | null;
  signal: NodeJS.Signals | null;
}

/** Events an {@link Api} emits, by name. Notifications need `subscribe()` first. */
export interface ApiEvents {
  'inbox.changed': InboxChanged;
  'inbox.shown': InboxShown;
  'instances.changed': {};
  /** A root this connection watches (`instances.watch`); needs the `instances` topic. */
  'instances.snapshot': InstancesSnapshot;
  'forwards.changed': {};
  'forwards.status': ForwardsStatus;
  /** A verb request's output; sent to its connection, no `subscribe` needed. */
  output: VerbOutput;
  /** A verb request's phase change; sent to its connection, no `subscribe` needed. */
  progress: VerbProgress;
  closing: Closing;
  /** Every notification, including ones this package doesn't know yet. */
  notification: ApiNotification;
  /** The relay exited; pending calls were rejected with code `closed`. */
  close: ApiClose;
}

/** Params and result of each typed method, for {@link Api}'s `call`. */
export interface ApiMethods {
  'inbox.threads.list': { params: { view?: InboxView }; result: ThreadSummary[] };
  'inbox.thread.get': { params: ThreadAddress; result: ThreadDetail };
  'inbox.thread.markRead': { params: ThreadAddress; result: OkResult };
  'inbox.thread.act': { params: ThreadAddress & { action: string }; result: OkResult };
  'inbox.thread.reply': { params: ThreadAddress & { text: string }; result: OkResult };
  'inbox.thread.done': { params: ThreadAddress; result: OkResult };
  'inbox.thread.reopen': { params: ThreadAddress; result: OkResult };
  /** Merge `answers` (any subset) into an open form's draft; no event. */
  'inbox.form.saveDraft': { params: FormParams; result: OkResult };
  /** Submit an open form: `answers` over the draft over the defaults; the owner gets one `submit` event. */
  'inbox.form.submit': { params: FormParams; result: OkResult };
  'inbox.notify.dismiss': { params: NotifyTarget; result: OkResult };
  'inbox.notify.markRead': { params: NotifyTarget; result: OkResult };
  /**
   * `depth`: `disk` (state + config, statuses `unknown`), `listing` (+ statuses, drift), `full` (+ cpu/mem; the default).
   * `machine`: ask that machine (its `dir`), answered as it answers; `not-found` for a machine that isn't registered.
   */
  'instances.list': { params: { dir: string; depth?: SnapshotDepth; machine?: string }; result: InstancesList };
  /**
   * Push `instances.snapshot` for config root `dir` (absolute) while connected; subscribe to
   * `instances` first (else `invalid`). The first one comes at once (cached, or from disk).
   * `machines`: every push also carries the machines' rows and `machines`.
   */
  'instances.watch': { params: { dir: string; machines?: boolean }; result: OkResult };
  /** Stop `instances.watch`ing `dir`; not watching it is fine. */
  'instances.unwatch': { params: { dir: string }; result: OkResult };
  /**
   * `not-found` when nothing matches. `machines`: this host and every machine (5 s each); a
   * `not-found` then names the machines that didn't answer.
   * `machine` (here and on every instance, container and config method below): ask that machine,
   * answered as it answers; `not-found` for a machine that isn't registered.
   */
  'instances.resolve': { params: { name: string; machines?: boolean; machine?: string }; result: InstancesResolved };
  /** SIGINT an in-flight `run`/`rebuild` (it rolls back or records failed); `invalid` when not in flight. */
  'instances.cancel': { params: { instance: string; machine?: string }; result: InstanceCancelled };
  /** Per named instance (exact names), its processes and runs or an `error`; a per-instance failure never fails the call. */
  'instances.procs': { params: { instances: string[]; machine?: string }; result: Record<string, InstanceProcs> };
  /**
   * `instance`: exact name or instance id; `lines` default 500, at most 10000. `null`: no build log yet.
   * A pty log is replayed: plain lines, or with `ansi` lines carrying SGR escapes only.
   */
  'instances.buildLog': { params: { instance: string; lines?: number; ansi?: boolean; machine?: string }; result: BuildLogTail | null };
  /** Signal a process in the instance's container (exact name; `pid` from `instances.procs`, digits). */
  'instances.signal': { params: { instance: string; pid: string; signal: 'TERM' | 'KILL'; machine?: string }; result: OkResult };
  /** Exact name; `invalid` for a bad or taken name, or an instance mid-build. */
  'instances.rename': { params: { instance: string; newName: string; machine?: string }; result: InstanceRenamed };
  /** Set or clear the done flag (exact name); idempotent. */
  'instances.done': { params: { instance: string; done: boolean; machine?: string }; result: InstanceDone };
  /** `devsandbox-…` containers only; `lines` default 50, at most 10000. */
  'containers.logs': { params: { container: string; lines?: number; machine?: string }; result: ContainerLogs };
  /** `devsandbox-…` containers only: the runtime's inspect JSON (docker's: an array of one object). */
  'containers.inspect': { params: { container: string; machine?: string }; result: unknown };
  /**
   * The config root's sandboxes, sorted; `[]` without a config file. `machines`: then every
   * machine's registered roots' (5 s; one that doesn't answer adds nothing, see `machines.list`).
   */
  'sandboxes.list': { params: { root: string; machines?: boolean; machine?: string }; result: SandboxEntry[] };
  /**
   * This host's config roots: every one its instances were made from, plus `~/.devsandboxes`; sorted.
   * With `paths`: exactly those, in order (`~` expanded here, canonicalized; a bad one has its `error`).
   */
  'roots.list': { params: { paths?: string[] }; result: RootCandidate[] };
  /**
   * Dial a target over ssh and list its candidate roots; `failed` when ssh or the hello fails. `command`: the
   * machine's devsandbox as shell text (default `devsandbox`); `invalid` when empty.
   */
  'machines.probe': { params: { target: string; command?: string }; result: MachineProbe };
  /**
   * Check `roots` on the machine (a quoted `~` is its home) and register it; `invalid` for a taken name, a bad
   * root or an empty `command`. `command` as `machines.probe`'s, saved as the machine's.
   */
  'machines.add': { params: { target: string; name?: string; roots: string[]; agent?: boolean; command?: string }; result: MachineAdded };
  'machines.list': { params: {}; result: MachineRow[] };
  /** Change a registered machine's settings; `not-found` for an unknown name. */
  'machines.set': { params: { name: string; agent: boolean }; result: OkResult };
  'machines.rm': { params: { name: string }; result: OkResult };
  /** One registered machine, to build an ssh command to it; `not-found` for an unknown name. */
  'machines.get': { params: { name: string }; result: MachineInfo };
  /**
   * Hold a machine's link (and, with `agent` on, its agent relay) up until this connection closes; `agent` is the
   * caller's absolute `SSH_AUTH_SOCK`. Waits up to 5 s for the relay; `not-found` for an unknown name.
   */
  'machines.hold': { params: { name: string; agent?: string }; result: MachineHeld };
  /** `not-found` for an unknown sandbox; one that doesn't merge answers why in `resolved`. */
  'sandboxes.get': { params: { root: string; name: string; machine?: string }; result: SandboxConfig };
  'services.get': { params: { root: string; name: string; machine?: string }; result: ServiceConfig };
  /** `not-found` for no config file or an unknown `sandbox`; with `sandbox`, only the findings applying to it. */
  'config.findings': { params: { root: string; sandbox?: string; machine?: string }; result: ConfigFindings };
  /**
   * Rewrite the config file: `ids` (absent: every finding with an edit), and "continue as is" for the
   * `root-user` findings in `keep`. `conflict` when the file's hash isn't `fileHash`; `unsupported` for YAML
   * (the message lists the edits).
   */
  'config.migrate': { params: { root: string; ids?: string[]; keep?: string[]; fileHash: string; machine?: string }; result: ConfigMigrated };
  /** Continue as is: ack `ids`, dropping acked ids no longer among the root's findings. */
  'config.ack': { params: { root: string; ids: string[]; machine?: string }; result: ConfigAcked };
  'forwards.list': { params: { dir?: string; machines?: boolean }; result: ForwardRow[] };
  'forwards.add': { params: ForwardAddParams; result: ForwardAdded };
  'forwards.rm': { params: { id: number }; result: ForwardRemoved };
  /** Verbs hold the connection until they exit (or a `detach` answer); hanging up cancels. */
  run: { params: RunParams; result: VerbResult | VerbDetached };
  rebuild: { params: RebuildParams; result: VerbResult | VerbDetached };
  start: { params: StartParams; result: VerbResult | VerbDetached };
  stop: { params: StopParams; result: VerbResult };
  rm: { params: RmParams; result: VerbResult };
  /** No instance verb: `instance` is always `null`; `detach` only keeps it running past a hang-up, the answer comes at its exit. */
  'services.rebuild': { params: ServicesRebuildParams; result: VerbResult };
  subscribe: { params: { topics: ApiTopic[] }; result: OkResult };
  unsubscribe: { params: { topics: ApiTopic[] }; result: OkResult };
  /** Stop the daemon (it drains, then exits); `devsandbox serve install` sends it. */
  shutdown: { params: {}; result: OkResult };
}

/** `inbox.*` methods; mutations resolve once the store is written. */
export interface ApiInbox {
  /** Threads in `view` (default `all`), last change first. */
  list: (view?: InboxView) => Promise<ThreadSummary[]>;
  get: (thread: ThreadAddress) => Promise<ThreadDetail>;
  markRead: (thread: ThreadAddress) => Promise<void>;
  /** Press an owner's action (its event; host-only actions are refused). */
  act: (thread: ThreadAddress, action: string) => Promise<void>;
  /** Send a reply; trimmed, cut at 2000 chars. */
  reply: (thread: ThreadAddress, text: string) => Promise<void>;
  done: (thread: ThreadAddress) => Promise<void>;
  reopen: (thread: ThreadAddress) => Promise<void>;
  /** Remove one notification thread, or every one. */
  dismiss: (target: NotifyTarget) => Promise<void>;
  /** Mark one notification thread read, or every one. */
  markNotifyRead: (target: NotifyTarget) => Promise<void>;
}

/** `instances.*` methods. */
export interface ApiInstances {
  /** The {@link status} snapshot of config root `dir` (resolved against `process.cwd()`), full depth. */
  list: (dir: string) => Promise<InstancesList>;
}

/** `forwards.*` methods. Ad-hoc forwards outlive the connection until `rm`. */
export interface ApiForwards {
  /** Forwards of config root `dir`, or of every root. */
  list: (dir?: string) => Promise<ForwardRow[]>;
  /** Start an ad-hoc forward; resolves once its listener is bound. */
  add: (params: ForwardAddParams) => Promise<ForwardAdded>;
  rm: (id: number) => Promise<ForwardRemoved>;
}

/** A connection to the daemon (`connect()`), an EventEmitter of {@link ApiEvents}. */
export interface Api {
  /** The daemon's `hello` answer. */
  readonly daemon: HelloResult;
  /** The relay has exited; calls reject with code `closed`. */
  readonly closed: boolean;
  /** Any method; typed for {@link ApiMethods}, `unknown` for the rest. */
  call: <M extends string>(
    method: M,
    params?: M extends keyof ApiMethods ? ApiMethods[M]['params'] : unknown,
  ) => Promise<M extends keyof ApiMethods ? ApiMethods[M]['result'] : unknown>;
  /** Start notifications for `topics`; subscribe first, then fetch. */
  subscribe: (topics: readonly ApiTopic[]) => Promise<void>;
  unsubscribe: (topics: readonly ApiTopic[]) => Promise<void>;
  on: <E extends keyof ApiEvents>(event: E, listener: (params: ApiEvents[E]) => void) => Api;
  once: <E extends keyof ApiEvents>(event: E, listener: (params: ApiEvents[E]) => void) => Api;
  off: <E extends keyof ApiEvents>(event: E, listener: (params: ApiEvents[E]) => void) => Api;
  /** Hang up: closes the relay's stdin, resolves once it exited. */
  close: () => Promise<void>;
  inbox: ApiInbox;
  instances: ApiInstances;
  forwards: ApiForwards;
}

export interface ConnectOptions {
  /** Sent as `npm:<name>` in the hello and passed to the relay, for `serve.log`. Default `node`. */
  name?: string;
  env?: NodeJS.ProcessEnv;
  /** `inherit` passes the relay's diagnostics to this process's stderr. Default `pipe` (kept for error messages). */
  stderr?: 'pipe' | 'inherit';
}

/**
 * Connect to the host daemon through `devsandbox api --stdio`, starting the
 * daemon if needed, and say hello. Rejects with {@link DevsandboxApiError}:
 * `unsupported` on Windows (the daemon is unix only), `protocol` when the
 * daemon speaks another protocol, `closed` when the relay exits first.
 */
export declare function connect(opts?: ConnectOptions): Promise<Api>;

/**
 * Rejection of an API call. `code` is the daemon's (`not-found`, `invalid`,
 * `denied`, `closed-form`, `conflict`, `unsupported`, `bind-failed`, `failed`, `unknown-method`, `internal`, …) or the client's
 * own: `closed` (the connection ended), `protocol`, `unsupported`.
 */
export declare class DevsandboxApiError extends Error {
  readonly name: 'DevsandboxApiError';
  readonly code: string;
  /** The method called; `hello` for a failed connect. */
  readonly method: string;
  /** The error body's `data`, when its code carries some ({@link VerbFailedData} for `failed`). */
  readonly data?: unknown;
}
