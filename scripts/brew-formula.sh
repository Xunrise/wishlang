#!/usr/bin/env bash
# Print the Homebrew formula for a release: scripts/brew-formula.sh <version> <dist-dir>
# <dist-dir> holds wishlang-<version>-checksums.txt.
set -euo pipefail
version=${1:?version}; dist=${2:?dist dir}
sums="$dist/wishlang-$version-checksums.txt"
sha() { awk -v f="wishlang-$version-$1.tar.gz" '$2 == f {print $1}' "$sums" | grep . || { echo "no checksum for $1" >&2; exit 1; }; }
url() { echo "https://github.com/Xunrise/wishlang/releases/download/v$version/wishlang-$version-$1.tar.gz"; }

cat <<RUBY
class Wishlang < Formula
  desc "Create-from-usage for Rust and Java: you write the call sites, Claude writes the code"
  homepage "https://github.com/Xunrise/wishlang"
  version "$version"
  license any_of: ["MIT", "Apache-2.0"]

  on_macos do
    on_arm do
      url "$(url aarch64-apple-darwin)"
      sha256 "$(sha aarch64-apple-darwin)"
    end
    on_intel do
      url "$(url x86_64-apple-darwin)"
      sha256 "$(sha x86_64-apple-darwin)"
    end
  end
  on_linux do
    on_arm do
      url "$(url aarch64-unknown-linux-musl)"
      sha256 "$(sha aarch64-unknown-linux-musl)"
    end
    on_intel do
      url "$(url x86_64-unknown-linux-musl)"
      sha256 "$(sha x86_64-unknown-linux-musl)"
    end
  end

  def install
    bin.install "wishlang"
    man1.install "man/wishlang.1" if File.exist?("man/wishlang.1")
    bash_completion.install "completions/bash" => "wishlang" if File.exist?("completions/bash")
    zsh_completion.install "completions/zsh" => "_wishlang" if File.exist?("completions/zsh")
    fish_completion.install "completions/fish" => "wishlang.fish" if File.exist?("completions/fish")
  end

  def caveats
    <<~EOS
      The \`wish\` alias is not installed: macOS ships /usr/bin/wish (Tk). Use \`wishlang\`,
      or alias it yourself.
    EOS
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/wishlang --version")
  end
end
RUBY
