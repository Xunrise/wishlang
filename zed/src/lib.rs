//! Zed extension for wishlang: launches `wishlang lsp` next to rust-analyzer.
//!
//! All behaviour lives in the language server (the `wishlang` binary). This extension finds it through
//! `$WISH_BIN`, else on PATH, else downloads the latest GitHub release for this platform into its
//! working directory (`wishlang-<version>/`) and keeps only that version.

use std::fs;

use zed_extension_api::{self as zed, LanguageServerId, LanguageServerInstallationStatus as Status, Result, Worktree};

const REPO: &str = "Xunrise/wishlang";

#[derive(Default)]
struct Wishlang {
    /// The downloaded binary, once found or installed in this Zed session.
    cached: Option<String>,
}

impl Wishlang {
    fn downloaded(&mut self, id: &LanguageServerId) -> Result<String> {
        if let Some(path) = self.cached.as_ref().filter(|p| fs::metadata(p).is_ok_and(|m| m.is_file())) {
            return Ok(path.clone());
        }
        zed::set_language_server_installation_status(id, &Status::CheckingForUpdate);
        let release = zed::latest_github_release(REPO, zed::GithubReleaseOptions { require_assets: true, pre_release: false })?;
        let (os, arch) = zed::current_platform();
        let (suffix, kind, exe) = asset(os, arch)?;
        let asset = release
            .assets
            .iter()
            .find(|a| a.name.ends_with(&suffix))
            .ok_or_else(|| format!("wishlang {} has no release asset ending in {suffix}", release.version))?;

        let dir = format!("wishlang-{}", release.version);
        if find(&dir, exe).is_none() {
            zed::set_language_server_installation_status(id, &Status::Downloading);
            zed::download_file(&asset.download_url, &dir, kind).map_err(|e| format!("downloading wishlang: {e}"))?;
            // Older versions are no longer needed.
            for entry in fs::read_dir(".").map_err(|e| e.to_string())?.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with("wishlang-") && name != dir {
                    fs::remove_dir_all(entry.path()).ok();
                }
            }
        }
        let path = find(&dir, exe).ok_or_else(|| format!("no {exe} in the wishlang {} release asset", release.version))?;
        zed::make_file_executable(&path)?;
        zed::set_language_server_installation_status(id, &Status::None);
        self.cached = Some(path.clone());
        Ok(path)
    }
}

/// The release asset suffix, archive kind and binary name for a platform.
fn asset(os: zed::Os, arch: zed::Architecture) -> Result<(String, zed::DownloadedFileType, &'static str)> {
    let cpu = match arch {
        zed::Architecture::Aarch64 => "aarch64",
        zed::Architecture::X8664 => "x86_64",
        zed::Architecture::X86 => return Err("wishlang has no 32-bit x86 build".into()),
    };
    Ok(match os {
        zed::Os::Mac => (format!("-{cpu}-apple-darwin.tar.gz"), zed::DownloadedFileType::GzipTar, "wishlang"),
        zed::Os::Linux => (format!("-{cpu}-unknown-linux-musl.tar.gz"), zed::DownloadedFileType::GzipTar, "wishlang"),
        zed::Os::Windows => (format!("-{cpu}-pc-windows-msvc.zip"), zed::DownloadedFileType::Zip, "wishlang.exe"),
    })
}

/// `<dir>/<exe>`, or `<dir>/<sub>/<exe>` when the archive wraps the binary in a folder.
fn find(dir: &str, exe: &str) -> Option<String> {
    let direct = format!("{dir}/{exe}");
    if fs::metadata(&direct).is_ok_and(|m| m.is_file()) {
        return Some(direct);
    }
    fs::read_dir(dir).ok()?.flatten().map(|e| e.path().join(exe)).find(|p| p.is_file()).map(|p| p.to_string_lossy().into_owned())
}

impl zed::Extension for Wishlang {
    fn new() -> Self {
        Self::default()
    }

    fn language_server_command(&mut self, id: &LanguageServerId, worktree: &Worktree) -> Result<zed::Command> {
        let env = worktree.shell_env();
        let explicit = env.iter().find(|(k, _)| k == "WISH_BIN").map(|(_, v)| v.clone());
        let command = match explicit.or_else(|| worktree.which("wishlang")) {
            Some(path) => path,
            None => self.downloaded(id)?,
        };
        Ok(zed::Command { command, args: vec!["lsp".into()], env })
    }
}

zed::register_extension!(Wishlang);
