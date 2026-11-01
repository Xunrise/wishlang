//! Claude Code PreToolUse guard (`wish hook`).
//!
//! Reads the hook JSON on stdin, computes what the file *would* look like
//! after the tool call, and denies the call if it touches the user's code,
//! adopted items, or tool-owned files. Denial reasons are written for Claude:
//! they tell it to file a suggestion instead.

use crate::items;
use crate::project::{Project, wiring_only};
use serde_json::{Value, json};
use std::path::Path;

pub enum Verdict {
    Allow,
    Deny(String),
}

pub fn run() -> anyhow::Result<()> {
    let mut input = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut input)?;
    let v: Value = serde_json::from_str(&input)?;
    if let Verdict::Deny(reason) = evaluate(&v) {
        let out = json!({"hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": format!("wishlang: {reason}"),
        }});
        println!("{out}");
    }
    Ok(())
}

pub fn evaluate(v: &Value) -> Verdict {
    let tool = v["tool_name"].as_str().unwrap_or("");
    let input = &v["tool_input"];
    let cwd = v["cwd"].as_str().map(Path::new).unwrap_or(Path::new("."));

    if tool == "Bash" {
        return bash(input["command"].as_str().unwrap_or(""), cwd);
    }
    let Some(file) = input["file_path"].as_str().or(input["notebook_path"].as_str()) else { return Verdict::Allow };
    let path = if Path::new(file).is_absolute() { Path::new(file).to_path_buf() } else { cwd.join(file) };
    let Some(project) = Project::discover_from(path.parent().unwrap_or(cwd)) else { return Verdict::Allow };
    let Some(rel) = project.rel(&path) else { return Verdict::Allow };

    if project.is_tool_owned(&rel) {
        return Verdict::Deny(format!("{rel} is maintained by the wish tool. Report decisions and suggestions in your final JSON report instead."));
    }

    let current = project.read(&rel);
    let proposed = match proposed_content(tool, input, current.as_deref()) {
        Some(p) => p,
        None => return Verdict::Allow, // not a file-writing tool we understand
    };

    if project.is_protected(&rel) {
        let old = current.unwrap_or_default();
        return match wiring_only(&old, &proposed) {
            Ok(()) => Verdict::Allow,
            Err(why) => Verdict::Deny(format!(
                "{rel} is the user's code and is protected; this edit {why}. Do not change the user's lines. \
                 If you think they should change, add an entry to `suggestions` in your final report."
            )),
        };
    }

    // Lines you wrote inside a generated file must survive (provenance from the git journal).
    if let (Some(cur), Ok(git)) = (current.as_deref(), crate::git::Git::open(&project.root)) {
        if let Some(whos) = crate::provenance::of(&git, &rel, cur) {
            let missing = crate::provenance::missing(&crate::provenance::human_lines(&whos, cur), &proposed);
            if let Some(l) = missing.first() {
                return Verdict::Deny(format!(
                    "the user wrote the line `{l}` in {rel}; this edit removes or changes it. Keep the user's lines verbatim and \
                     make the code around them fulfil what they ask. If a line of theirs must change, add an entry to `suggestions`."
                ));
            }
        }
    }

    let adopted: Vec<(&String, &String)> = project.cfg.adopted.iter().filter(|(_, a)| a.file == rel).map(|(p, a)| (p, &a.hash)).collect();
    if adopted.is_empty() {
        return Verdict::Allow;
    }
    let names = adopted.iter().map(|(p, _)| format!("`{p}`")).collect::<Vec<_>>().join(", ");
    let Ok(new_items) = items::parse(&proposed) else {
        return Verdict::Deny(format!("this edit would leave {rel} unparseable, and it contains adopted items ({names}). Make the edit in one complete step."));
    };
    for (path, hash) in adopted {
        match new_items.iter().find(|i| &i.path == path) {
            Some(i) if &i.hash == hash => {}
            Some(_) => return Verdict::Deny(format!("`{path}` is adopted by the user and protected; this edit changes it. Leave it exactly as is (code and doc comment). If it must change, add an entry to `suggestions` in your final report.")),
            None => return Verdict::Deny(format!("`{path}` is adopted by the user and must stay in {rel}; this edit removes or moves it. If it must change, add an entry to `suggestions` in your final report.")),
        }
    }
    Verdict::Allow
}

fn proposed_content(tool: &str, input: &Value, current: Option<&str>) -> Option<String> {
    match tool {
        "Write" => input["content"].as_str().map(String::from),
        "Edit" => Some(apply_edit(current.unwrap_or(""), input)),
        "MultiEdit" => {
            let mut s = current.unwrap_or("").to_string();
            for e in input["edits"].as_array()? {
                s = apply_edit(&s, e);
            }
            Some(s)
        }
        // Treat any other file-targeting tool as a full rewrite we can't inspect.
        "NotebookEdit" => Some(String::from("\u{0}unknown")),
        _ => None,
    }
}

fn apply_edit(s: &str, e: &Value) -> String {
    let old = e["old_string"].as_str().unwrap_or("");
    let new = e["new_string"].as_str().unwrap_or("");
    if old.is_empty() {
        return format!("{new}{s}");
    }
    if e["replace_all"].as_bool().unwrap_or(false) { s.replace(old, new) } else { s.replacen(old, new, 1) }
}

fn bash(cmd: &str, cwd: &Path) -> Verdict {
    let re = regex::Regex::new(r"\bwish\s+(adopt|unadopt|accept|reject|decide|init|undo)\b").unwrap();
    if re.is_match(cmd) {
        return Verdict::Deny("adopting items, accepting suggestions and deciding are the user's actions. Report them in your final JSON report instead.".into());
    }
    let Some(project) = Project::discover_from(cwd) else { return Verdict::Allow };
    let writes = regex::Regex::new(r"(>|\btee\b|\bsed\s+-i|\bmv\b|\brm\b|\bcp\b|\btruncate\b|\bperl\s+-[a-z]*i)").unwrap();
    if writes.is_match(cmd) {
        let mut guarded: Vec<String> = project.cfg.protected.files.clone();
        guarded.extend(project.cfg.adopted.values().map(|a| a.file.clone()));
        guarded.extend(["wish.toml".into(), "DECISIONS.md".into(), ".wish".into()]);
        if let Some(f) = guarded.iter().find(|f| cmd.contains(f.as_str())) {
            return Verdict::Deny(format!("shell writes touching {f} are not allowed (protected by wishlang). Use the Edit tool on files you own."));
        }
    }
    Verdict::Allow
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn setup() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("wish-hook-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/main.rs"), "fn main() {\n    App::new(\"Demo\").run()\n}\n").unwrap();
        let lib = "pub struct App;\nimpl App {\n    /// New.\n    pub fn new(_: &str) -> Self { App }\n    pub fn run(self) {}\n}\n";
        fs::write(dir.join("src/app.rs"), lib).unwrap();
        let hash = items::parse(lib).unwrap().into_iter().find(|i| i.path == "App::new").unwrap().hash;
        fs::write(dir.join("wish.toml"), format!("[protected]\nfiles = [\"src/main.rs\"]\n\n[adopted]\n\"App::new\" = {{ file = \"src/app.rs\", hash = \"{hash}\" }}\n")).unwrap();
        dir
    }

    fn edit(dir: &Path, file: &str, old: &str, new: &str) -> Value {
        json!({"tool_name": "Edit", "cwd": dir, "tool_input": {"file_path": dir.join(file), "old_string": old, "new_string": new}})
    }

    fn denied(v: &Value) -> bool {
        matches!(evaluate(v), Verdict::Deny(_))
    }

    #[test]
    fn guards() {
        let d = setup();
        // user's line: deny
        assert!(denied(&edit(&d, "src/main.rs", "run()", "run_forever()")));
        // wiring into user's file: allow
        assert!(!denied(&edit(&d, "src/main.rs", "fn main", "mod app;\nuse app::*;\n\nfn main")));
        // adopted method: deny (code and docs)
        assert!(denied(&edit(&d, "src/app.rs", "Self { App }", "Self { todo!() }")));
        assert!(denied(&edit(&d, "src/app.rs", "/// New.", "/// Brand new.")));
        // unadopted method in same file: allow
        assert!(!denied(&edit(&d, "src/app.rs", "pub fn run(self) {}", "pub fn run(self) { loop {} }")));
        // new file: allow
        assert!(!denied(&json!({"tool_name": "Write", "cwd": d, "tool_input": {"file_path": d.join("src/window.rs"), "content": "pub struct Window;"}})));
        // tool-owned files: deny
        assert!(denied(&json!({"tool_name": "Write", "cwd": d, "tool_input": {"file_path": d.join("wish.toml"), "content": ""}})));
        // bash
        assert!(denied(&json!({"tool_name": "Bash", "cwd": d, "tool_input": {"command": "wish adopt App::run"}})));
        assert!(denied(&json!({"tool_name": "Bash", "cwd": d, "tool_input": {"command": "sed -i s/run/go/ src/main.rs"}})));
        assert!(!denied(&json!({"tool_name": "Bash", "cwd": d, "tool_input": {"command": "cargo check"}})));
        let _ = fs::remove_dir_all(&d);
    }
}
