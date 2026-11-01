//! Drives headless Claude Code (`claude -p`) as the implementation engine.
//!
//! Claude runs inside the user's project with a narrow tool allowlist and
//! with `wish hook` registered as a PreToolUse guard. It finishes with a
//! structured JSON report (validated against [`REPORT_SCHEMA`]).

use crate::events::{Event, Sink};
use crate::project::Project;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

pub const RULES: &str = r#"You are the implementation engine of wishlang, a "create from usage" tool for Rust.

The user writes short, intent-level Rust in their protected files (call sites that read like sentences). Your job: invent the vocabulary (types, enums, functions, modules) and the implementation, in OTHER files of this crate, so that the user's code compiles and does what it says.

Hard rules (a hook enforces most of them; denied edits come back to you as errors):
1. Never change the user's protected files, except to ADD single-line wiring at the top: `mod foo;` / `use foo::Bar;` / `use foo::*;`. Nothing else, never remove or alter a line.
2. Never change adopted items (code or doc comment). You may call them and add items around them.
3. Never touch wish.toml, DECISIONS.md or .wish/.
4. If the user's code is ambiguous, impossible, or would read better differently, or an adopted item must change, do not silently work around it: add an entry to `suggestions` in your report. `replace` must be text copied exactly from the file; `with` is its replacement.

How to write the vocabulary:
- Ordinary, clean, idiomatic Rust in normal modules (src/app.rs, src/window.rs, ...). No wishlang markers. It should look like code a careful human wrote.
- Interpret the user's names literally and naturally: `Mode::Maximized` means maximized, not fullscreen.
- Every public item gets a doc comment stating what it ACTUALLY does. When you made a choice the user's code did not force, add a `# Decisions` section with bullets, e.g.:
      /// Opens the window in the given mode.
      ///
      /// # Decisions
      /// - `Maximized` asks the window manager to maximize; it is not exclusive fullscreen.
  Keep these honest and short; skip the section when nothing was decided.
- Minimal change: when you are given a diff of the user's edits, change only what that diff requires. Do not rename, reorganize or "improve" unrelated items. Unchanged user lines mean unchanged behaviour.

Decisions (dependencies & architecture):
- Adding a crate, or a cross-cutting choice (backend, event loop model, error strategy, threading, global API conventions) is a decision. Report each NEW one in `decisions` (with `crates` listing every crate it adds). Do not re-report decisions already listed as proposed/accepted.
- Accepted decisions are binding. Rejected decisions are forbidden; choose an alternative that addresses the rejection reason, and report it as a new decision.
- Respect the user's constraints. Prefer few, well-known, current crates. Add crates with `cargo add`.

Finish only when `cargo check` passes. Do not run the program (it may open windows). Your final answer is the JSON report."#;

pub fn report_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "summary": {"type": "string", "description": "2-4 lines: what you built or changed and why"},
            "decisions": {"type": "array", "items": {
                "type": "object",
                "properties": {
                    "kind": {"type": "string", "enum": ["dependency", "architecture"]},
                    "title": {"type": "string"},
                    "choice": {"type": "string"},
                    "alternatives": {"type": "array", "items": {"type": "string"}},
                    "why": {"type": "string"},
                    "crates": {"type": "array", "items": {"type": "string"}}
                },
                "required": ["kind", "title", "choice", "alternatives", "why", "crates"]
            }},
            "suggestions": {"type": "array", "items": {
                "type": "object",
                "properties": {
                    "file": {"type": "string"},
                    "reason": {"type": "string"},
                    "replace": {"type": "string"},
                    "with": {"type": "string"}
                },
                "required": ["file", "reason", "replace", "with"]
            }}
        },
        "required": ["summary", "decisions", "suggestions"]
    })
}

#[derive(Debug, Default, Deserialize)]
pub struct Report {
    pub summary: String,
    #[serde(default)]
    pub decisions: Vec<ReportDecision>,
    #[serde(default)]
    pub suggestions: Vec<ReportSuggestion>,
}

#[derive(Debug, Deserialize)]
pub struct ReportDecision {
    pub kind: String,
    pub title: String,
    pub choice: String,
    #[serde(default)]
    pub alternatives: Vec<String>,
    pub why: String,
    #[serde(default)]
    pub crates: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct ReportSuggestion {
    pub file: String,
    pub reason: String,
    pub replace: String,
    pub with: String,
}

pub fn run(project: &Project, prompt: &str, sink: &mut dyn Sink) -> Result<Report> {
    let exe = std::env::current_exe()?;
    let settings = json!({"hooks": {"PreToolUse": [{
        "matcher": "Edit|Write|MultiEdit|NotebookEdit|Bash",
        "hooks": [{"type": "command", "command": format!("'{}' hook", exe.display())}]
    }]}});

    let mut cmd = Command::new(std::env::var("WISH_CLAUDE").unwrap_or_else(|_| "claude".into()));
    cmd.current_dir(&project.root)
        .arg("-p")
        .args(["--output-format", "stream-json", "--verbose"])
        .args(["--json-schema", &report_schema().to_string()])
        .args(["--settings", &settings.to_string()])
        .args(["--append-system-prompt", RULES])
        .args(["--allowedTools", "Read,Edit,Write,Glob,Grep,Bash(cargo check:*),Bash(cargo add:*),Bash(cargo remove:*),Bash(cargo tree:*)"]);
    if let Ok(model) = std::env::var("WISH_MODEL") {
        cmd.args(["--model", &model]);
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting `claude` (is Claude Code installed and on PATH?)")?;
    child.stdin.take().unwrap().write_all(prompt.as_bytes())?;

    let mut stderr = child.stderr.take().unwrap();
    let err_thread = std::thread::spawn(move || {
        let mut err = String::new();
        let _ = std::io::Read::read_to_string(&mut stderr, &mut err);
        err
    });
    let stdout = child.stdout.take().unwrap();
    let mut result: Option<Value> = None;
    for line in BufReader::new(stdout).lines() {
        let line = line?;
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        match v["type"].as_str() {
            Some("assistant") => {
                for c in v["message"]["content"].as_array().into_iter().flatten() {
                    match c["type"].as_str() {
                        Some("text") => sink.emit(Event::Says { text: c["text"].as_str().unwrap_or("").to_string() }),
                        Some("tool_use") => sink.emit(Event::Tool { name: c["name"].as_str().unwrap_or("?").to_string(), detail: tool_detail(project, &c["input"]) }),
                        _ => {}
                    }
                }
            }
            Some("user") => {
                for c in v["message"]["content"].as_array().into_iter().flatten() {
                    if c["type"] == "tool_result" && c["is_error"] == true {
                        let text = tool_result_text(&c["content"]);
                        if let Some(i) = text.find("wishlang:") {
                            sink.emit(Event::Blocked { reason: text[i + 9..].trim().lines().next().unwrap_or("").to_string() });
                        }
                    }
                }
            }
            Some("result") => result = Some(v),
            _ => {}
        }
    }
    let status = child.wait()?;
    let err = err_thread.join().unwrap_or_default();
    let Some(res) = result else {
        bail!("claude exited ({status}) without a result: {}", err.trim());
    };
    if res["is_error"] == true {
        bail!("claude reported an error: {}", res["result"].as_str().unwrap_or(&res["subtype"].to_string()));
    }
    if let Some(cost) = res["total_cost_usd"].as_f64() {
        let secs = res["duration_ms"].as_f64().unwrap_or(0.0) / 1000.0;
        sink.emit(Event::Status { msg: format!("claude finished in {secs:.0}s (${cost:.2})") });
    }
    let report_value = if res["structured_output"].is_object() {
        res["structured_output"].clone()
    } else {
        let text = res["result"].as_str().unwrap_or("");
        let start = text.find('{').unwrap_or(0);
        let end = text.rfind('}').map(|e| e + 1).unwrap_or(text.len());
        serde_json::from_str(&text[start..end]).unwrap_or(json!({"summary": text}))
    };
    Ok(serde_json::from_value(report_value).unwrap_or_default())
}

fn tool_detail(project: &Project, input: &Value) -> String {
    if let Some(f) = input["file_path"].as_str() {
        return project.rel(std::path::Path::new(f)).unwrap_or_else(|| f.to_string());
    }
    if let Some(c) = input["command"].as_str() {
        return c.to_string();
    }
    input["pattern"].as_str().unwrap_or("").to_string()
}

fn tool_result_text(c: &Value) -> String {
    match c {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().filter_map(|x| x["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}
