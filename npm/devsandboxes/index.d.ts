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
}

/** One `[sandbox.*]` from config.toml (`ls()`, `status().sandboxes`). */
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

/** One `[services.*]` from config.toml (`service.ls()`, `status().services`). */
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
  /** Config root containing config.toml (the CLI's `-C`). Defaults to `cwd`. */
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

/** Sandbox configs in config.toml. */
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
/** Create and start an instance of `sandbox`; resolves with the instance name. */
export declare function run(sandbox: string, opts?: RunOptions): Promise<string>;
export declare function start(target: Target, opts?: CommonOptions): Promise<void>;
export declare function stop(target: Target, opts?: CommonOptions): Promise<void>;
/** Recreate from current config when drifted (`force`: even without drift). */
export declare function rebuild(target: Target, opts?: CommonOptions & { force?: boolean }): Promise<void>;
export declare function rename(name: string, newName: string, opts?: CommonOptions): Promise<void>;
/** Remove container, worktree, and state entry. */
export declare function rm(name: string, opts?: CommonOptions): Promise<void>;
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

export declare const service: {
  ls(opts?: CommonOptions): Promise<ServiceRow[]>;
  /** Recreate a service's containers and rewire running sandboxes in place. */
  rebuild(name: string, opts?: CommonOptions): Promise<void>;
};
