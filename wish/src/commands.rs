//! The user's commands. Each returns text so the CLI and `wish dev` can both
//! show it. Claude is blocked from running any of these by the hook.

use crate::items;
use crate::project::{Adopted, Project, Status};
use anyhow::{Result, bail};
use std::fmt::Write;

pub fn init() -> Result<String> {
    let cwd = std::env::current_dir()?;
    if !cwd.join("Cargo.toml").is_file() {
        bail!("run `wish init` inside a cargo project");
    }
    if cwd.join("wish.toml").exists() {
        bail!("wish.toml already exists");
    }
    let in_git = std::process::Command::new("git").args(["rev-parse", "--git-dir"]).current_dir(&cwd).output().is_ok_and(|o| o.status.success());
    if !in_git {
        std::process::Command::new("git").arg("init").arg("-q").current_dir(&cwd).status()?;
    }
    let p = Project { root: cwd, cfg: Default::default() };
    p.save()?;
    Ok("created wish.toml: src/main.rs is protected. Write your wishes there, then run `wish dev`.".into())
}

pub fn adopt(p: &mut Project, path: &str) -> Result<String> {
    let (file, item) = p.find_item(path)?;
    if p.is_protected(&file) {
        bail!("`{path}` is in your own file {file}; it is already protected");
    }
    p.cfg.adopted.insert(path.to_string(), Adopted { file: file.clone(), hash: item.hash });
    p.save()?;
    Ok(format!("adopted `{path}` ({file}). It is now yours: Claude can call it but not change it."))
}

pub fn unadopt(p: &mut Project, path: &str) -> Result<String> {
    if p.cfg.adopted.remove(path).is_none() {
        bail!("`{path}` is not adopted");
    }
    p.save()?;
    Ok(format!("`{path}` is Claude's again."))
}

pub fn items(p: &Project) -> Result<String> {
    let mut s = String::new();
    for f in p.rust_files() {
        let Some(src) = p.read(&f) else { continue };
        let owner = if p.is_protected(&f) { "yours" } else { "claude" };
        writeln!(s, "{f} ({owner})")?;
        match items::parse(&src) {
            Ok(list) => {
                for i in list {
                    let mark = if p.cfg.adopted.contains_key(&i.path) { "★ adopted" } else { "" };
                    writeln!(s, "  {:<12} {:<36} {mark}", i.kind, i.path)?;
                }
            }
            Err(e) => writeln!(s, "  (does not parse: {e})")?,
        }
    }
    Ok(s)
}

pub fn decisions(p: &Project) -> Result<String> {
    let mut s = String::new();
    let ds = p.decisions();
    writeln!(s, "Architecture & dependencies (DECISIONS.md):")?;
    if ds.is_empty() {
        writeln!(s, "  (none)")?;
    }
    for d in &ds {
        let st = match d.status {
            Status::Proposed => "proposed",
            Status::Accepted => "accepted",
            Status::Rejected => "rejected",
        };
        writeln!(s, "  {:>3} [{st}] {}: {}", d.id, d.title, d.choice)?;
        if !d.crates.is_empty() {
            writeln!(s, "      crates: {}", d.crates.join(", "))?;
        }
        writeln!(s, "      why: {}", d.why)?;
        if !d.alternatives.is_empty() {
            writeln!(s, "      alternatives: {}", d.alternatives.join("; "))?;
        }
        if let Some(r) = &d.reason {
            writeln!(s, "      rejected because: {r}")?;
        }
    }
    writeln!(s, "\nLocal decisions (doc comments):")?;
    let mut any = false;
    for (f, src) in p.owned_files() {
        let Ok(list) = items::parse(&src) else { continue };
        for i in list {
            let local = items::local_decisions(&i.docs);
            if !local.is_empty() {
                any = true;
                writeln!(s, "  {} ({f})", i.path)?;
                for l in local {
                    writeln!(s, "    - {l}")?;
                }
            }
        }
    }
    if !any {
        writeln!(s, "  (none)")?;
    }
    Ok(s)
}

pub fn decide(p: &Project, id: u32, accept: bool, reason: Option<String>) -> Result<String> {
    let mut ds = p.decisions();
    let Some(d) = ds.iter_mut().find(|d| d.id == id) else { bail!("no decision {id}") };
    d.status = if accept { Status::Accepted } else { Status::Rejected };
    d.reason = if accept { None } else { Some(reason.unwrap_or_else(|| "no reason given".into())) };
    let msg = if accept {
        format!("decision {id} accepted: \"{}\" is now binding.", d.title)
    } else {
        format!("decision {id} rejected. The next compile reworks the code without it (press `f` in `wish dev`, or run `wish compile --force`).")
    };
    p.save_decisions(&ds)?;
    Ok(msg)
}

pub fn suggestions(p: &Project) -> Result<String> {
    let pending: Vec<_> = p.suggestions().into_iter().filter(|s| s.status == Status::Proposed).collect();
    if pending.is_empty() {
        return Ok("no pending suggestions".into());
    }
    let mut s = String::new();
    for x in pending {
        writeln!(s, "\x1b[1m{} · {}\x1b[0m: {}", x.id, x.file, x.reason)?;
        for l in x.replace.lines() {
            writeln!(s, "\x1b[31m  - {l}\x1b[0m")?;
        }
        for l in x.with.lines() {
            writeln!(s, "\x1b[32m  + {l}\x1b[0m")?;
        }
    }
    s.push_str("accept with `wish accept <id>`, reject with `wish reject <id>`");
    Ok(s)
}

pub fn accept(p: &mut Project, id: u32) -> Result<String> {
    let mut ss = p.suggestions();
    let Some(x) = ss.iter_mut().find(|s| s.id == id && s.status == Status::Proposed) else { bail!("no pending suggestion {id}") };
    let Some(src) = p.read(&x.file) else { bail!("{} not found", x.file) };
    if !src.contains(&x.replace) {
        bail!("suggestion {id} no longer applies: its text isn't in {} anymore. Reject it.", x.file);
    }
    let new = src.replacen(&x.replace, &x.with, 1);
    std::fs::write(p.root.join(&x.file), &new)?;
    // You approved this change, so re-lock any adopted items it touched.
    let mut relocked = Vec::new();
    if let Ok(list) = items::parse(&new) {
        for (path, a) in p.cfg.adopted.iter_mut().filter(|(_, a)| a.file == x.file) {
            if let Some(i) = list.iter().find(|i| &i.path == path) {
                if i.hash != a.hash {
                    a.hash = i.hash.clone();
                    relocked.push(path.clone());
                }
            }
        }
    }
    x.status = Status::Accepted;
    let file = x.file.clone();
    p.save_suggestions(&ss)?;
    p.save()?;
    let mut msg = format!("applied suggestion {id} to {file}.");
    if !relocked.is_empty() {
        msg.push_str(&format!(" Re-locked adopted: {}.", relocked.join(", ")));
    }
    Ok(msg)
}

pub fn reject(p: &Project, id: u32) -> Result<String> {
    let mut ss = p.suggestions();
    let Some(x) = ss.iter_mut().find(|s| s.id == id && s.status == Status::Proposed) else { bail!("no pending suggestion {id}") };
    x.status = Status::Rejected;
    p.save_suggestions(&ss)?;
    Ok(format!("rejected suggestion {id}."))
}

/// Roll Claude's side of the project back to the previous green cycle.
///
/// Restores everything except your protected files and `wish.toml` (your
/// adoptions and constraints), including decisions and suggestions recorded
/// by the undone cycle. Moves `refs/wish/…/last` back one step, so if your
/// lines changed in that cycle the next compile regenerates for them again.
pub fn undo(p: &Project) -> Result<String> {
    let git = crate::git::Git::open(&p.root)?;
    let last_ref = git.ref_name("last");
    let Some(last) = git.resolve(&last_ref) else { bail!("no cycles yet: nothing to undo") };
    let Some(prev) = git.resolve(&format!("{last}^")) else { bail!("the last cycle is the baseline: nothing to undo") };

    let now = git.snapshot(None, "wish: before undo")?;
    let keep = |path: &str| path != "wish.toml" && !p.is_protected(path);
    let restored = git.restore_where(&prev, &now, keep)?;

    // Never let an undo break something you adopted after that cycle.
    let broken = p.verify_adopted();
    if !broken.is_empty() {
        git.restore_where(&now, &git.snapshot(None, "wish: aborted undo")?, keep)?;
        bail!("undo would change adopted items, so nothing was changed:\n  {}\n(unadopt them first if you really want to undo)", broken.join("\n  "));
    }
    git.set_ref(&last_ref, &prev)?;

    let subject = git.subject(&last);
    let subject = subject.strip_prefix("wish cycle: ").unwrap_or(&subject);
    let short: String = subject.chars().take(90).collect();
    let mut msg = format!("undid cycle: {short}{}", if short.len() < subject.len() { "…" } else { "" });
    msg.push_str(&if restored.is_empty() { "\n  (no files differed)".to_string() } else { format!("\n  restored: {}", restored.join(", ")) });
    if !git.diff(&prev, &now, &p.cfg.protected.files)?.is_empty() {
        msg.push_str("\n  your code changed since that point: edit it, or compile to regenerate for it again");
    }
    Ok(msg)
}

/// Returns (ok, report).
pub fn check(p: &Project) -> (bool, String) {
    let mut s = String::new();
    let mut ok = true;
    for v in p.verify_adopted() {
        ok = false;
        let _ = writeln!(s, "✗ {v}");
    }
    for d in p.undecided_dependencies() {
        ok = false;
        let _ = writeln!(s, "✗ dependency `{d}` has no decision record");
    }
    let (cargo_ok, errors) = crate::engine::cargo_check(p);
    if !cargo_ok {
        ok = false;
        let _ = writeln!(s, "✗ cargo check failed\n{errors}");
    }
    if ok {
        s.push_str("✓ adopted items intact, every dependency decided, cargo check passes");
    }
    (ok, s)
}
