/// <reference types="node" />

// Mirrors the serde output of the Rust CLI's `--json` verbs (src/snapshot.rs,
// src/runtime/mod.rs). Rust `Option<T>` serializes as `T | null`.

/** JSON envelope version this package understands. */
export declare const SCHEMA: 1;

// ---------------------------------------------------------------------------
// Payloads

/** Container liveness; `text` is the runtime's raw status (e.g. "Up 3 minutes"). */
export type ContainerStatus =
  | { state: 'running'; text: string }
  | { state: 'exited'; text: string }
  | { state: 'missing' };

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

// The host daemon's JSON-lines API (docs/api.md), over `devsandbox api
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
  /** Every question's answer; `null` for an optional one left unanswered. */
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
  /** Once submitted, every question's answer (`null`: optional, unanswered); `null` before. */
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
  /** The canonical config root. */
  dir: string;
  /** The bound host address, e.g. `127.0.0.1:3000`. */
  local: string;
  /** Route label, e.g. `api:3000`; empty until the route first resolves. */
  target: string;
  /** The listening process (`node (pid 412)`). */
  process: string | null;
  /** `active`, `connecting`, or `error: <reason>`. */
  state: string;
  /** Open connections. */
  conns: number;
  /** From a sandbox's `forwardPorts`, not `forwards.add()`. */
  configured: boolean;
}

/** `forwards.add()` params; `instance`, `service`, or both. */
export interface ForwardAddParams {
  /** Config root (the CLI's `-C`); resolved against `process.cwd()`. */
  dir: string;
  /** Instance name, as the CLI takes it (never prompts). */
  instance?: string;
  /** Forward one of its services instead. */
  service?: string;
  /** Host address to bind. Default `127.0.0.1`. */
  address?: string;
  /** `port` (that host port or the next free one up) or `host:port` (exactly). */
  spec: string;
}

/** A forward `forwards.add()` bound. */
export interface ForwardAdded {
  id: number;
  local: string;
  /** Route label, best effort this early. */
  target: string;
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
  'forwards.changed': {};
  'forwards.status': ForwardsStatus;
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
  'instances.list': { params: { dir: string }; result: Snapshot };
  'forwards.list': { params: { dir?: string }; result: ForwardRow[] };
  'forwards.add': { params: ForwardAddParams; result: ForwardAdded };
  'forwards.rm': { params: { id: number }; result: ForwardRemoved };
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
  /** The {@link status} snapshot of config root `dir` (resolved against `process.cwd()`). */
  list: (dir: string) => Promise<Snapshot>;
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
 * `denied`, `closed-form`, `bind-failed`, `unknown-method`, `internal`, …) or the client's
 * own: `closed` (the connection ended), `protocol`, `unsupported`.
 */
export declare class DevsandboxApiError extends Error {
  readonly name: 'DevsandboxApiError';
  readonly code: string;
  /** The method called; `hello` for a failed connect. */
  readonly method: string;
}
