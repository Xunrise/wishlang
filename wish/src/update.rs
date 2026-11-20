//! `wishlang update`: replace this binary with the latest (or a given) release.
//! Downloads go through `curl`, extraction through `tar`; the archive is verified against the release's SHA-256 checksums.

use crate::daemon;
use anyhow::{Context, Result, bail};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;

const REPO: &str = "Xunrise/wishlang";

#[derive(Clone, Copy, PartialEq, Debug)]
enum Install {
    SelfUpdate,
    Homebrew,
    Cargo,
}

impl Install {
    fn name(self) -> &'static str {
        match self {
            Install::SelfUpdate => "self",
            Install::Homebrew => "homebrew",
            Install::Cargo => "cargo",
        }
    }
}

fn detect(exe: &Path) -> Install {
    let p = exe.to_string_lossy().replace('\\', "/");
    if p.contains("/Cellar/") || p.contains("/homebrew/") {
        Install::Homebrew
    } else if p.contains("/.cargo/bin/") {
        Install::Cargo
    } else {
        Install::SelfUpdate
    }
}

/// The release triple of this build (Linux builds are musl).
fn target() -> Option<&'static str> {
    use std::env::consts::{ARCH, OS};
    Some(match (OS, ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "aarch64") => "aarch64-unknown-linux-musl",
        ("linux", "x86_64") => "x86_64-unknown-linux-musl",
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        _ => return None,
    })
}

fn asset_name(version: &str, target: &str) -> String {
    let ext = if target.contains("windows") { "zip" } else { "tar.gz" };
    format!("wishlang-{version}-{target}.{ext}")
}

/// The checksum listed for `file` in a `<sha256>  <file>` (or `*<file>`) list.
fn checksum_for(list: &str, file: &str) -> Option<String> {
    list.lines().filter_map(|l| l.split_once(char::is_whitespace)).find(|(_, f)| f.trim().trim_start_matches('*') == file).map(|(h, _)| h.to_lowercase())
}

fn sha256_file(path: &Path) -> Result<String> {
    Ok(Sha256::digest(std::fs::read(path)?).iter().map(|b| format!("{b:02x}")).collect())
}

fn newer(latest: &str, current: &str) -> bool {
    let core = |v: &str| -> Option<Vec<u64>> { v.split('-').next()?.split('.').map(|n| n.parse().ok()).collect() };
    match (core(latest), core(current)) {
        (Some(l), Some(c)) => l > c || (l == c && latest != current && !latest.contains('-')),
        _ => latest != current,
    }
}

fn curl(args: &[&str]) -> Result<String> {
    let out = Command::new("curl").args(args).output().context("could not run curl")?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn latest_version() -> Result<String> {
    let url = curl(&["-fsSLI", "-o", "/dev/null", "-w", "%{url_effective}", &format!("https://github.com/{REPO}/releases/latest")])
        .context("could not resolve the latest release (the repository may still be private; try --version)")?;
    match url.rsplit_once("/v") {
        Some((_, v)) if !v.is_empty() && !v.contains('/') => Ok(v.to_string()),
        _ => bail!("could not resolve the latest release (got {url})"),
    }
}

pub fn run(check: bool, json: bool, version: Option<String>) -> Result<()> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let install = detect(&exe);
    let current = env!("CARGO_PKG_VERSION");
    let wanted = version.or_else(|| std::env::var("WISHLANG_VERSION").ok()).map(|v| v.trim_start_matches('v').to_string());
    if check || json {
        let latest = match &wanted {
            Some(v) => v.clone(),
            None => latest_version()?,
        };
        let available = newer(&latest, current);
        if json {
            println!("{}", json!({"current": current, "latest": latest, "update_available": available, "install": install.name()}));
        } else if available {
            println!("wishlang {latest} is available (you have {current}); run `wishlang update`");
        } else {
            println!("wishlang {current} is up to date");
        }
        if available {
            std::process::exit(1);
        }
        return Ok(());
    }
    match install {
        Install::Homebrew => {
            println!("wishlang is managed by Homebrew; run `brew upgrade wishlang`");
            return Ok(());
        }
        Install::Cargo => {
            println!("wishlang was installed by cargo; run `cargo install wishlang`");
            return Ok(());
        }
        Install::SelfUpdate => {}
    }
    let version = match wanted {
        Some(v) => v,
        None => latest_version()?,
    };
    if version == current {
        println!("wishlang {current} is already installed");
        return Ok(());
    }
    let target = target().context("no release build for this platform")?;
    let base = std::env::var("WISHLANG_BASE_URL").unwrap_or_else(|_| format!("https://github.com/{REPO}/releases/download/v{version}"));
    println!("Updating wishlang {current} -> {version}");
    install_release(&exe, &version, target, &base)?;
    if daemon::stop() {
        println!("Restarted the daemon handover; editors reconnect on the new version.");
    }
    println!("Updated wishlang to {version}");
    Ok(())
}

/// Download, verify and swap in the release for `target`, replacing the binary at `exe`.
fn install_release(exe: &Path, version: &str, target: &str, base: &str) -> Result<()> {
    let dir = exe.parent().context("binary has no directory")?.join(format!(".wishlang-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot write next to {}", exe.display()))?;
    let result = download_and_swap(exe, version, target, base, &dir);
    let _ = std::fs::remove_dir_all(&dir);
    result
}

fn download_and_swap(exe: &Path, version: &str, target: &str, base: &str, dir: &Path) -> Result<()> {
    let asset = asset_name(version, target);
    let fetch = |name: &str, to: &Path| -> Result<()> {
        let url = format!("{}/{name}", base.trim_end_matches('/'));
        curl(&["-fsSL", &url, "-o", &to.to_string_lossy()]).with_context(|| format!("download failed: {url} (the repository may still be private)"))?;
        Ok(())
    };
    let archive = dir.join(&asset);
    let sums = dir.join("checksums.txt");
    fetch(&asset, &archive)?;
    fetch(&format!("wishlang-{version}-checksums.txt"), &sums)?;
    let want = checksum_for(&std::fs::read_to_string(&sums)?, &asset).with_context(|| format!("no checksum listed for {asset}"))?;
    let got = sha256_file(&archive)?;
    if want != got {
        bail!("checksum mismatch for {asset} (expected {want}, got {got})");
    }
    let out = dir.join("x");
    std::fs::create_dir(&out)?;
    let status = Command::new("tar").arg("-xf").arg(&archive).arg("-C").arg(&out).status().context("could not run tar")?;
    if !status.success() {
        bail!("could not extract {asset}");
    }
    let bin_name = if cfg!(windows) { "wishlang.exe" } else { "wishlang" };
    let new = find_file(&out, bin_name).with_context(|| format!("archive has no {bin_name}"))?;
    let said = Command::new(&new).arg("--version").output().context("the downloaded binary does not run")?;
    let said = String::from_utf8_lossy(&said.stdout);
    if said.trim() != format!("wishlang {version}") {
        bail!("the downloaded binary reports {:?}, expected \"wishlang {version}\"", said.trim());
    }
    // same directory as the target: the rename is atomic
    let staged = dir.join("wishlang.new");
    std::fs::rename(&new, &staged).or_else(|_| std::fs::copy(&new, &staged).map(|_| ()))?;
    if cfg!(windows) {
        let old = exe.with_extension("old");
        let _ = std::fs::remove_file(&old);
        std::fs::rename(exe, &old)?;
    }
    std::fs::rename(&staged, exe).with_context(|| format!("could not replace {}", exe.display()))
}

fn find_file(dir: &Path, name: &str) -> Option<PathBuf> {
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        if p.is_dir() {
            if let Some(f) = find_file(&p, name) {
                return Some(f);
            }
        } else if p.file_name().is_some_and(|n| n == name) {
            return Some(p);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wishlang-update-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn parses_checksums_with_either_separator() {
        let list = "AAA111  wishlang-1.0.0-x.tar.gz\nbbb222 *wishlang-1.0.0-y.tar.gz\n";
        assert_eq!(checksum_for(list, "wishlang-1.0.0-x.tar.gz").as_deref(), Some("aaa111"));
        assert_eq!(checksum_for(list, "wishlang-1.0.0-y.tar.gz").as_deref(), Some("bbb222"));
        assert_eq!(checksum_for(list, "wishlang-1.0.0-z.tar.gz"), None);
    }

    #[test]
    fn asset_names_follow_the_release_layout() {
        assert_eq!(asset_name("0.2.0", "aarch64-apple-darwin"), "wishlang-0.2.0-aarch64-apple-darwin.tar.gz");
        assert_eq!(asset_name("0.2.0", "x86_64-unknown-linux-musl"), "wishlang-0.2.0-x86_64-unknown-linux-musl.tar.gz");
        assert_eq!(asset_name("0.2.0", "x86_64-pc-windows-msvc"), "wishlang-0.2.0-x86_64-pc-windows-msvc.zip");
    }

    #[test]
    fn detects_the_install_method() {
        assert_eq!(detect(Path::new("/opt/homebrew/Cellar/wishlang/0.1.0/bin/wishlang")), Install::Homebrew);
        assert_eq!(detect(Path::new("/home/linuxbrew/.linuxbrew/Cellar/wishlang/0.1.0/bin/wishlang")), Install::Homebrew);
        assert_eq!(detect(Path::new("/Users/me/.cargo/bin/wishlang")), Install::Cargo);
        assert_eq!(detect(Path::new("/Users/me/.local/bin/wishlang")), Install::SelfUpdate);
    }

    #[test]
    fn compares_versions() {
        assert!(newer("0.2.0", "0.1.9"));
        assert!(newer("0.10.0", "0.9.0"));
        assert!(!newer("0.1.0", "0.1.0"));
        assert!(!newer("0.1.0", "0.2.0"));
        assert!(!newer("0.2.0-rc1", "0.2.0"));
    }

    /// A mirror directory holding the release assets of a fake 9.9.9 whose binary is a shell script.
    #[cfg(unix)]
    fn mirror(name: &str, corrupt_checksum: bool) -> (PathBuf, String) {
        use std::os::unix::fs::PermissionsExt;
        let target = target().expect("supported test platform");
        let root = dir(name);
        let top = format!("wishlang-9.9.9-{target}");
        std::fs::create_dir_all(root.join("src").join(&top)).unwrap();
        let bin = root.join("src").join(&top).join("wishlang");
        std::fs::write(&bin, "#!/bin/sh\necho \"wishlang 9.9.9\"\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let assets = root.join("assets");
        std::fs::create_dir(&assets).unwrap();
        let asset = asset_name("9.9.9", target);
        assert!(Command::new("tar").arg("-czf").arg(assets.join(&asset)).arg("-C").arg(root.join("src")).arg(&top).status().unwrap().success());
        let sum = if corrupt_checksum { "0".repeat(64) } else { sha256_file(&assets.join(&asset)).unwrap() };
        std::fs::write(assets.join("wishlang-9.9.9-checksums.txt"), format!("{sum}  {asset}\n")).unwrap();
        (root.join("current"), format!("file://{}", assets.display()))
    }

    #[cfg(unix)]
    fn dummy_current(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(dir).unwrap();
        let exe = dir.join("wishlang");
        std::fs::write(&exe, "#!/bin/sh\necho \"wishlang 0.0.1\"\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        exe
    }

    #[cfg(unix)]
    #[test]
    fn updates_from_a_local_mirror() {
        let (cur, base) = mirror("e2e", false);
        let exe = dummy_current(&cur);
        install_release(&exe, "9.9.9", target().unwrap(), &base).unwrap();
        let out = Command::new(&exe).arg("--version").output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "wishlang 9.9.9");
        assert_eq!(std::fs::read_dir(&cur).unwrap().count(), 1, "temp files are cleaned up");
    }

    #[cfg(unix)]
    #[test]
    fn a_checksum_mismatch_leaves_the_binary_alone() {
        let (cur, base) = mirror("mismatch", true);
        let exe = dummy_current(&cur);
        let err = install_release(&exe, "9.9.9", target().unwrap(), &base).unwrap_err();
        assert!(err.to_string().contains("checksum mismatch"), "{err:#}");
        let out = Command::new(&exe).arg("--version").output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "wishlang 0.0.1");
        assert_eq!(std::fs::read_dir(&cur).unwrap().count(), 1);
    }
}
