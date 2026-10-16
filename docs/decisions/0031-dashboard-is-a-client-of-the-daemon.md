# 0031: The dashboard is a client of the daemon, with no in-process fallback

Status: accepted

## Context

The TUI dashboard used to load the config, poll the runtime, run bridges
and run verbs itself, suspended in its own process. That duplicated the
daemon's work per open dashboard, made actions die with the dashboard, and
meant the dashboard and the CLI/npm clients could behave differently.

## Decision

- The dashboard reads no config and calls no runtime. Its tree comes from
  `instances.watch` snapshots (0018); config views, logs, inspect, build
  logs, processes, rename/done and signals are API calls; verbs are verb
  requests (`run`/`rebuild`/`start` detached, `stop`/`rm`/`service rebuild`
  held with `detach`, 0017), each on its own connection (`tui::spawn_verb`).
  Terminal tabs run the CLI (0033).
- Local exceptions, kept on purpose: the Inbox store (read directly; ops go
  through the API while connected, else applied locally, 0026),
  `dashboard.toml` settings, prompt history, and VS Code's `state.toml`
  read.
- Daemon down: the last snapshot stays, marked stale with the reason, and
  actions refuse with "…: needs devsandbox serve (…)". Unlike the CLI, the
  dashboard has no in-process fallback.

## Consequences

- One behaviour for every client; anything the dashboard asks the daemon
  for, an API client can ask for too (an Inbox action's host verb is the
  dashboard's own, see 0027).
- The dashboard is useless without the daemon (it starts it lazily, so
  this only bites when the daemon can't run).
- New dashboard features start as API methods.

Source: docs/dashboard-client.md, "Decisions".
