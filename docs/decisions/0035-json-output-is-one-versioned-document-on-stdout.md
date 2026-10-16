# 0035: `--json` output is one versioned document on a clean stdout

Status: accepted

## Context

External UIs and scripts drive devsandbox through `--json`. They need a
stable shape to parse and a stdout that holds nothing but the document,
yet verbs like `run` spawn children (image builds, `docker run -d` printing
an id, lifecycle hooks, git) that write to stdout.

## Decision

- Every `--json` payload devsandbox defines is wrapped as
  `{"schema": 1, "data": …}` (`commands::status::Envelope`, `SCHEMA`),
  bumped only on breaking changes. Two print someone else's document as
  is: `inspect --json` (the runtime's) and `machine ls --json` (the
  daemon's `machines.list` answer). The payloads reuse the same structs the dashboard and API use
  (the snapshot rows), so parity holds by construction.
- For a verb that spawns children (today only `run --json`), the
  process's own stdout is pointed at stderr up front (`json_stdout::JsonStdout::capture`), and the document is
  written to a saved handle on the original stdout. Every inheriting child
  and stray `println!` lands on stderr.

## Consequences

- Callers parse stdout whole and can still show progress from stderr.
- New spawn sites need no flag threading; but `capture` must run before
  any child is spawned.
- Rejected: passing a "stdout to stderr" flag through every spawn site
  (misses the next one added).

Source: docs/json-output.md; `src/json_stdout.rs` module doc.
