# Changelog

All notable changes are listed here. Versions follow [semver](https://semver.org).

## [Unreleased]

### Added
- Neovim plugin (`nvim/`): starts `wishlang lsp` for Rust and Java, tints generated code, hover reasons, code lenses for Implement and Accept/Reject, `:WishLog` and `:WishTicket`, and `:checkhealth wishlang`.

## [0.1.1] - 2026-11-20

First public release.

### Added
- `wishlang`: create-from-usage for Rust (Cargo) and Java (Maven, multi-module). You write the call sites, Claude Code writes a prototype first and the real implementation once you're happy with it.
- One per-user daemon serving every editor and project; `wishlang lsp` is a thin client to it.
- `wishlang setup` installs the Claude Code hooks that record ordinary sessions; `wishlang doctor` checks the installation.
- `wishlang update` updates from GitHub releases, and the release archives include shell completions and a man page.
- Ticket tracing: generated code is traced to the problems of the branch's ticket.
- IntelliJ plugin and Zed extension: generated code is tinted, with the reasons on hover, plus Implement and Accept/Reject actions and a ticket view.
- Installers for macOS, Linux and Windows, and a Homebrew tap.

### Changed
- The command is `wishlang`, because macOS ships Tk's `/usr/bin/wish`. `wish` is still available as an alias where it doesn't conflict.
- Large codebases: editor requests take milliseconds instead of seconds, cycles send Claude only the files you changed, and false "will be generated" markers on existing code are gone.
