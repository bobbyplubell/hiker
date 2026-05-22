#!/usr/bin/env bash
#
# Self-contained release build for GitHub Pages. Lives inside the `site`
# crate so it travels with it — run it from anywhere; it cd's to its own dir.
# Output goes to ./dist (next to this script).
#
# GitHub Pages serves a project repo from a SUBPATH
# (https://<user>.github.io/<repo>/), so we build with a RELATIVE public URL
# ("./"): the generated index.html references its assets relatively and works
# at any subpath without knowing the repo name. Override if you need an
# absolute base:  PUBLIC_URL=/my-repo/ ./build.sh
#
set -euo pipefail

cd "$(cd "$(dirname "$0")" && pwd)"
PUBLIC_URL="${PUBLIC_URL:-./}"

echo ">> building release wasm (public-url: $PUBLIC_URL)"
trunk build --release --public-url "$PUBLIC_URL" --dist ./dist index.html

# Shrink the wasm with binaryen's wasm-opt. Trunk's built-in pass is disabled
# (see index.html) because trunk doesn't let us pass the feature flags that
# the bulk-memory ops modern rustc emits require.
WASM="$(ls ./dist/*_bg.wasm)"
echo ">> wasm-opt -Oz $(du -h "$WASM" | cut -f1) ..."
wasm-opt -Oz \
    --enable-bulk-memory \
    --enable-mutable-globals \
    --enable-nontrapping-float-to-int \
    --enable-sign-ext \
    --enable-reference-types \
    --enable-multivalue \
    "$WASM" -o "$WASM"

# GitHub Pages runs files through Jekyll, which silently drops anything
# starting with "_". This empty marker turns Jekyll off so every asset is
# served verbatim.
touch ./dist/.nojekyll

echo
echo ">> done. Output in $(pwd)/dist :"
ls -lh ./dist
echo
echo "Publish ./dist to GitHub Pages (e.g. push it to a 'gh-pages' branch)."
