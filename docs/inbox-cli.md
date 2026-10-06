# `devsandbox inbox`: the Inbox from a shell

Read and answer the Inbox's threads (owner threads from `devsbd thread put`,
notifications from `devsbd notify`) without the dashboard. Design:
`docs/inbox-redesign.md`, "CLI". Code: `src/commands/inbox.rs`, with the
API's views and checks in `src/inbox/wire.rs`.

```
devsandbox inbox ls [--view needs-you|active|done|all] [--json]
devsandbox inbox show <thread> [--json]
devsandbox inbox reply <thread> <text|->            # - reads stdin
devsandbox inbox act <thread> <action-id>
devsandbox inbox submit <thread> <message-id> [--json '<answers>']   # stdin without --json
devsandbox inbox done|reopen <thread>
```

## Naming a thread

`<thread>` is either `<owner>/<key>` or the numeric id `ls` shows (the only
handle a notification has: it has no key).

`<owner>` is an instance name (or its id), resolved like every instance
argument (`ps -a` names). A removed instance's threads are archived and
read-only, but still found by the name it had when it last wrote.

## Reads

`ls` and `show` read the store (`inbox.json` beside `state.toml`) directly:
they work without the daemon and never start it.

- `ls`: id, state (`needs you` / `active` / `done`, a notification's level,
  `(archived)` once the owner is gone), owner, key, title, age; last change
  first. `--view` picks the dashboard's view (default `all`).
- `show`: the header (title, state · status, owner/key and id, child, link,
  actions with their ids, how to reply), then the feed newest first:
  messages as their markdown source, fields as `label: value`, forms with
  each question, its options and its answer / draft / default, then
  replies, actions, submissions and markers one line each.

`--json` prints the API's views (docs/api.md) in the `{"schema":1,"data":…}`
envelope: `ls` a list of `ThreadSummary`, `show` one `ThreadDetail`.

## Mutations

`reply`, `act`, `submit`, `done` and `reopen` go to a daemon that's already
running (never lazy-started), as the API methods `inbox.thread.reply` /
`act` / `done` / `reopen` and `inbox.form.submit`, so subscribers and the
owner's `--follow` wake at once. With no daemon answering (or off unix,
where there's none) they apply the same ops to the store directly through
the same checks, so the owner's events and the errors are identical either
way; the owner sees them on its next `devsbd events`.

Success prints one line on stderr and nothing on stdout. A refusal exits
non-zero with the API's message and code, e.g. `no thread 5 (not-found)`,
`thread 3 takes no replies (denied)`, `required questions unanswered: note
(invalid)`.

- `reply -` reads all of stdin (one trailing newline dropped). An empty
  reply is an error.
- `act` takes the action's id from `show`. Dashboard-only actions (host
  verbs like VS Code or logs) are refused.
- `submit` answers are a JSON object of question id to answer: an option id
  (a list of ids for a multiple choice), a string, or a bool. Any subset:
  the saved draft and the defaults fill in the rest, and missing required
  answers are an error naming them. Without `--json` the object is read
  from stdin; blank input submits no answers of its own.

```bash
devsandbox inbox ls --view needs-you
devsandbox inbox show babysit/pr-6900
devsandbox inbox submit babysit/pr-6900 run-1791277117 --json '{"post": "yes", "note": "Batched in flush()"}'
git log -1 --format=%B | devsandbox inbox reply babysit/pr-6900 -
```
