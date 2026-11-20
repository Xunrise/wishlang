#!/usr/bin/env bash
# Package a built release binary: scripts/package.sh <version> <target-triple>
# Writes dist/wishlang-<version>-<target>.tar.gz (.zip on Windows).
set -euo pipefail

version=${1:?version}
target=${2:?target triple}
name="wishlang-$version-$target"
exe=wishlang; case "$target" in *windows*) exe=wishlang.exe;; esac
bin="target/$target/release/$exe"

rm -rf "dist/$name"; mkdir -p "dist/$name"
cp "$bin" LICENSE-MIT LICENSE-APACHE README.md "dist/$name/"

# Man page and completions come from the binary itself. Skipped when the binary
# can't run on this host (cross build) or doesn't have the subcommands yet.
if "$bin" --version >/dev/null 2>&1; then
  if "$bin" man >"dist/$name/wishlang.1" 2>/dev/null; then
    mkdir -p "dist/$name/man" && mv "dist/$name/wishlang.1" "dist/$name/man/"
  else rm -f "dist/$name/wishlang.1"; echo "note: 'wishlang man' not available, skipping"; fi
  for sh in bash zsh fish powershell; do
    mkdir -p "dist/$name/completions"
    if ! "$bin" completions "$sh" >"dist/$name/completions/$sh" 2>/dev/null; then
      rm -f "dist/$name/completions/$sh"; echo "note: 'wishlang completions $sh' not available, skipping"
    fi
  done
  rmdir "dist/$name/completions" 2>/dev/null || true
fi

cd dist
case "$target" in
  *windows*) 7z a -tzip "$name.zip" "$name" >/dev/null ;;
  *) tar -czf "$name.tar.gz" "$name" ;;
esac
rm -rf "$name"
ls -l
