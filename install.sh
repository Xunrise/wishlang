#!/bin/sh
# wishlang installer for macOS and Linux.
#
#   curl -fsSL https://raw.githubusercontent.com/Xunrise/wishlang/master/install.sh | sh
#   curl -fsSL .../install.sh | sh -s -- --system          # /usr/local/bin
#   curl -fsSL .../install.sh | sh -s -- --version 0.2.0
#
# Environment:
#   WISHLANG_INSTALL_DIR  install directory (default ~/.local/bin)
#   WISHLANG_VERSION      version to install (default: latest release)
#   WISHLANG_BASE_URL     where release assets live (default: GitHub releases); for mirrors and tests
#
# Re-running upgrades in place.
set -eu

REPO="Xunrise/wishlang"
system=0
version="${WISHLANG_VERSION:-}"

while [ $# -gt 0 ]; do
  case "$1" in
    --system) system=1 ;;
    --version) shift; version="${1:?--version needs a value}" ;;
    --version=*) version="${1#*=}" ;;
    -h|--help) sed -n '2,13p' "$0" 2>/dev/null | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "install.sh: unknown option $1" >&2; exit 2 ;;
  esac
  shift
done

say() { printf '%s\n' "$*"; }
die() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

# --- platform ---------------------------------------------------------------
case "$(uname -s)" in
  Darwin) os=apple-darwin ;;
  Linux) os=unknown-linux-musl ;;
  *) die "unsupported OS $(uname -s); on Windows use install.ps1" ;;
esac
case "$(uname -m)" in
  arm64|aarch64) arch=aarch64 ;;
  x86_64|amd64) arch=x86_64 ;;
  *) die "unsupported architecture $(uname -m)" ;;
esac
# Rosetta: a shell running as x86_64 on an Apple Silicon Mac should still get the native build.
if [ "$os" = apple-darwin ] && [ "$arch" = x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" = 1 ]; then
  arch=aarch64
fi
target="$arch-$os"

# --- downloader -------------------------------------------------------------
if have curl; then fetch() { curl -fsSL "$1" -o "$2"; }
elif have wget; then fetch() { wget -qO "$2" "$1"; }
else die "need curl or wget"; fi

if have sha256sum; then sha256() { sha256sum "$1" | cut -d' ' -f1; }
elif have shasum; then sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
else die "need sha256sum or shasum"; fi

# --- version ----------------------------------------------------------------
if [ -z "$version" ]; then
  have curl || die "set WISHLANG_VERSION (resolving 'latest' needs curl)"
  url=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest") \
    || die "could not resolve the latest release"
  version="${url##*/v}"
  case "$version" in */*|"") die "could not resolve the latest release (got $url)" ;; esac
fi
version="${version#v}"
base="${WISHLANG_BASE_URL:-https://github.com/$REPO/releases/download/v$version}"
name="wishlang-$version-$target"

# --- install dir ------------------------------------------------------------
if [ -n "${WISHLANG_INSTALL_DIR:-}" ]; then dir="$WISHLANG_INSTALL_DIR"
elif [ "$system" = 1 ]; then dir=/usr/local/bin
else dir="$HOME/.local/bin"; fi

# --- download and verify ----------------------------------------------------
tmp=$(mktemp -d) || die "mktemp failed"
trap 'rm -rf "$tmp"' EXIT INT TERM

say "Downloading wishlang $version for $target"
fetch "$base/$name.tar.gz" "$tmp/$name.tar.gz" || die "download failed: $base/$name.tar.gz"
fetch "$base/wishlang-$version-checksums.txt" "$tmp/checksums.txt" || die "download failed: checksums"

want=$(awk -v f="$name.tar.gz" '$2 == f || $2 == "*" f {print $1}' "$tmp/checksums.txt")
[ -n "$want" ] || die "no checksum listed for $name.tar.gz"
got=$(sha256 "$tmp/$name.tar.gz")
[ "$want" = "$got" ] || die "checksum mismatch for $name.tar.gz (expected $want, got $got)"

tar -xzf "$tmp/$name.tar.gz" -C "$tmp"
src="$tmp/$name"
[ -f "$src/wishlang" ] || die "archive has no wishlang binary"

# --- install ----------------------------------------------------------------
run() { "$@"; }
if ! mkdir -p "$dir" 2>/dev/null || [ ! -w "$dir" ]; then
  have sudo || die "$dir is not writable and sudo is missing"
  say "$dir needs elevated permissions"
  run() { sudo "$@"; }
  run mkdir -p "$dir"
fi

old=""
[ -x "$dir/wishlang" ] && old=$("$dir/wishlang" --version 2>/dev/null | awk '{print $NF}' || true)

# Stage next to the target and rename, so a running wishlang is replaced atomically.
run cp "$src/wishlang" "$dir/.wishlang.new.$$"
run chmod 755 "$dir/.wishlang.new.$$"
run mv -f "$dir/.wishlang.new.$$" "$dir/wishlang"

if [ -z "$old" ]; then say "Installed wishlang $version to $dir/wishlang"
elif [ "$old" = "$version" ]; then say "wishlang $version was already installed; reinstalled in $dir/wishlang"
else say "Upgraded wishlang $old -> $version in $dir/wishlang"; fi

# --- man page and completions (when the archive has them) -------------------
if [ "$system" = 0 ] && [ -z "${WISHLANG_INSTALL_DIR:-}" ]; then
  share="${XDG_DATA_HOME:-$HOME/.local/share}"
  if [ -f "$src/man/wishlang.1" ]; then
    mkdir -p "$share/man/man1" && cp "$src/man/wishlang.1" "$share/man/man1/"
  fi
  [ -f "$src/completions/bash" ] && mkdir -p "$share/bash-completion/completions" \
    && cp "$src/completions/bash" "$share/bash-completion/completions/wishlang"
  [ -f "$src/completions/fish" ] && mkdir -p "$HOME/.config/fish/completions" \
    && cp "$src/completions/fish" "$HOME/.config/fish/completions/wishlang.fish"
fi

# --- PATH and the `wish` alias ----------------------------------------------
# `wish` is only linked where it wins over macOS's /usr/bin/wish (Tk): the install
# dir must come before /usr/bin on PATH. An existing foreign `wish` there is left alone.
on_path=0; before_usr_bin=0; seen_usr_bin=0
oldifs=$IFS; IFS=:
for p in $PATH; do
  [ "${p%/}" = "$dir" ] && { on_path=1; [ "$seen_usr_bin" = 0 ] && before_usr_bin=1; }
  [ "${p%/}" = /usr/bin ] && seen_usr_bin=1
done
IFS=$oldifs

if [ "$before_usr_bin" = 1 ]; then
  if [ -L "$dir/wish" ] && [ "$(readlink "$dir/wish")" = wishlang ]; then :
  elif [ -e "$dir/wish" ] || [ -L "$dir/wish" ]; then say "Left existing $dir/wish alone; use wishlang"
  else run ln -s wishlang "$dir/wish" && say "Linked $dir/wish -> wishlang"; fi
else
  say "No wish alias: $dir is not ahead of /usr/bin on PATH (macOS's /usr/bin/wish is Tk). Use wishlang."
fi

if [ "$on_path" = 0 ]; then
  say ""
  say "$dir is not on your PATH. Add it:"
  case "${SHELL:-}" in
    */fish) say "  fish_add_path $dir" ;;
    */zsh) say "  echo 'export PATH=\"$dir:\$PATH\"' >> ~/.zshrc" ;;
    *) say "  echo 'export PATH=\"$dir:\$PATH\"' >> ~/.profile" ;;
  esac
  say "then restart your shell."
fi
say "Run: wishlang --version"
