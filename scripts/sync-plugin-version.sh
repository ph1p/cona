#!/bin/sh
# Sync the crate version from Cargo.toml into the plugin/marketplace manifests.
# Run by release-plz.yml after `release-plz update`; safe to run by hand.
# Prints the bare version on stdout (the workflow reads it — the ONE place the
# Cargo.toml version is extracted); diagnostics go to stderr.
# tests/basic/plugin.rs (plugin_versions_match_the_crate) pins the result in CI.
set -eu
cd "$(dirname "$0")/.."
ver="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"
[ -n "$ver" ] || { echo "error: could not read version from Cargo.toml" >&2; exit 1; }
for f in plugin/.claude-plugin/plugin.json \
         plugin/.codex-plugin/plugin.json \
         .claude-plugin/marketplace.json; do
    tmp="$f.tmp"
    sed 's/"version": "[^"]*"/"version": "'"$ver"'"/' "$f" > "$tmp"
    mv "$tmp" "$f"
    # sed succeeds even when it matched nothing (reformatted JSON, renamed
    # key) — and the release-plz bump commit fires no CI, so this assertion
    # is the only check that runs on the exact commit that ships
    grep -F "\"version\": \"$ver\"" "$f" >/dev/null \
        || { echo "error: $f has no \"version\" field after sync" >&2; exit 1; }
done
echo "plugin manifests -> v$ver" >&2
echo "$ver"
