#!/usr/bin/env bash
set -euo pipefail

bump="${1:-patch}"

if [ -n "$(git status --porcelain)" ]; then
  echo "error: working tree is dirty, commit or stash first" >&2
  exit 1
fi

branch=$(git rev-parse --abbrev-ref HEAD)
if [ "$branch" != "main" ]; then
  echo "error: releases are cut from main (currently on $branch)" >&2
  exit 1
fi

git pull --ff-only origin main

# The Unreleased section becomes the GitHub release body, so it has to be
# written (see .agents/skills/release) before anything gets bumped.
if ! scripts/changelog-section.sh Unreleased >/dev/null; then
  echo "error: write the release notes under \"## Unreleased\" in CHANGELOG.md first" >&2
  exit 1
fi

current=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
IFS=. read -r major minor patch <<<"$current"
case "$bump" in
  major) new="$((major + 1)).0.0" ;;
  minor) new="$major.$((minor + 1)).0" ;;
  patch) new="$major.$minor.$((patch + 1))" ;;
  *)
    echo "usage: $0 [patch|minor|major]" >&2
    exit 1
    ;;
esac

sed -i "0,/^version = \"$current\"/s//version = \"$new\"/" Cargo.toml
# refresh Cargo.lock with the new version
cargo check --quiet

# npm packages ship the same binary, so they share the crate version: each
# manifest's own version plus the root's exact pins on the platform packages.
npm_manifests=(npm/devsandboxes/package.json npm/platforms/*/package.json)
sed -i -E \
  -e "s/^(  \"version\": )\"$current\"/\1\"$new\"/" \
  -e "s/^(    \"@devsandboxes\/[^\"]+\": )\"$current\"/\1\"$new\"/" \
  "${npm_manifests[@]}"
if grep -nE "\"(version|@devsandboxes/[^\"]+)\": \"" "${npm_manifests[@]}" | grep -v "\"$new\""; then
  echo "error: npm manifests above were not bumped to $new" >&2
  exit 1
fi

# README's curl install example pins a release archive URL.
current_re=${current//./\\.}
sed -i -E \
  -e "s#/download/v$current_re/#/download/v$new/#g" \
  -e "s#devsandbox-$current_re-#devsandbox-$new-#g" \
  README.md
if grep -noE "(/download/v|devsandbox-)[0-9]+\.[0-9]+\.[0-9]+" README.md \
  | grep -vE "(v|-)${new//./\\.}$"; then
  echo "error: README download links above were not bumped to $new" >&2
  exit 1
fi

sed -i "0,/^## Unreleased$/s//## $new/" CHANGELOG.md
if ! grep -qx "## $new" CHANGELOG.md; then
  echo "error: CHANGELOG.md was not updated with $new" >&2
  exit 1
fi

git add Cargo.toml Cargo.lock README.md CHANGELOG.md "${npm_manifests[@]}"
git commit -m "release v$new"
git tag "v$new"
git push origin main "v$new"

echo "released v$new"
