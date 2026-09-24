#!/bin/sh
# Build devsbd for both Linux arches into target/devsbd/{x86_64,aarch64}/devsbd,
# where build.rs picks them up for embedding. Rerun when devsbd/ changes.
# Prereq: rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
set -eu
cd "$(dirname "$0")/.."
for arch in x86_64 aarch64; do
    target="$arch-unknown-linux-musl"
    cargo build -p devsbd --profile devsbd --target "$target"
    mkdir -p "target/devsbd/$arch"
    cp "target/$target/devsbd/devsbd" "target/devsbd/$arch/devsbd"
    printf '%s: %s bytes\n' "$arch" "$(wc -c < "target/devsbd/$arch/devsbd" | tr -d ' ')"
done
