# Terminal prompts

Interactive CLI questions and reviews that read well on a TTY: color and
weight to say what matters, long lists folded to a short preview that
expands in place, and answers picked from a list instead of typed `y/N`.
Off a TTY (and with `NO_COLOR`, `TERM=dumb`) everything stays as today:
plain text, same `--yes` / refusal rules.

## Library choice

`crossterm` is already a dependency (the dashboard's backend) and is the
low-level layer every prompt crate builds on: raw mode, key events,
cursor moves, SGR styles. The widgets needed here are small (a select
list, a fold tree, a masked line), so they're hand-rolled on it: no new
dependency (the release profile is size-tuned), the look is ours, and
the state machines stay pure and unit-tested like `src/tui/app/`.

Considered: `inquire` (look), `cliclack` (left gutter bar, not
removable), `dialoguer` + `console` (themeable via its `Theme` trait,
closest fit, but no foldable tree and a second terminal crate),
`promkit` (has tree widgets, heavier, its own render loop), ratatui's
`Viewport::Inline` (fixed height, fights scrollback when content grows).

## Shape

- `src/ask/` (new, unix + windows: crossterm covers both):
  - `style.rs`: stderr-gated styling (`stderr` a TTY, `NO_COLOR` unset,
    `TERM` not `dumb`); semantic helpers (`heading`, `name`, `danger`,
    `warn`, `note`, `ok`, `dim`, `added`, `removed`). `src/render.rs`
    stays stdout-gated for tables.
  - `select.rs`: pure `Select` state (choices with a label, an optional
    hint and a hotkey; cursor; default) + `keys` (↑/↓, j/k, hotkey,
    Enter, Esc/Ctrl-C = cancel) + `lines()` render.
  - `fold.rs`: pure `Fold` tree (nodes with a label, a style, children;
    collapsed nodes show the first `PREVIEW` children and `… N more`);
    →/Space expand, ← collapse.
  - `review.rs`: a `Fold` above a `Select` in one widget: Tab or ↑ past
    the first choice moves focus into the tree, so the user can open
    `never` before answering.
  - `mod.rs`: the I/O loop: raw mode on the controlling terminal, cursor
    hidden, redraw the widget's lines in place (clamped to the terminal
    height, scrolling the tree), a guard restoring the terminal on drop
    and panic; on answer, the widget collapses to one line
    (`? Approve this claim for `gh`? › Approve`). Ctrl-C/Esc returns
    `Cancelled`, which callers turn into their existing refusal error.
  - Plain fallback in the same entry points: when the terminal can't be
    put in raw mode, today's `[y/N]` / numbered menu.

## Step 1: `src/ask` widgets

Constraints: no I/O in `style`'s pure fns (take the enabled flag),
`select`/`fold`/`review` are I/O-free state machines with unit tests
(keys → state, state → lines, both with color off so tests compare
text). The loop writes to stderr only. No caller changes yet.
Landmarks: `src/render.rs` (existing SGR helpers to mirror),
`src/tui/app/mod.rs` (I/O-free state machine pattern),
`src/commands/login.rs` `EchoOff` (termios guard pattern).
Docs: new `docs/architecture/cli-prompts.md` (module, gating, fallback),
linked from `docs/README.md`.

## Step 2: `login` and `access approve`

- `credentials::review::View::fold()` and `Diff::fold()` build the tree,
  replacing `summary()` / `render()`: the plain path prints the tree
  unfolded (scopes `brief`: their count, not their routes). Kind and credential name bold, upstreams plain, each scope a
  node (routes as children, count as hint), `never` in the danger style,
  folded with a preview; a diff's added lines green `+`, removed red `-`.
- `login::Term` gains `review(fold, question, choices) -> Result<usize>`
  and `choose(question, choices) -> Result<usize>`; `Stdio` runs the
  widgets, the tests' `Script` answers from its script. `confirm` goes.
  `approve_or_refuse`, the broad confirm and `access`'s
  `confirm_revoke` / approve use them with `Approve` / `Refuse`
  (`Revoke` / `Keep` …), default the safe answer. `--yes` and off-TTY
  refusal unchanged.
- The rest of `term.show`'s lines styled by kind: `warning:` warn,
  `note:` note, the create-token page URL bold, the final
  `logged in …` line ok. Secret prompt styled, still unechoed.

Landmarks: `src/commands/login.rs` (`Term`, `Stdio`, `approve_or_refuse`,
`login_at`'s `term.show` calls, test `Script`), `src/commands/access.rs`
(`confirm_revoke`, `approve`, the summary/diff shown before it),
`src/credentials/review.rs` (`View::summary`, `Diff::render`).
Docs: `docs/reference/credentials-cli.md` (what a TTY shows, keys),
`docs/architecture/credentials/login.md` if it describes the prompt.

## Step 3 (proposed, not asked): pick the login method

On a TTY with no `--method` and more than one usable method, `choose()`
lists them (`device`, `browser`, `paste`, each with its hint), the one
`choose` picks today preselected. Off a TTY: unchanged.
Landmarks: `login.rs` `choose`, `obtain`.
Docs: `docs/reference/credentials-cli.md`, `docs/architecture/credentials/login.md`.

## Step 4: the shared prompts

`commands::confirm` and `commands::pick` (`src/commands/mod.rs`) run the
`select` widget, so every caller gets it: `rm`'s branch question,
`services` gc's history delete, `machine`'s key fingerprint, replace and
link confirms (fingerprint shown styled), instance ambiguity
(`commands::resolve_instance`, `serve/verbs_client.rs`,
`serve/machines/cli.rs`), `exec`'s session pick.
`serve/verbs_client/identity.rs`'s `[u/c/Skip]` prompt becomes a
three-choice select. `machine.rs`'s free-text `prompt()` only gets
styling. Callers' signatures unchanged.
Docs: `docs/architecture/cli-prompts.md` (callers).

## Not in scope

Coloring report output (`ls`, `ps`, `inbox ls`, `status`): `render.rs`
already covers tables; parked in `docs/status.md` if wanted.
