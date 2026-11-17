# 0054: Interactive execs are helper sessions, ended only on purpose

Status: accepted (2026-11-17).

## Context

A tty `devsandbox exec` was `<runtime> exec -it`. When its client went
(ssh dropped, a terminal or dashboard tab closed), the runtime kept the
process running with nobody able to reach it again: docker and podman
can't re-attach to an exec, and nothing sends the process a `SIGHUP`. A
long agent session started in such a shell was lost to a short outage.

## Decision

- **Every tty exec into an instance with the helper is a session**
  (`commands::exec::session_for`): the helper's supervisor
  (`devsbd/src/sessions.rs`) owns the pty and the command, and any later
  client attaches to it (`exec --attach`, the dashboard's `a` and session
  rows, the reconnect loops). Piped execs, instances without the helper
  and service terminals stay plain execs.
- **One client at a time; a new attach takes over** (`DETACHED_EXIT` to
  the old one). The client docker leaves behind after its terminal went
  stays connected and can't tell (its stdin gets no EOF and no signal), so
  `attached` means "a client is connected", not "someone is watching", and
  attaching without an id takes an attached session too when it is the
  only candidate (`commands::exec::choose_session`).
- **The pty is always drained**: output with no client is dropped, never
  buffered without bound and never left to block the program. The
  supervisor starts the command only once its creator is attached
  (bounded), so the creator sees the first output.
- **A session ends when its program exits, when it is ended explicitly
  (`exec --end`, `sessions.rm`, `K` on its dashboard row, `devsbd session
  rm`) or with its container.** Nothing ends one for being idle: the long
  sessions are the ones worth keeping (an agent working while nobody
  watches, a shell left overnight), and idle can't be told from working
  unwatched without guessing at output.
- Where devsandbox owns the transport it reconnects by itself: `exec m/x`
  and the dashboard's remote tabs re-dial and attach on a lost link
  (`serve::machines::cli::Reconnect`). A local client has no link to lose
  and doesn't.

## Consequences

- A dropped connection, a closed terminal or a quit dashboard never loses
  the program; getting back is `--attach` or `a`.
- Abandoned sessions hold their memory and CPU until someone ends them or
  the container stops; the dashboard's count and rows and `exec
  --sessions` are what make them visible. An opt-in timeout can come if
  it's missed.
- A reattach shows only what comes next (the program redraws on the
  resize it gets); there is no replay of what was printed while detached.
- Attaching without an id can take over a client that was live after all;
  that client gets `detached` and can attach back.
