---
name: release
description: Cut a devsandbox release - bring the CHANGELOG.md Unreleased section up to date with readable notes, then run release.sh, which tags the release and makes CI publish it with that section as the GitHub release body.
metadata:
  internal: true
---

# Release

The GitHub release body comes straight from `CHANGELOG.md`. `release.sh` renames `## Unreleased` to `## <new version>`, and the Release workflow publishes that section through `scripts/changelog-section.sh`. The notes have to be right **before** the script runs, because it commits, tags and pushes in one go.

## 1. Preconditions

- On `main`, working tree clean, up to date with `origin/main`.
- `cargo test --workspace` passes.
- The user asked for a release. `release.sh` pushes a tag, which publishes to GitHub, crates.io and npm, and none of that can be undone.

## 2. Bring `## Unreleased` up to date

List what's unreleased:

```bash
last=$(git describe --tags --abbrev=0)
git log --format='%h %s%n%b' "$last..HEAD"
```

Compare that with the current `## Unreleased` section (`scripts/changelog-section.sh Unreleased`). If there is none, add it directly under `# Changelog`; `release.sh` leaves none behind. Every user-visible change must be covered. Read the commit bodies, and read the diff or docs when a subject is vague. Then rewrite the section in the house style (copy the shape of the existing sections):

1. One or two sentences summarizing the release, as a user would describe it.
2. `### Added` / `### Changed` / `### Fixed` (and `### Internal` if worth mentioning), each a bullet list. Lead each bullet with the feature in **bold**, then say what it does for the user and why, not how it's implemented. Skip empty headings.
3. A short fenced example (`toml` config snippet or `bash` invocation) after each notable feature. Take real syntax from `skills/config-toml-spec/SKILL.md`, `README.md` or `src/main.rs`, and never invent flags or keys.
4. Plan docs, agent docs, test flakes and refactors stay out of the prose unless they matter to users; they still appear in the commit list.
5. End with the raw commit list, excluding `release vX` commits:

   ```markdown
   <details><summary>Commits</summary>

   - abc1234 feat(run): ...

   </details>
   ```

   Generate it with `git log --format='- %h %s' "$last..HEAD"`, and remember that the commit carrying the changelog itself will also be in the release.

Never use a `## ` heading inside the section (it would end the section); use `###` and deeper.

## 3. Pick the bump

`patch` for fixes and small additions, `minor` for new commands, config keys or behavior changes, `major` for breaking config/CLI changes (pre-1.0 too, if it would break existing `devsandboxes.toml` files). Ask the user if it's unclear.

## 4. Commit and release

```bash
git add CHANGELOG.md
git commit -m "docs(changelog): notes for the next release"
git push origin main
./release.sh <patch|minor|major>
```

`release.sh` refuses to run with a dirty tree or an empty `## Unreleased`. It bumps every version pin, renames the section, commits `release vX.Y.Z`, tags and pushes.

## 5. Verify

```bash
gh run watch "$(gh run list --workflow Release --limit 1 --json databaseId -q '.[0].databaseId')"
gh release view vX.Y.Z
```

The release body must match the changelog section. If the workflow fails, report it to the user; don't re-tag or force-push without their approval.
