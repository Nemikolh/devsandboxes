---
name: simplify-comments
description: Check the comments in a change and delete the ones that only say what the code does, keeping the ones that say why. Use before committing any change that adds or edits comments, when reviewing a diff, or when the user runs /simplify-comments [paths].
---

# Simplify comments

A comment earns its place by telling the reader something the code can't:
why it is this way, or a contract the name and type don't carry. Everything
else restates the code, goes stale, and buries the comments that matter.

Only comments change. Never edit code, strings or markup in this pass, and
don't reword surviving comments for style.

## Steps

1. Scope. By default, the comments your change adds or edits:
   `git diff -U0 HEAD` (plus staged), lines starting with `+` that are
   comments. With paths (`/simplify-comments src/tui`), every comment under
   them.
2. Read each comment with the code right after it (and, in tests, the
   assertion it sits on). Decide with the rules below. When unsure, keep it.
3. Delete or trim. A trim keeps the original words of the why part and must
   still read as a sentence: capital first letter, final period, no dangling
   pronoun. If nothing but a restatement would be left, delete it.
4. Run the checks that cover the files (`cargo test --workspace`; for `site/`,
   `cd site && pnpm check`), then commit with the change, or on its own as
   `chore(comments): …` for a path sweep.

## Delete

- Narration of the next line or block: `// Remove the old container` above
  `remove_force(&container)`, `// Lock the screen to render it`.
- Test captions the assertion already says: `// Nothing went to the shell.`
  above `assert!(written.is_empty())`, step labels like `// onto inst0`,
  `// Port 0.`, `// drift: false`.
- Doc comments that only restate the name, type or signature:
  `/// A listener failed.` on `ListenerFailed`, `/// The API call for
  [`Cmd::Done`].` on `fn done_call`, `/// Service command, if set.` on
  `command: Option<String>`.
- Section banners and dividers (`// ---- wire ----`, `/* ---- Drill */`). If
  a banner carries real prose, keep the prose and drop the dashes.
- Change history, including the quiet kind: `used to`, `previously`,
  `as before`, `as ever`, `as today`, `the old parser`, `step-2 rewire`. The
  commit message holds history; a comment says what holds now.

## Keep

- Why: platform, runtime or vendor quirks, protocol rules, races and
  orderings, trade-offs, why something is NOT done.
- Contracts the name and type don't give: units, what `None`/empty means,
  errors, side effects, blocking, who calls it, invariants. One-word-looking
  docs can still be contracts: `/// The socket dir.` on a `dir: PathBuf`
  that could be the data dir, `/// Open connections.` (current, not a total).
- Fixture explanations in tests: why an argument is `Some("main")`, which
  container in a list is which, why a repeated call is there
  (`// And once more, with everything already gone.`), magic coordinates.
- The scenario or bug a test guards, when the test name doesn't say it.
- `SAFETY:` comments, `docs/…` citations, RFC/spec/vendor links, the reason
  after `#[allow(...)] //`, module docs (`//!`).

## Never touch

- `///` on clap derive items in `src/main.rs` (`Parser`, `Subcommand`,
  `Args`, `ValueEnum`): they are the CLI's `--help` text.
- Doc comments in `npm/devsandboxes/index.d.ts`: the site renders them as the
  API reference.
- Text inside string literals and fixtures that looks like a comment (e.g.
  the banners in `site/src/lib/dts.test.ts`'s sample).
