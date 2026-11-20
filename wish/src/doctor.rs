//! `wishlang doctor`: check what wishlang depends on and report a fix for each problem.

use crate::{daemon, project::Project, setup};
use anyhow::Result;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Status {
    Ok,
    Warn,
    Fail,
}

struct Check {
    name: &'static str,
    status: Status,
    detail: String,
    fix: Option<String>,
}

fn ok(name: &'static str, detail: impl Into<String>) -> Check {
    Check { name, status: Status::Ok, detail: detail.into(), fix: None }
}
fn warn(name: &'static str, detail: impl Into<String>, fix: impl Into<String>) -> Check {
    Check { name, status: Status::Warn, detail: detail.into(), fix: Some(fix.into()) }
}
fn fail(name: &'static str, detail: impl Into<String>, fix: impl Into<String>) -> Check {
    Check { name, status: Status::Fail, detail: detail.into(), fix: Some(fix.into()) }
}

pub fn run(json: bool) -> Result<()> {
    let checks = checks();
    let failed = checks.iter().any(|c| c.status == Status::Fail);
    if json {
        let list: Vec<Value> = checks.iter().map(|c| json!({"name": c.name, "status": label(c.status), "detail": c.detail, "fix": c.fix})).collect();
        println!("{}", serde_json::to_string_pretty(&json!({"ok": !failed, "checks": list}))?);
    } else {
        for c in &checks {
            println!("{:<5} {:<14} {}", label(c.status), c.name, c.detail);
            if let Some(f) = &c.fix {
                println!("{:<20} fix: {f}", "");
            }
        }
    }
    if failed {
        std::process::exit(1);
    }
    Ok(())
}

fn label(s: Status) -> &'static str {
    match s {
        Status::Ok => "ok",
        Status::Warn => "warn",
        Status::Fail => "fail",
    }
}

fn checks() -> Vec<Check> {
    let exe = std::env::current_exe().ok().and_then(|p| p.canonicalize().ok());
    let mut out = vec![];
    out.push(match tool_version("claude") {
        Some((p, v)) => ok("claude", format!("{v} ({})", p.display())),
        None => fail("claude", "not on PATH", "install Claude Code: https://claude.com/claude-code"),
    });
    out.push(match tool_version("git") {
        Some((_, v)) => ok("git", v),
        None => fail("git", "not on PATH", "install git"),
    });
    let build: Vec<String> = ["cargo", "mvn"].iter().filter_map(|t| tool_version(t).map(|(_, v)| format!("{t}: {v}"))).collect();
    out.push(if build.is_empty() {
        warn("build tool", "neither cargo nor mvn on PATH", "install Rust (rustup.rs) or Maven")
    } else {
        ok("build tool", build.join(", "))
    });
    out.push(match which("wish") {
        Some(p) => warn("wish on PATH", format!("{} (the Tk windowing shell, or an old wish binary)", p.display()), "use `wishlang`; remove the old binary if it is ours"),
        None => ok("wish on PATH", "nothing shadows wishlang"),
    });
    out.push(match (which("wishlang"), &exe) {
        (None, _) => warn("wishlang path", "not on PATH", "add its directory to PATH (installer: ~/.local/bin)"),
        (Some(p), Some(me)) if p.canonicalize().ok().as_ref() != Some(me) => {
            warn("wishlang path", format!("first on PATH is {}, not this binary ({})", p.display(), me.display()), "remove the other install, or put this one first on PATH")
        }
        (Some(p), _) => ok("wishlang path", p.display().to_string()),
    });
    out.push(match daemon::status() {
        None => ok("daemon", "not running (starts when an editor needs it)"),
        Some(s) => match s["version"].as_str() {
            Some(v) if v != env!("CARGO_PKG_VERSION") => warn("daemon", format!("runs {v}, this CLI is {}", env!("CARGO_PKG_VERSION")), "restart it: wishlang daemon stop (or wishlang update)"),
            v => ok("daemon", format!("running {}", v.unwrap_or("?"))),
        },
    });
    out.push(hooks_check(setup::settings_path().ok().as_deref()));
    out.push(editors_check());
    out.push(match std::env::current_dir().ok().and_then(|d| Project::discover_from(&d)) {
        Some(p) => ok("project", p.root.display().to_string()),
        None => warn("project", "no wish.toml here or in a parent", "run `wishlang init` in a cargo or Maven project"),
    });
    out
}

fn hooks_check(settings: Option<&Path>) -> Check {
    let value: Value = settings.and_then(|p| std::fs::read_to_string(p).ok()).and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null);
    let cmds = setup::session_hooks(&value, "Stop");
    let Some(bin) = cmds.iter().find_map(|c| setup::session_bin(c)) else {
        return warn("claude hooks", "session hooks not installed", "run `wishlang setup`");
    };
    let path = match bin.strip_prefix("~/") {
        Some(rest) => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(rest),
        None => PathBuf::from(bin),
    };
    if path.exists() {
        ok("claude hooks", format!("installed, run {bin}"))
    } else {
        fail("claude hooks", format!("hook points at {bin}, which does not exist"), "run `wishlang setup`")
    }
}

fn editors_check() -> Check {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let (zed, jetbrains) = if cfg!(target_os = "macos") {
        (home.join("Library/Application Support/Zed/extensions/installed"), home.join("Library/Application Support/JetBrains"))
    } else {
        (home.join(".local/share/zed/extensions/installed"), home.join(".local/share/JetBrains"))
    };
    let mut found = vec![];
    if has_wish_entry(&zed) {
        found.push("Zed extension");
    }
    // `<JetBrains>/<product>/plugins` on macOS and Linux
    let ide_plugin_dirs = std::fs::read_dir(&jetbrains).into_iter().flatten().flatten().map(|e| e.path().join("plugins"));
    if ide_plugin_dirs.into_iter().any(|d| has_wish_entry(&d)) {
        found.push("IntelliJ plugin");
    }
    if found.is_empty() {
        warn("editor", "no Zed extension or IntelliJ plugin found", "install the wishlang extension in Zed or the plugin in IntelliJ (optional)")
    } else {
        ok("editor", found.join(", "))
    }
}

/// Does `dir` contain an entry (directory or jar) whose name has "wish" in it?
fn has_wish_entry(dir: &Path) -> bool {
    std::fs::read_dir(dir).into_iter().flatten().flatten().any(|e| e.file_name().to_string_lossy().to_lowercase().contains("wish"))
}

fn which(name: &str) -> Option<PathBuf> {
    let exts: &[&str] = if cfg!(windows) { &[".exe", ".cmd", ".bat"] } else { &[""] };
    std::env::split_paths(&std::env::var_os("PATH")?).find_map(|d| exts.iter().map(|e| d.join(format!("{name}{e}"))).find(|p| p.is_file()))
}

/// The path and first line of `--version` output of a tool on PATH.
fn tool_version(name: &str) -> Option<(PathBuf, String)> {
    let path = which(name)?;
    let out = Command::new(&path).arg("--version").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    Some((path, text.lines().next().unwrap_or("").trim().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wishlang-doctor-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn hooks_check_warns_when_missing_and_fails_on_a_dead_path() {
        let d = dir("hooks");
        assert_eq!(hooks_check(Some(&d.join("nope.json"))).status, Status::Warn);
        let s = d.join("settings.json");
        std::fs::write(&s, json!({"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "/no/such/wishlang session"}]}]}}).to_string()).unwrap();
        assert_eq!(hooks_check(Some(&s)).status, Status::Fail);
        let real = d.join("wishlang");
        std::fs::write(&real, "").unwrap();
        std::fs::write(&s, json!({"hooks": {"Stop": [{"hooks": [{"type": "command", "command": format!("{} session", real.display())}]}]}}).to_string()).unwrap();
        assert_eq!(hooks_check(Some(&s)).status, Status::Ok);
    }

    #[test]
    fn finds_wish_entries_in_plugin_dirs() {
        let d = dir("plugins");
        assert!(!has_wish_entry(&d));
        std::fs::write(d.join("other.jar"), "").unwrap();
        assert!(!has_wish_entry(&d));
        std::fs::create_dir(d.join("wishlang-intellij")).unwrap();
        assert!(has_wish_entry(&d));
    }
}
