# CLI prompts

How the CLI asks a question on a terminal: a list of answers picked with
the arrows or a hotkey, optionally under a tree of details that folds
open in place, or a checklist of answers to check any number of. Read this before adding a prompt or changing how one
looks.

Code: `src/ask/mod.rs` (`select`, `review`, `multi`, `Answer`, `Key`,
`Frame`, `Widget`, `Raw`, `interactive`, `visible`, `window`, `summary`,
`checked_labels`, `plain_select`, `parse_plain`, `plain_multi`,
`parse_plain_multi`), `src/ask/style.rs` (`enabled`, `Style`, `Role`),
`src/ask/select.rs` (`Select`, `Choice`, `Outcome`),
`src/ask/multi.rs` (`Multi`, `Outcome`),
`src/ask/fold.rs` (`Fold`, `Node`, `PREVIEW`), `src/ask/review.rs`
(`Review`).

## Shape

The widgets (`Select`, `Multi`, `Fold`, `Review`) are I/O-free state machines:
`key(Key)` changes the state, `lines(Style, …)` / `frame(Style)` render
it as strings. `Key` is the module's own key type, so tests drive them
without crossterm; `key_of` maps crossterm's `KeyEvent` to it. All
output goes to stderr, so a command's stdout stays clean for pipes.

`select(question, choices, default)` and `review(tree, question,
choices, default)` are the entry points. Both return
`Answer::Picked(index)` or `Answer::Cancelled`; callers turn a cancel
into their own refusal. `multi(question, choices, checked)` returns the
checked indexes, ascending (possibly none), or `None` on a cancel.

The `Widget` trait is what `interactive` runs: `key` returns `None`
while the question is open, else the widget's own `Outcome` type
(`select::Outcome` for `Select` and `Review`, `multi::Outcome` for
`Multi`).

## Callers

`login` and `access` ask through their `Term` seam
(`src/commands/login.rs`: `Term::choose`, `Term::review`; `Stdio` runs
these entry points, the tests' `Script` answers by choice label and
records a review's tree as `Fold::plain_lines` prints it):

- `login::approved` (behind `approve_or_refuse`, and `access::ask_approve`
  for `run`'s question): Approve / Refuse, Refuse preselected, under the
  claim version's tree (`credentials::review::View::fold`, or
  `Diff::fold` against the latest approved version); with no tree (the
  broad-credential question) a plain select.
- `access::confirm_revoke`: Revoke (or Drop, for a version) / Keep, Keep
  preselected.
- `login::pick_method`: one choice per method `offered` (`device`,
  `browser`, `paste`, each with a hint and its first letter as hotkey),
  the method `choose` picks preselected; asked only when there are
  several.

All treat a cancel as the safe answer (for the method pick: no login). docs/reference/credentials-cli.md
has what the user sees.

The other commands ask through three helpers in `src/commands/mod.rs`:

- `confirm(prompt)`: Yes (`y`) / No (`n`), No preselected, a cancel is
  No; `false` without asking when stdin isn't a terminal. `rm`'s branch
  question (`rm::question`, also asked by the verb client's confirm
  round, `serve/verbs_client.rs` `run`) and `services::gc`'s `delete
  shell history …?`.
- `pick(prompt, options)`: one choice per option, the first
  preselected, a cancel is a `cancelled` error. The instance ambiguity
  (`commands::resolve_instance`, the verb client's `Ui::pick`,
  `serve/machines/cli.rs` `resolve_and_route`), `run`'s sandbox pick and
  `exec`'s session pick (`exec::choose_session`, also from
  `serve/machines/cli.rs` `session_exec`).
- `choose(question, &[(label, hotkey)], default)`: `None` on a cancel;
  the caller gates on a TTY. `machine.rs`: the key fingerprint
  (`confirm_key`: the fingerprint line in bold above it, Trust / Refuse),
  the install (`ensure_devsandbox`: Copy / Don't add it, or Replace /
  Keep) and `upgrade` (Replace / Keep), each with the second answer
  preselected and a cancel taken as it.

`serve/verbs_client/identity.rs` `prompt` asks `QUESTION` with
`choices` (Update `u`, Continue as is `c`, Skip this time `s`; Skip
preselected and a cancel's answer) through `Ui`'s `ask`, which is
`ask::select` outside the tests; the listing above it has each file in
bold (`listing`, styled by `Ui`'s `style`).

`machine add`'s allowance question (`commands::machine::add`, its
`AllowStep::Ask`) is the one `ask::multi`: one choice per
`AllowInfo::flags` allowance (`allowance_choices`, labels in the CLI's
spelling, each with a hint), the ones the machine lends checked; the
checked set becomes the add's `allow` (`allow_patch`), and a cancel stops
the add.

## Styling

`style::enabled` gates color on stderr: it is a TTY, `NO_COLOR` is
unset and `TERM` isn't `dumb`. `src/render.rs` is the stdout
counterpart for tables. Renders take a `Style { on }` decided once
(`Style::detect`), so tests render with `Style::plain()` and compare
text. Text is styled by `Role`: `Heading` and `Name` bold, `Danger`
bold red, `Warn` yellow, `Note` cyan, `Ok` and `Added` green, `Removed`
red, `Dim`, and `Accent` (cyan) for the cursor and the picked answer.

## Widgets and keys

`Select` shows `? question` (bold) and one line per `Choice`: the label,
its hotkey as ` [a]` and its hint, both dim. The cursor's line starts
with an accent `❯` and a bold label; the others are indented to match.
There is no left gutter bar.

- ↑/↓ and `k`/`j` move; the cursor clamps at both ends rather than
  wrapping, so a held arrow never lands on the opposite answer.
- A choice's hotkey (case-insensitive) picks it at once; a hotkey `k` or
  `j` wins over navigation.
- Enter picks the cursor's choice; Esc and Ctrl-C cancel. Raw mode turns
  Ctrl-C into a key event, not SIGINT.

`Fold` is a tree of `Node`s, each with a label, a `Role`, an optional
hint (shown dim in parentheses) and children. A collapsed parent shows
`▸ label` then its first `PREVIEW` (2) children dimmed and `… N more`
when there are more; an open one shows `▾ label` and every child, two
spaces deeper per level. Leaves have no glyph. The navigable nodes are
the roots and the children of open nodes; preview lines are not.

- ↑/↓ and `k`/`j` move; → opens; Space and Enter toggle; ← closes an
  open node, else moves to its parent.

`Multi` shows `? question` and one line per `Choice`, as `Select`
does, with a mark between the cursor gutter and the label: an accent
`◉` for a checked choice, `◯` otherwise. A dim key-help line
follows. `checked` sets the first marks; the cursor starts on the
first choice.

- ↑/↓ and `k`/`j` move, clamped, as in `Select`.
- Space toggles the cursor's choice; a choice's hotkey
  (case-insensitive) toggles that choice, and a hotkey `k` or `j` wins
  over navigation.
- Enter answers with the checked set, empty included; Esc and Ctrl-C
  cancel.

`Review` stacks a `Fold` above a `Select` with a dim key-help line
below. Focus starts on the choices, at the caller's default (the safe
answer). Tab switches focus; ↑ on the first choice moves into the tree
at its last node, ↓ past the tree's last node moves to the first
choice. Only the focused part shows its cursor. With the tree focused,
hotkeys and Enter act on the tree, so nothing meant for it answers the
question; Esc and Ctrl-C cancel from either side.

## Drawing in place

`interactive` runs a widget when stdin and stderr are terminals and
`TERM` isn't `dumb`. `Raw` puts the terminal in raw mode, hides the
cursor and turns autowrap off (a long line is clipped, so the number of
lines drawn is the number of rows used); its `Drop` restores all three,
on `?` errors and panics too. Each key redraws the frame: back to the
first drawn line, clear to the end of the screen, then the lines joined
with `\r\n` and no trailing newline.

A `Frame` is a scrolling `body` (the tree, or a select's whole list)
and a `foot` (the question and choices) that always shows. When the
frame is taller than the terminal, `visible` keeps the foot and a
window of the body; `window` scrolls just enough to keep the focused
line in view and leaves the window where it was while focus is in the
foot. A terminal reporting 0 rows (an unsized pty) is drawn unclamped.

Once answered, the widget is erased and plain lines are left in
scrollback: for `review`, each root of the tree collapsed to one line
(`Fold::summary_lines`); then `? question › answer` (question bold,
answer accent), or `? question › cancelled` dimmed. For `multi` the
answer is the checked labels joined with `, `, or `none`.

## Plain fallback

Off a terminal (stdin or stderr not a TTY, `TERM=dumb`) or when raw
mode can't be enabled, the same entry points print the tree with no
cursor and every node open (`Fold::plain_lines`: nothing can be
expanded there, so a fold would hide lines), except a `brief` node,
which shows its head alone (a claim's scopes: the route count, not the
routes), the question, the choices numbered `1) …`, and
`[1-N, default D] > `. One line is read from stdin: empty picks the
default, a number or a hotkey picks that choice, end of input cancels,
anything else is an error.

`multi` falls back the same way: the question, the choices numbered
with `[x]` / `[ ]` for `checked`, and `[numbers or names,
comma-separated; empty keeps the checked] > `. Empty keeps `checked`;
else the line, split on commas and whitespace, is exactly the set
answered, each item a 1-based number or a label (case-insensitive).
An invalid item prints an error line and the prompt asks again, so a
typo doesn't end the command; end of input cancels.
