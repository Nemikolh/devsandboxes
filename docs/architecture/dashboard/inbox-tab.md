# The Inbox tab

The dashboard's view of the shared Inbox store: the thread list, the
thread pane, replies, forms and thread actions. UI only; the store, its
ops and wire views live in `src/inbox/`. Read this before changing how the
dashboard shows or mutates threads.

Code: `src/tui/app/inbox.rs` (`InboxView`, `InboxFocus`, `View` display
bits, `set_inbox`, `request_inbox`, `sync_inbox_selection`, `set_tab`,
`on_key_inbox_pane`, `pane_header`, `pane_feed`, `chip`),
`src/tui/app/forms.rs` (`FormEdit`, `pinned_forms`, `form_lines`),
`src/tui/app/thread_actions.rs` (`Deferred`, `starts_target`,
`thread_target`, `run_thread_action`, `run_after_start`,
`start_answered`), `src/tui/markdown.rs` (`render`, `inline_spans`,
`wrap`; input passed through `inbox::sanitize`), `src/tui/textarea.rs` (`TextArea`),
`src/tui/mod.rs` (store load/apply/reload, `open_link`),
`src/tui/daemon.rs` (`op_call`, `DaemonUpdate::InboxChanged`/`Shown`),
`src/inbox/view.rs` (`View`), `src/inbox/clock.rs`.

## Data flow

Reads are always local: the event loop loads `inbox.json`
(`inbox::ops::load`) at start and whenever its stamp (mtime/size) moves or
the daemon says `inbox.changed`; any dashboard, CLI or the daemon's notify
sink may have written it. `App::set_inbox` installs it, keeping the cursor
on its thread.

Writes are `Op`s. `request_inbox` applies an op to `App`'s copy at once
(except ops that enqueue an owner event, whose ids and times are minted
under the store lock) and queues it. The event loop sends queued ops
through the daemon worker as API calls (`op_call`: `inbox.thread.*`,
`inbox.notify.*`, `inbox.form.*`) while connected, else applies them to
the store itself (`inbox::ops::apply`, actor `tui`); the worker does the
same for ops it holds when the connection drops. Both paths run the same
ops code. A reload follows.

`inbox.shown` pushes (a container message's status line from the
daemon's notify sink) become the status line. Desktop popups are the
daemon's, not the dashboard's.

## List and views

One row per thread, newest change first, filtered by `View` (`NeedsYou`,
`Active`, `Done`, `All`; membership from `src/inbox/view.rs`, shared with
the API), stepped with `←`/`→` and clamped at the ends. The tab title
carries the needs-you count. `D` clears notify threads; `d` dismisses a
notify thread or marks a dispatcher thread done; `u` reopens. The
list/pane divider drags.

## Read semantics

A thread is read when it becomes the selected one, and again when it
changes while selected. The selected thread stays listed until the cursor
leaves it (`InboxView::shown`) so reading it doesn't slide the next row
under the cursor. Leaving the Inbox from a view that shows notify records
marks every notify read (`Op::MarkNotifyRead`), since a notify record has
no state to resolve it.

## Thread pane

Always the selected thread: a pinned header (`pane_header`: owner, state,
child and its status), the thread's open forms pinned under it, then the
feed newest first (`pane_feed`). Focus zones (`InboxFocus`): `List`,
`Thread`, `Form`, `Input`; all but `List` shadow the dashboard keys like a
modal (quit, `:` and `?` stay reachable). `m` toggles raw text.

Message text renders as markdown (`markdown::render`: headings, lists,
quotes, code, tables; `inline_spans` for one-line titles and cards),
sanitized first. Wrapping is the module's own, so the pane knows its exact
row count for scrolling and each row's `RowMeta` lets a selection copy a
wrapped paragraph as one line ([selection.md](selection.md)). Times use
`inbox::clock` with the UTC offset taken once at start.

## Replies and forms

`r`/`i` opens the reply box (`TextArea`: wraps by character, grows to 6
rows) when the thread takes replies; `enter` sends `Op::Reply`,
`alt`/`shift-enter` inserts a newline. A thread's open forms are pinned;
in the `Form` zone keys pick options or edit a text answer, each finished
change an `Op::SaveDraft` applied locally at once, and a second `enter`
after the summary submits (`Op::Submit`). Archived threads (owner removed)
refuse every user op.

## Thread actions

In the thread pane `1`-`9` run the thread's buttons (in the list they
stay tab keys); `o`/`t`/`l`/`p` are fixed keys in both zones. A
button's dispatcher half is an `Op::Act` (the owner's event); a host verb
reuses the dashboard's own plumbing aimed at the thread's target:
`PromptAction::Code`, a terminal tab, the logs modal, a forward or the
`port` prompt, `pending_open`, `PromptAction::Rm`. The target is the
thread's `child` (resolved through the owner's `thread_children` entry
from the snapshot) or else the owner; nothing a container writes can aim
a host action elsewhere. VS Code, terminal and forward on a stopped
target start it first and run when a snapshot shows it up
(`after_start`); a failed start request drops the waiting action. Marking
a dispatcher thread done (`d`) or reopening it (`u`) also sets or clears
its child's done flag (`instances.done`), and so does a button with
`done: true` (`send_act` → `mark_thread_child`); a thread without a
`child`, or one whose child doesn't resolve, leaves every flag alone (a
dispatcher is never marked done for its thread). This is the dashboard's
doing: `inbox.thread.act`/`done`/`reopen` over the API or the CLI change
the thread only. Links open through `xdg-open`/`open` only
when they are URLs.

Limitations: [status.md](../../status.md#dashboard)
