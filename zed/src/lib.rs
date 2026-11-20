//! Zed extension for wishlang: launches `wish lsp` next to rust-analyzer.
//!
//! All behaviour lives in the language server (the `wish` binary); this
//! extension only finds `wishlang` on PATH (or via the WISH_BIN environment variable).

use zed_extension_api::{self as zed, LanguageServerId, Result, Worktree};

struct Wishlang;

impl zed::Extension for Wishlang {
    fn new() -> Self {
        Wishlang
    }

    fn language_server_command(&mut self, _id: &LanguageServerId, worktree: &Worktree) -> Result<zed::Command> {
        let env = worktree.shell_env();
        let explicit = env.iter().find(|(k, _)| k == "WISH_BIN").map(|(_, v)| v.clone());
        let command = explicit
            .or_else(|| worktree.which("wishlang"))
            .ok_or("wishlang: `wishlang` not found on PATH (install it with `cargo install --path wish`)")?;
        Ok(zed::Command { command, args: vec!["lsp".into()], env })
    }
}

zed::register_extension!(Wishlang);
