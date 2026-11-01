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
    crate::engine::ensure_journal(&p, &crate::git::Git::open(&p.root)?)?;
    let zed = p.root.join(".zed/settings.json");
    if !zed.exists() {
        std::fs::create_dir_all(zed.parent().unwrap())?;
        std::fs::write(&zed, ZED_SETTINGS)?;
    }
    let user = std::env::var("HOME").map(|h| std::path::PathBuf::from(h).join(".config/zed/settings.json")).ok();
    let has_rule = user.and_then(|u| std::fs::read_to_string(u).ok()).is_some_and(|t| t.contains("\"generated\""));
    if !has_rule {
        println!("note: to see generated code in blue, add wishlang's `semantic_token_rules` (see README) to your Zed user settings");
    }
    Ok("created wish.toml and .zed/settings.json: src/main.rs is protected. Write your wishes there, then run `wish dev`.".into())
}

/// Project settings for Zed: run `wish lsp` next to rust-analyzer, let
/// wishlang (not rust-analyzer) report not-yet-generated symbols, and show
/// inlay hints (wishlang's provenance markers) without type-hint clutter.
pub const ZED_SETTINGS: &str = include_str!("zed_settings.json");

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

/// Undo the last green cycle: reverse exactly the changes Claude made in it.
///
/// Everything you wrote stays, including lines you added to generated files
/// and edits made since. The cycle's decisions and suggestions go too. `last`
/// moves back to the previous green cycle, so your edits from then on are the
/// regeneration scope again on the next compile.
pub fn undo(p: &Project) -> Result<String> {
    use crate::git::{Git, Who};
    let git = Git::open(&p.root)?;
    let Some(last) = git.last() else { bail!("no cycles yet: nothing to undo") };
    let history = git.history(&last);
    let Some(cycle) = history.first().filter(|e| e.who == Who::Claude && !e.subject.starts_with("baseline")) else {
        bail!("the last green state is the baseline: nothing to undo")
    };
    let Some(mut save) = cycle.parent.clone() else { bail!("journal is malformed") };
    // If the cycle kept your reformatted lines in a commit of their own, undo that too.
    if git.subject(&save) == crate::engine::REFORMAT {
        if let Some(e) = git.history(&save).into_iter().next().and_then(|e| e.parent) {
            save = e;
        }
    }
    let prev = history.iter().skip(1).find(|e| e.who == Who::Claude && is_green(&e.subject)).map(|e| e.sha.clone());

    let tip = git.journal().unwrap_or(last.clone());
    let tip = crate::engine::record_save(&git, &tip, "save")?;
    let mut paths: Vec<String> = git.changed(&save, &last)?.into_iter().map(|(_, f)| f).filter(|f| f != "wish.toml" && !p.is_protected(f)).collect();
    paths.retain(|f| !f.starts_with("target/"));
    git.apply_between(&save, &last, &paths, true).map_err(|e| anyhow::anyhow!("can't undo cleanly, the files changed too much since that cycle:\n  {e:#}"))?;

    // Never let an undo break something you adopted after that cycle.
    let broken = p.verify_adopted();
    if !broken.is_empty() {
        git.apply_between(&save, &last, &paths, false)?;
        bail!("undo would change adopted items, so nothing was changed:\n  {}\n(unadopt them first if you really want to undo)", broken.join("\n  "));
    }
    let subject = cycle.subject.split_once(": ").map(|x| x.1).unwrap_or(&cycle.subject).to_string();
    let short: String = subject.chars().take(90).collect();
    let short = format!("{short}{}", if short.len() < subject.len() { "…" } else { "" });
    // Restored lines are Claude's earlier code, so the undo is authored by claude.
    let undo = git.snapshot(&[&tip], Who::Claude, &format!("undo: {short}"))?;
    git.set_ref(&git.ref_name("journal"), &undo)?;
    match prev {
        Some(prev) => git.set_ref(&git.ref_name("last"), &prev)?,
        None => {
            let _ = std::process::Command::new("git").args(["update-ref", "-d", &git.ref_name("last")]).current_dir(&p.root).status();
        }
    }
    let mut msg = format!("undid cycle: {short}");
    msg.push_str(&if paths.is_empty() { "\n  (no files differed)".to_string() } else { format!("\n  reverted: {}", paths.join(", ")) });
    msg.push_str("\n  your own lines were kept; compile to regenerate for them again");
    Ok(msg)
}

fn is_green(subject: &str) -> bool {
    subject.starts_with("cycle ok") || subject.starts_with("implemented") || subject.starts_with("baseline")
}

/// Implement the prototype (whole project, or one file).
pub fn implement_options(file: Option<String>, item: Option<String>) -> crate::engine::Options {
    crate::engine::Options { force: true, note: None, mode: crate::engine::Mode::Implement { file, item } }
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

/// Push this project's wish journals (every branch) so others can see the
/// provenance and reasoning behind generated code.
pub fn share(p: &Project, remote: &str) -> Result<String> {
    let git = crate::git::Git::open(&p.root)?;
    let out = git.push_journals(remote)?;
    Ok(format!("pushed wish journals to {remote}\n{out}\nothers get them with `wish sync`"))
}

/// Fetch others' wish journals. New branches' journals are adopted, behind
/// ones fast-forwarded; diverged ones are merged on your next save once your
/// code contains theirs.
pub fn sync(p: &Project, remote: &str) -> Result<String> {
    let git = crate::git::Git::open(&p.root)?;
    git.fetch_journals(remote)?;
    let mut lines = Vec::new();
    for j in git.journals().into_iter().filter(|j| j.remote.as_deref() == Some(remote)) {
        let local = git.resolve(&git.ref_for("journal", &j.branch));
        match local {
            None => {
                git.set_branch_journal(&j.branch, &j.sha)?;
                lines.push(format!("  {}: adopted", j.branch));
            }
            Some(l) if l == j.sha || git.is_ancestor(&j.sha, &l) => lines.push(format!("  {}: up to date", j.branch)),
            Some(l) if git.is_ancestor(&l, &j.sha) => {
                git.set_branch_journal(&j.branch, &j.sha)?;
                lines.push(format!("  {}: fast-forwarded", j.branch));
            }
            Some(_) => lines.push(format!("  {}: diverged; merged on your next save once your code contains theirs", j.branch)),
        }
        // Adopt their "last green" for branches you didn't have.
        let theirs_last = format!("refs/wish-remote/{remote}/{}", git.ref_for("last", &j.branch).trim_start_matches("refs/wish/"));
        if git.resolve(&git.ref_for("last", &j.branch)).is_none() {
            if let Some(l) = git.resolve(&theirs_last) {
                let _ = std::process::Command::new("git").args(["update-ref", &git.ref_for("last", &j.branch), &l]).current_dir(&p.root).status();
            }
        }
    }
    if lines.is_empty() {
        lines.push("  (no journals on the remote for this project)".into());
    }
    Ok(format!("synced wish journals from {remote}:\n{}", lines.join("\n")))
}
