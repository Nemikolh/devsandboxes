# 0001: Files are the truth; the daemon is replaceable

Status: accepted

## Context

devsandbox has a per-user host daemon (`devsandbox serve`) that runs verbs,
bridges, port forwards, the API and machine links. A daemon that held
state of its own would make every crash, upgrade handoff or `kill` a data
loss event, and would make the CLI useless whenever it isn't running.

## Decision

- The container runtime is the source of truth for liveness. Host-side
  state lives in files next to `state.toml` in the data dir: `state.toml`
  (instances), `inbox.json` (the Inbox), `forwards.toml` (ad-hoc forwards),
  `machines.toml` (machines), `dashboard.toml` (dashboard settings).
- The daemon holds only what can't live in a file: connections, running
  forwards, subscribers, in-flight verbs. Stopping it loses nothing; its
  successor rebuilds from the files and the runtime.
- Every command works without it: verbs fall back to running in-process
  (`serve::verbs_client` `in_process`), `devsandbox inbox` reads the store
  directly and runs mutations locally through the same `inbox::wire`
  `decide_*` checks, and the daemon starts lazily (`serve::client::connect`).
- Everything devsandbox creates is named `devsandbox-…`, so it shows up in
  plain `docker ps`.

## Consequences

- New daemon features must persist anything that has to survive a restart
  in a file under a lock, not in daemon memory.
- Two writers (daemon and CLI fallback) touch the same files, so every
  store needs locked read-modify-write and atomic rename.
- Rejected: a daemon-owned database or in-memory registry as the single
  source of truth.

Source: docs/high-level-architecture.md, "Principles"; docs/inbox-redesign.md, "Principles" (6).
