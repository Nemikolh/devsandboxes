# `devsandbox inbox`

Read and answer Inbox threads (owner threads from `devsbd thread put`,
notifications from `devsbd notify`) from a shell. Read when scripting the
Inbox or changing the CLI; the model behind it is in
[architecture/inbox](../architecture/inbox/model.md).

Code: `src/main.rs` (`InboxCommand`), `src/commands/inbox.rs` (`ThreadArg`,
`locate`, `ls`, `show`, `render_detail`, `Mutation`, `mutate`,
`apply_local`, `via_daemon`, `reply_text`, `submit_answers`),
`src/inbox/wire.rs` (`summaries`, `ThreadDetail`, `resolve`, `decide_*`),
`src/inbox/view.rs`, `src/inbox/clock.rs`.

```
devsandbox inbox ls [--view needs-you|active|done|all] [--json]
devsandbox inbox show <thread> [--json]
devsandbox inbox reply <thread> <text|->
devsandbox inbox act <thread> <action>
devsandbox inbox submit <thread> <message> [--json <ANSWERS>]
devsandbox inbox done <thread>
devsandbox inbox reopen <thread>
```

## `<thread>`

`ThreadArg::parse`: all digits → the store id `ls` shows (the only handle a
notification has: it has no key); else `<owner>/<key>`, split at the first
`/`, both non-empty. `<owner>` is an instance name or id, resolved like any
instance argument (`resolve_instance_noninteractive`) to its `instance_id`;
when that finds no thread (no such instance, or a removed one whose threads
are archived), `wire::resolve` matches `owner` as an instance id, then as
the last-known `owner_name` (a live thread wins, then the most recently
changed). Not found: `no thread <id> (not-found)` /
``no thread `<key>` from `<owner>` (not-found)``.

## `ls`

Threads in `--view` (default `all`; names from `View::as_str`), last change
first (`wire::summaries`). Views: `needs-you` (live threads in state
`needs-you` and unread notifications), `active`, `done` (live threads only),
`all` (everything, archived and read notifications included).

Table columns: `ID`, `STATE` (`needs you` / `active` / `done`, a
notification's level, `-`; suffixed ` (archived)` once the owner is gone),
`OWNER` (owner name), `KEY` (`-` for notifications), `TITLE` (cut to 50
chars with `…`), `AGE` (`clock::short_age` of `changed_at`, local time).

## `show`

Header: title; state chip (` · <status>` when set); `<owner>/<key>  (thread
<id>)`; `child:`, `link:` when set; `actions:` as `[<id>] <label>`, tagged
`(sets done)` and/or `(dashboard only)` (a host verb that sends no event);
`reply:` with the compose hint (or placeholder) and the `inbox reply`
command, when the thread takes replies. Then a notification's notes, then
the feed newest first:

- message: `<age>  <owner>  message <id>` (`(edited)`, `(withdrawn)`: a
  withdrawn message shows no body), then its blocks indented: markdown
  source, fields as `label: value`, forms as `form <id>: <title|submit>
  (<state>)` with each question (`(required)`; a follow-up ends `· only if
  <question> = <option>` or `· under <question>`; options as `one of:` /
  `any of: <id> = <label>, …`; confirm as `true | false` or its labels),
  its `answer:` / `draft:` / `default:`, and for an open form the `inbox
  submit` command;
- reply: `<age>  you` over the text;
- action: `<age>  you · <label> [<action>]`;
- submission: `<age>  you · submitted form <form> of message <message>`
  over `<question id>: <value>` per answer (`-` for `null`, lists
  comma-joined);
- marker: `<age>  · marked done` / `reopened` / `<state|status> <from> →
  <to>`.

## `--json`

`ls` and `show` print the API's views in the envelope
`{"schema": 1, "data": …}` (`commands::status::Envelope`): `ls` a list of
[thread summaries](api.md#thread-summary), `show` one detail as
[`inbox.thread.get`](api.md#inboxthreadget) answers it.

## Mutations

| command | API method | check |
|---|---|---|
| `reply <thread> <text>` | `inbox.thread.reply` | `decide_reply` |
| `act <thread> <action>` | `inbox.thread.act` | `decide_act` |
| `submit <thread> <message>` | `inbox.form.submit` | `decide_submit` |
| `done <thread>` | `inbox.thread.done` | `decide_done` |
| `reopen <thread>` | `inbox.thread.reopen` | `decide_reopen` |

All need a live owner thread: a notification is `invalid`, an archived
thread `denied`.

- `reply`: `-` reads all of stdin, one trailing newline (`\n` or `\r\n`)
  dropped. Blank text is `invalid` (`empty reply`); a thread without
  `compose` is `denied` (`takes no replies`). Text past `MAX_REPLY` (2000
  chars) is cut when applied.
- `act`: the action id from `show`. Unknown id → `not-found`; a host-verb
  action that enqueues no event → `invalid` (the API doesn't run host
  verbs).
- `submit`: answers are a JSON object of question id → answer: an option id
  (single choice), a list of option ids (multiple), a string (text), a bool
  (confirm). From `--json`, else all of stdin; blank input is no answers.
  Any subset: the saved draft and the defaults fill in the rest. Errors:
  no such message or no form on it → `not-found`; unknown question or wrong
  type → `invalid`; missing required answers → `invalid` (`required
  questions unanswered: <ids>`); form already submitted or withdrawn →
  `closed-form`. Details: [forms](../architecture/inbox/forms.md).
- `done` / `reopen` (and an `act` on a `done: true` action) change the
  thread only; the thread's child instance keeps its done flag (the
  dashboard sets that itself, `devsandbox done` does it by hand).

Success prints one line on stderr (`replied to thread <id>`, ``sent action
`<a>` to thread <id>``, ``submitted the form of message `<m>` on thread
<id>``, `marked thread <id> done`, `reopened thread <id>`), nothing on
stdout. A refusal exits 1 with `Error: <message> (<code>)`, the same from
either path below.

## How it runs

- Reads (`ls`, `show`, and `locate` for every command) load the store
  (`inbox.json` beside `state.toml`) directly; they never start the daemon.
- Mutations go to a daemon already running (`client::connect_running`, no
  lazy start), so subscribers and owners' `events --follow` wake at once.
  With none answering (or off unix, where `via_daemon` doesn't exist) they
  run the same `decide_*` under the store lock (`ops::apply_if`) with client
  tag `cli`, so events and errors are identical. Once a request reached a
  daemon, a lost connection is an error, never a local retry.
- `-C` doesn't matter: the store is host-wide. `--machine` is refused
  (``--machine isn't for `devsandbox inbox` ``); a `<thread>` is always
  this host's.

Limitations: [status.md](../status.md#inbox)
