//! One compile cycle: user code → Claude → verified vocabulary → build.

use crate::claude;
use crate::events::{Event, Sink};
use crate::git::Git;
use crate::items;
use crate::project::{Decision, Project, Status, Suggestion, wiring_only};
use anyhow::Result;
use std::collections::BTreeMap;
use std::process::{Child, Command, Stdio};

pub struct Options {
    /// Run Claude even if the code compiles and nothing changed.
    pub force: bool,
    /// Extra instruction from the user for this cycle only.
    pub note: Option<String>,
}

pub fn cargo_check(project: &Project) -> (bool, String) {
    let out = Command::new("cargo")
        .args(["check", "--quiet", "--message-format", "short"])
        .current_dir(&project.root)
        .output();
    match out {
        Ok(o) => (o.status.success(), String::from_utf8_lossy(&o.stderr).trim().to_string()),
        Err(e) => (false, e.to_string()),
    }
}

/// Unified diff of the user's edits since the last green cycle, or None on
/// the first cycle. `now` is a snapshot of the current working tree.
pub fn user_diff(p: &Project, git: &Git, now: &str) -> Result<Option<String>> {
    match git.resolve(&git.ref_name("last")) {
        None => Ok(None),
        Some(last) => Ok(Some(git.diff(&last, now, &p.cfg.protected.files)?)),
    }
}

/// Item path -> hash for every Claude-owned Rust file at a snapshot.
fn items_at(p: &Project, git: &Git, sha: &str, files: &[String]) -> BTreeMap<String, String> {
    let srcs: BTreeMap<String, String> = files
        .iter()
        .filter(|f| f.ends_with(".rs") && !p.is_protected(f))
        .filter_map(|f| git.show(sha, f).map(|s| (f.clone(), s)))
        .collect();
    items::hash_map(&srcs)
}

fn numbered(src: &str) -> String {
    src.lines().enumerate().map(|(i, l)| format!("{:>4} | {l}\n", i + 1)).collect()
}

fn build_prompt(p: &Project, diff: &Option<String>, check_errors: &str, note: &Option<String>) -> String {
    let mut s = String::new();
    s.push_str("# The user's code (protected)\n\n");
    for (f, src) in p.protected_files() {
        s.push_str(&format!("## {f}\n```rust\n{}```\n\n", numbered(&src)));
    }
    match diff {
        None => s.push_str("# What changed\nFirst cycle: nothing has been generated yet. Build the vocabulary from scratch.\n\n"),
        Some(d) if d.is_empty() => s.push_str("# What changed\nThe user's code did not change since the last cycle. Only fix what is broken.\n\n"),
        Some(d) => s.push_str(&format!("# What changed since the last cycle (your regeneration scope)\n```diff\n{d}```\n\n")),
    }
    if let Some(n) = note {
        s.push_str(&format!("# Note from the user for this cycle\n{n}\n\n"));
    }
    let owned: Vec<_> = p.owned_files().into_keys().collect();
    s.push_str(&format!("# Files you own\n{}\n\n", if owned.is_empty() { "(none yet)".into() } else { owned.join(", ") }));
    if !check_errors.is_empty() {
        s.push_str(&format!("# Current `cargo check` output\n```\n{}\n```\n\n", truncate(check_errors, 6000)));
    }
    if !p.cfg.adopted.is_empty() {
        s.push_str("# Adopted items (protected; do not change)\n");
        for (path, a) in &p.cfg.adopted {
            s.push_str(&format!("- `{path}` in {}\n", a.file));
        }
        s.push('\n');
    }
    if !p.cfg.constraints.rules.is_empty() {
        s.push_str("# The user's constraints\n");
        for r in &p.cfg.constraints.rules {
            s.push_str(&format!("- {r}\n"));
        }
        s.push('\n');
    }
    let ds = p.decisions();
    if !ds.is_empty() {
        s.push_str("# Decisions so far\n");
        for d in &ds {
            let st = match d.status {
                Status::Proposed => "proposed (keep using unless you must change it)".to_string(),
                Status::Accepted => "ACCEPTED (binding)".to_string(),
                Status::Rejected => format!("REJECTED (forbidden): {}", d.reason.clone().unwrap_or_default()),
            };
            s.push_str(&format!("- {} [{}] {}: {} (crates: {}): {st}\n", d.id, d.kind, d.title, d.choice, d.crates.join(", ")));
        }
        s.push('\n');
    }
    let pending: Vec<_> = p.suggestions().into_iter().filter(|s| s.status == Status::Proposed).collect();
    if !pending.is_empty() {
        s.push_str("# Your suggestions still pending with the user (don't repeat them)\n");
        for x in pending {
            s.push_str(&format!("- {} {}: {}\n", x.id, x.file, x.reason));
        }
        s.push('\n');
    }
    s.push_str("Make the user's code compile and do what it says, following your rules. Then give the JSON report.\n");
    s
}

fn truncate(s: &str, n: usize) -> &str {
    if s.len() <= n {
        return s;
    }
    let mut i = n;
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    &s[..i]
}

/// Run one cycle. Returns Ok(true) if the project compiles at the end.
pub fn compile(p: &mut Project, sink: &mut dyn Sink, opts: &Options) -> Result<bool> {
    let git = Git::open(&p.root)?;
    let pre = git.snapshot(None, "wish: before cycle")?;
    let diff = user_diff(p, &git, &pre)?;

    sink.emit(Event::Status { msg: "checking".into() });
    let (ok, errors) = cargo_check(p);
    let unchanged = diff.as_deref() == Some("");
    if ok && diff.is_none() && !opts.force && opts.note.is_none() {
        // Compiles but has no cycle history yet (e.g. an existing project): take it as the baseline.
        let sha = git.snapshot(None, "wish cycle: baseline")?;
        git.set_ref(&git.ref_name("last"), &sha)?;
        sink.emit(Event::Check { ok: true, errors: String::new() });
        sink.emit(Event::Done { ok: true, summary: "project already compiles: recorded it as the baseline".into() });
        return Ok(true);
    }
    if ok && unchanged && !opts.force && opts.note.is_none() {
        sink.emit(Event::Check { ok: true, errors: String::new() });
        sink.emit(Event::Done { ok: true, summary: "nothing changed; vocabulary is up to date".into() });
        return Ok(true);
    }

    sink.emit(Event::Status { msg: match &diff {
        None => "first cycle: asking Claude to build the vocabulary".into(),
        Some(d) if d.is_empty() => "asking Claude to fix the build".into(),
        Some(d) => format!("your code changed ({} lines): asking Claude to regenerate", d.lines().filter(|l| (l.starts_with('+') || l.starts_with('-')) && !l.starts_with("+++") && !l.starts_with("---")).count()),
    }});
    let prompt = build_prompt(p, &diff, if ok { "" } else { &errors }, &opts.note);
    let report = match claude::run(p, &prompt, sink) {
        Ok(r) => r,
        Err(e) => {
            sink.emit(Event::Done { ok: false, summary: format!("{e:#}") });
            return Ok(false);
        }
    };
    let post = git.snapshot(Some(&pre), "wish: after claude")?;

    // ---- verify (backstop for the hook) ----
    let mut violations = p.verify_adopted();
    for f in &p.cfg.protected.files {
        let old = git.show(&pre, f).unwrap_or_default();
        let new = git.show(&post, f).unwrap_or_default();
        match wiring_only(&old, &new) {
            Ok(()) => {
                let added: Vec<String> = similar::TextDiff::from_lines(old.as_str(), new.as_str())
                    .iter_all_changes()
                    .filter(|c| c.tag() == similar::ChangeTag::Insert && !c.value().trim().is_empty())
                    .map(|c| c.value().trim().to_string())
                    .collect();
                if !added.is_empty() {
                    sink.emit(Event::Wiring { file: f.clone(), lines: added });
                }
            }
            Err(why) => violations.push(format!("{f}: Claude's edit {why}")),
        }
    }
    if !violations.is_empty() {
        for v in &violations {
            sink.emit(Event::Violation { msg: v.clone() });
        }
        git.restore(&pre, &post)?;
        sink.emit(Event::Done { ok: false, summary: "protection violated: all changes from this cycle were reverted".into() });
        return Ok(false);
    }

    // ---- item-level change report (from git: only files that changed) ----
    let touched: Vec<String> = git.changed(&pre, &post)?.into_iter().map(|(_, f)| f).collect();
    let a = items_at(p, &git, &pre, &touched);
    let b = items_at(p, &git, &post, &touched);
    let added: Vec<String> = b.keys().filter(|k| !a.contains_key(*k)).cloned().collect();
    let removed: Vec<String> = a.keys().filter(|k| !b.contains_key(*k)).cloned().collect();
    let changed: Vec<String> = b.iter().filter(|(k, h)| a.get(*k).is_some_and(|x| x != *h)).map(|(k, _)| k.clone()).collect();
    let total = items::hash_map(&p.owned_files().into_iter().filter(|(k, _)| k.ends_with(".rs")).collect::<BTreeMap<_, _>>()).len();
    let untouched = total.saturating_sub(added.len() + changed.len());
    sink.emit(Event::Changes { added, changed, removed, untouched });

    // ---- record decisions & suggestions ----
    let mut ds = p.decisions();
    for d in report.decisions {
        let id = ds.iter().map(|d| d.id).max().unwrap_or(0) + 1;
        sink.emit(Event::Decision { id, kind: d.kind.clone(), title: d.title.clone(), choice: d.choice.clone() });
        ds.push(Decision { id, kind: d.kind, title: d.title, choice: d.choice, alternatives: d.alternatives, why: d.why, crates: d.crates, status: Status::Proposed, reason: None });
    }
    p.save_decisions(&ds)?;
    let mut ss = p.suggestions();
    for s in report.suggestions {
        let id = ss.iter().map(|s| s.id).max().unwrap_or(0) + 1;
        sink.emit(Event::Suggestion { id, file: s.file.clone(), reason: s.reason.clone() });
        ss.push(Suggestion { id, file: s.file, reason: s.reason, replace: s.replace, with: s.with, status: Status::Proposed });
    }
    p.save_suggestions(&ss)?;

    for dep in p.undecided_dependencies() {
        sink.emit(Event::Warning { msg: format!("dependency `{dep}` has no decision record") });
    }

    let (ok, errors) = cargo_check(p);
    sink.emit(Event::Check { ok, errors });
    if ok {
        // The green cycle becomes the new baseline; its parent chain is the cycle history.
        let last_ref = git.ref_name("last");
        let last = git.resolve(&last_ref);
        let msg = format!("wish cycle: {}", report.summary);
        let sha = git.snapshot(last.as_deref(), &msg)?;
        git.set_ref(&last_ref, &sha)?;
    }
    sink.emit(Event::Done { ok, summary: report.summary });
    Ok(ok)
}

pub fn build(p: &Project) -> (bool, String) {
    match Command::new("cargo").args(["build", "--quiet"]).current_dir(&p.root).output() {
        Ok(o) => (o.status.success(), String::from_utf8_lossy(&o.stderr).to_string()),
        Err(e) => (false, e.to_string()),
    }
}

/// Launch the app as the live preview.
pub fn spawn_preview(p: &Project, piped: bool) -> std::io::Result<Child> {
    // Run the built binary directly: killing it then really stops the app
    // (killing `cargo run` would orphan it), and cargo's warnings stay out.
    let name = p
        .read("Cargo.toml")
        .and_then(|t| t.parse::<toml::Table>().ok())
        .and_then(|t| t.get("package")?.get("name")?.as_str().map(String::from))
        .unwrap_or_else(|| "main".into());
    let target = std::env::var("CARGO_TARGET_DIR").map(std::path::PathBuf::from).unwrap_or_else(|_| p.root.join("target"));
    let mut c = Command::new(target.join("debug").join(&name));
    c.current_dir(&p.root);
    if piped {
        c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    }
    c.spawn()
}
