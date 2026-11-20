#!/usr/bin/env bash
# Cut a release: scripts/release.sh <version>      e.g. 0.2.0 or 0.2.0-rc.1
#
# Bumps wish/Cargo.toml, zed/Cargo.toml, zed/extension.toml, intellij/gradle.properties,
# refreshes Cargo.lock, dates the CHANGELOG, commits "Release vX" and tags vX.
# Nothing is pushed: it prints the push command. Pushing the tag publishes the release.
set -euo pipefail
cd "$(dirname "$0")/.."

v=${1:?usage: scripts/release.sh <version>}
v=${v#v}
[[ $v =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$ ]] || { echo "not a semver: $v" >&2; exit 1; }

[ -z "$(git status --porcelain --untracked-files=no)" ] || { echo "working tree has uncommitted changes" >&2; exit 1; }
[ "$(git branch --show-current)" = master ] || [ -n "${RELEASE_ANY_BRANCH:-}" ] || { echo "release from master (RELEASE_ANY_BRANCH=1 to override, e.g. for rc tests)" >&2; exit 1; }
git rev-parse "v$v" >/dev/null 2>&1 && { echo "tag v$v exists" >&2; exit 1; }

# First `version = "..."` line in a TOML file (the [package] one in all ours).
bump_toml() { perl -0pi -e 's/^version = "[^"]*"/version = "'"$v"'"/m' "$1"; }
bump_toml wish/Cargo.toml
bump_toml zed/Cargo.toml
bump_toml zed/extension.toml
perl -pi -e 's/^pluginVersion=.*/pluginVersion='"$v"'/' intellij/gradle.properties
cargo update -p wishlang --offline >/dev/null 2>&1 || cargo metadata --format-version 1 >/dev/null
[ -f zed/Cargo.lock ] && (cd zed && cargo update -p wishlang-zed --offline >/dev/null 2>&1 || true)

today=$(date +%F)
V="$v" D="$today" perl -0pi -e 's/## \[Unreleased\]\n/## [Unreleased]\n\n## [$ENV{V}] - $ENV{D}\n/' CHANGELOG.md

git add -u
git commit -qm "Release v$v"
git tag -a "v$v" -m "wishlang $v"
echo "Tagged v$v. Publish with:"
echo "  git push origin master v$v"
