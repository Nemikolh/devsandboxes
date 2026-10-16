# Inbox forms

A `form` block in an owner's `thread send` message: questions the user
answers and submits once, delivered to the owner as one `submit` event.
Read before changing `src/inbox/form.rs`, the form ops, or a client that
renders forms. Wire JSON: [api.md § Forms](../../reference/api.md#forms).

Code: `src/inbox/form.rs` (`Form`, `Question`, `QuestionKind`,
`ChoiceOption`, `ChoiceDefault`, `Answer`, `FormRecord`, `FormState`,
`check`, `check_under`, `sanitized`, `resend`, `applies`, `check_answer`,
`validate_draft`, `resolve_submission`, `default_of`, `answers_json`,
`partial_json`, `answers_from_json`), `src/inbox/message.rs` (form block
parse), `src/inbox/feed.rs` (`send`, `withdraw`, `cap`, `form_of`),
`src/inbox/mod.rs` (`Op::SaveDraft`, `Op::Submit`, `EventKind::Submit`),
`src/inbox/wire.rs` (`form_target`, `decide_save_draft`, `decide_submit`).

## Schema

`check` runs on the host when a `thread send` arrives (an error becomes an
`error` record, [threads-and-feed](threads-and-feed.md#thread-send)). At
most one form per message. `deny_unknown_fields` throughout.

- Form: `id` (`[a-z0-9-]{1,60}`), `title?` (one line, ≤ 200), `submit`
  (button label, one line ≤ 40, default `Submit`), 1-30 `questions`
  (`MAX_QUESTIONS`) with unique ids.
- Every question: `id`, `label` (one line ≤ 200, inline markdown),
  `context?` (markdown ≤ 4 KiB), `required`, `under?`, `when?`, `type`.

| `type` | fields | `required` default | answer |
|---|---|---|---|
| `choice` | `options` 1-20 (`id`, one-line `label`, `description?`), `multiple`, `default?` (an id, or a list when `multiple`) | true | option id; list of ids when `multiple` |
| `text` | `placeholder?`, `default?` (editable prefill), `multiline`, `max` bytes (1-8192, default 2048) | false | string ≤ `max` |
| `confirm` | `yes?`, `no?` (labels ≤ 40), `default?` bool | true | bool |

A text `default` must fit `max` after sanitizing and be single-line unless
`multiline`; a choice `default` must name known options with the right
arity.

## Follow-ups

`under` makes a question a follow-up of an earlier **top-level** question
(one level only), placed right after it or its other follow-ups. `when`
(only with `under`, parent must be a choice) names a parent option: the
follow-up `applies` only while that option is picked (given, else draft,
else default). A non-applying follow-up is hidden, never required, and
submitted as `null` whatever was given.

## Lifecycle

`FormRecord` lives on the message's feed item, not in the client: `id`,
`state` (`open` → `submitted {at, answers, client}` | `withdrawn`) and the
user's `draft`, so every client sees one draft and it survives restarts.

`resend` (same message id sent again):

| record | new blocks | result |
|---|---|---|
| submitted | anything | frozen: the stored form block (and answers) is kept, other blocks replaced; a re-assert is a no-op |
| open | with a form | form replaced, draft pruned to answers still valid for it |
| open | no form | withdrawn |
| none / withdrawn | with a form | fresh open form, counts as news (`opened`) |

`thread withdraw` withdraws an open form (draft cleared); a submitted one
stays submitted. `feed::cap` never drops a message with an open form.

## Answers

Clients send answers as JSON keyed by question id; `answers_from_json`
types them (wrong JSON type, unknown question → `invalid`). Then:

- `validate_draft` (`inbox.form.saveDraft`, `Op::SaveDraft`): form must be
  open, each answer passes `check_answer` (known options, no repeats, text
  within `max`); text is sanitized. Merged into the draft; no event, no
  feed item.
- `resolve_submission` (`inbox.form.submit`, `Op::Submit`): form must be
  open; each question takes given, else draft, else default. Unanswered
  optional text becomes `""`, unanswered `multiple` `[]`; unanswered
  optional choice/confirm stays absent. Required means answered (text
  non-blank, `multiple` non-empty); missing ones → `invalid` (`required
  questions unanswered: <ids>`).

Errors: no thread/message/form → `not-found`; not open → `closed-form`
(`FormError::Closed`); everything else `invalid`.

A submit sets the record `submitted` (draft cleared), appends a
`submission` feed item (with client) and enqueues one `submit` event with
`message`, `form` and `answers_json`: **every** question, `null` when
unanswered. `thread ls --feed` reports each form's `state` and, once
submitted, the same answers.

Limitations: [status.md](../../status.md#inbox)
