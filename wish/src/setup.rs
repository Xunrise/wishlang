//! `wishlang setup`: wire the session hooks and the explain permission into `~/.claude/settings.json`.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use similar::TextDiff;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

const EVENTS: [&str; 2] = ["UserPromptSubmit", "Stop"];

pub fn settings_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).context("no home directory")?;
    Ok(PathBuf::from(home).join(".claude/settings.json"))
}

pub fn run(uninstall: bool, yes: bool) -> Result<String> {
    let exe = std::env::current_exe()?.canonicalize()?;
    apply(&settings_path()?, &exe.to_string_lossy(), uninstall, |diff| {
        println!("{diff}");
        if yes {
            return Ok(true);
        }
        if !std::io::stdin().is_terminal() {
            bail!("not a terminal: pass --yes to apply the change above");
        }
        print!("Apply? [y/N] ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        Ok(line.trim().eq_ignore_ascii_case("y"))
    })
}

/// Merge (or remove) wishlang's entries in the settings file at `path`; `confirm` sees the diff first.
pub fn apply(path: &Path, exe: &str, uninstall: bool, confirm: impl FnOnce(&str) -> Result<bool>) -> Result<String> {
    let old = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let settings: Value = if old.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(&old).with_context(|| format!("{} is not valid JSON; fix it first", path.display()))?
    };
    let new = if old.trim().is_empty() && uninstall { old.clone() } else { serde_json::to_string_pretty(&merge(settings, exe, uninstall))? + "\n" };
    if new == old {
        return Ok(format!("{} is already up to date", path.display()));
    }
    let diff = TextDiff::from_lines(&old, &new).unified_diff().header(&path.display().to_string(), "new").to_string();
    if !confirm(&diff)? {
        return Ok("aborted, nothing changed".into());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if !old.is_empty() {
        std::fs::copy(path, path.with_extension("json.bak"))?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, new)?;
    std::fs::rename(&tmp, path)?;
    Ok(format!("{} {}", if uninstall { "removed wishlang from" } else { "wrote" }, path.display()))
}

/// Is this path (quotes and directory ignored) the `wish` or `wishlang` binary?
fn is_wishlang_bin(p: &str) -> bool {
    let name = p.trim_matches(['\'', '"']).rsplit(['/', '\\']).next().unwrap_or("");
    matches!(name.trim_end_matches(".exe"), "wish" | "wishlang")
}

/// The binary path of a wishlang session hook command, if it is one.
pub fn session_bin(cmd: &str) -> Option<&str> {
    cmd.trim().strip_suffix(" session").filter(|b| is_wishlang_bin(b)).map(|b| b.trim_matches(['\'', '"']))
}

fn is_explain_perm(p: &str) -> bool {
    p.strip_prefix("Bash(").and_then(|p| p.strip_suffix(" explain:*)")).is_some_and(is_wishlang_bin)
}

fn shell_quote(p: &str) -> String {
    if p.contains(' ') { format!("'{p}'") } else { p.to_string() }
}

/// Every wishlang session hook command registered for `event`.
pub fn session_hooks(settings: &Value, event: &str) -> Vec<String> {
    let groups = settings["hooks"][event].as_array().into_iter().flatten();
    let hooks = groups.flat_map(|g| g["hooks"].as_array().into_iter().flatten());
    hooks.filter_map(|h| h["command"].as_str()).filter(|c| session_bin(c).is_some()).map(String::from).collect()
}

fn merge(mut s: Value, exe: &str, uninstall: bool) -> Value {
    if !s.is_object() {
        s = json!({});
    }
    let cmd = format!("{} session", shell_quote(exe));
    let perm = format!("Bash({} explain:*)", shell_quote(exe));
    for ev in EVENTS {
        if let Some(groups) = s["hooks"][ev].as_array_mut() {
            for g in groups.iter_mut() {
                if let Some(hs) = g["hooks"].as_array_mut() {
                    hs.retain(|h| h["command"].as_str().is_none_or(|c| session_bin(c).is_none()));
                }
            }
            groups.retain(|g| g["hooks"].as_array().is_none_or(|h| !h.is_empty()));
        }
        if !uninstall {
            let entry = json!({"hooks": [{"type": "command", "command": cmd}]});
            match s["hooks"][ev].as_array_mut() {
                Some(a) => a.push(entry),
                None => s["hooks"][ev] = json!([entry]),
            }
        }
    }
    if let Some(a) = s["permissions"]["allow"].as_array_mut() {
        a.retain(|p| !p.as_str().is_some_and(is_explain_perm));
    }
    if !uninstall {
        match s["permissions"]["allow"].as_array_mut() {
            Some(a) => a.push(json!(perm)),
            None => s["permissions"]["allow"] = json!([perm]),
        }
    }
    prune(&mut s);
    s
}

/// Drop the empty containers an uninstall leaves behind.
fn prune(s: &mut Value) {
    if let Some(h) = s["hooks"].as_object_mut() {
        h.retain(|_, v| v.as_array().is_none_or(|a| !a.is_empty()));
    }
    if s["hooks"].as_object().is_some_and(|h| h.is_empty()) {
        s.as_object_mut().unwrap().remove("hooks");
    }
    if s["permissions"]["allow"].as_array().is_some_and(|a| a.is_empty()) {
        s["permissions"].as_object_mut().unwrap().remove("allow");
    }
    if s["permissions"].as_object().is_some_and(|p| p.is_empty()) {
        s.as_object_mut().unwrap().remove("permissions");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXE: &str = "/opt/bin/wishlang";

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wishlang-setup-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("settings.json")
    }
    fn read(p: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
    }
    fn run_yes(p: &Path, uninstall: bool) -> String {
        apply(p, EXE, uninstall, |_| Ok(true)).unwrap()
    }

    #[test]
    fn installs_into_a_missing_file() {
        let p = tmp("missing");
        run_yes(&p, false);
        let s = read(&p);
        for ev in EVENTS {
            assert_eq!(session_hooks(&s, ev), vec![format!("{EXE} session")]);
        }
        assert_eq!(s["permissions"]["allow"], json!([format!("Bash({EXE} explain:*)")]));
        assert!(!p.with_extension("json.bak").exists());
    }

    #[test]
    fn installs_into_an_empty_file() {
        let p = tmp("empty");
        std::fs::write(&p, "").unwrap();
        run_yes(&p, false);
        assert_eq!(session_hooks(&read(&p), "Stop").len(), 1);
    }

    #[test]
    fn keeps_foreign_hooks_and_replaces_old_wish_entries() {
        let p = tmp("foreign");
        let old = json!({
            "model": "opus",
            "hooks": {"Stop": [
                {"hooks": [{"type": "command", "command": "~/.cargo/bin/wish session"}, {"type": "command", "command": "other-tool"}]},
                {"matcher": "x", "hooks": [{"type": "command", "command": "~/.cargo/bin/wishlang session"}]}
            ]},
            "permissions": {"allow": ["Bash(ls:*)", "Bash(~/.cargo/bin/wish explain:*)"]}
        });
        std::fs::write(&p, old.to_string()).unwrap();
        run_yes(&p, false);
        let s = read(&p);
        assert_eq!(s["model"], "opus");
        assert_eq!(session_hooks(&s, "Stop"), vec![format!("{EXE} session")]);
        assert_eq!(s["hooks"]["Stop"][0]["hooks"][0]["command"], "other-tool");
        assert_eq!(s["hooks"]["Stop"].as_array().unwrap().len(), 2);
        assert_eq!(s["permissions"]["allow"], json!(["Bash(ls:*)", format!("Bash({EXE} explain:*)")]));
        assert!(p.with_extension("json.bak").exists());
    }

    #[test]
    fn a_second_run_changes_nothing() {
        let p = tmp("twice");
        run_yes(&p, false);
        let first = std::fs::read_to_string(&p).unwrap();
        let msg = apply(&p, EXE, false, |_| panic!("no diff expected")).unwrap();
        assert!(msg.contains("already up to date"));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), first);
    }

    #[test]
    fn declining_writes_nothing() {
        let p = tmp("decline");
        apply(&p, EXE, false, |_| Ok(false)).unwrap();
        assert!(!p.exists());
    }

    #[test]
    fn uninstall_removes_only_wishlang() {
        let p = tmp("uninstall");
        std::fs::write(&p, json!({"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "other-tool"}]}]}, "permissions": {"allow": ["Bash(ls:*)"]}}).to_string()).unwrap();
        run_yes(&p, false);
        run_yes(&p, true);
        let s = read(&p);
        assert!(session_hooks(&s, "Stop").is_empty());
        assert_eq!(s["hooks"]["Stop"][0]["hooks"][0]["command"], "other-tool");
        assert!(s["hooks"].get("UserPromptSubmit").is_none());
        assert_eq!(s["permissions"]["allow"], json!(["Bash(ls:*)"]));
        // from a clean install, uninstall leaves an empty object
        let q = tmp("uninstall2");
        run_yes(&q, false);
        run_yes(&q, true);
        assert_eq!(read(&q), json!({}));
    }
}
