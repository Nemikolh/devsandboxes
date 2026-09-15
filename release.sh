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

git add Cargo.toml Cargo.lock
git commit -m "release v$new"
git tag "v$new"
git push origin main "v$new"

echo "released v$new"
