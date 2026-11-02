//! One compile cycle: your code → Claude → verified vocabulary → build.
//!
//! Every cycle is recorded in the git journal (see `git.rs`): first your save
//! (authored by you), then Claude's result (authored by claude). That record
//! drives regeneration scope, line provenance, undo and implement history.

use crate::claude;
use crate::events::{Event, Sink};
use crate::git::{Git, Who};
use crate::items;
use crate::project::{Decision, Lang, Project, Status, Suggestion, wiring_only};
use crate::provenance;
use anyhow::{Result, bail};
use std::collections::BTreeMap;
use std::process::{Child, Command, Stdio};

/// Subject of the commit that keeps your reformatted lines yours.
pub const REFORMAT: &str = "your lines, reformatted";

#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    /// Shape the vocabulary as a skimmable prototype with mocked details.
    Prototype,
    /// Turn the prototype into the real implementation (optionally one file).
    /// `item` narrows it to one item (and its parts), e.g. `Mode::Windowed`.
    Implement { file: Option<String>, item: Option<String> },
}

#[derive(Debug, Clone)]
pub struct Options {
    /// Run Claude even if the code compiles and nothing changed.
    pub force: bool,
    /// Extra instruction from the user for this cycle only.
    pub note: Option<String>,
    pub mode: Mode,
}

impl Default for Options {
    fn default() -> Self {
        Self { force: false, note: None, mode: Mode::Prototype }
    }
}

/// Check that the project compiles. `Err` means the toolchain itself is
/// missing: that is not something Claude can fix, so no cycle should start.
pub fn check(project: &Project) -> Result<(bool, String)> {
    match project.lang {
        Lang::Rust => {
            let o = run_tool(project, "cargo", &["check", "--quiet", "--message-format", "short"])?;
            Ok((o.status.success(), String::from_utf8_lossy(&o.stderr).trim().to_string()))
        }
        Lang::Java => mvn_compile(project),
    }
}

/// Run a toolchain command in the project. `Err` only if the tool is not installed.
fn run_tool(project: &Project, tool: &str, args: &[&str]) -> Result<std::process::Output> {
    match Command::new(tool).args(args).current_dir(&project.root).output() {
        Ok(o) => Ok(o),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!("`{tool}` was not found on PATH"),
        Err(e) => bail!("could not run `{tool}`: {e}"),
    }
}

/// `mvn -q compile`. Maven reports compile errors on stdout with absolute
/// paths; keep the useful lines, with paths relative to the project.
fn mvn_compile(project: &Project) -> Result<(bool, String)> {
    let o = run_tool(project, "mvn", &["-q", "compile"])?;
    let all = format!("{}\n{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    let roots: Vec<String> = [Some(project.root.clone()), project.root.canonicalize().ok()].into_iter().flatten().map(|r| format!("{}/", r.display())).collect();
    // Maven prints javac's report twice (raw, then as `[ERROR]` lines) and pads it with help text.
    let noise = ["[ERROR] [Help", "[ERROR] -> [Help", "[ERROR] For more", "[ERROR] Re-run", "[ERROR] To see", "[ERROR] Failed to execute goal", "[ERROR] COMPILATION ERROR", "[ERROR] -----"];
    let mut seen = std::collections::HashSet::new();
    let text: Vec<String> = all
        .lines()
        .filter(|l| l.starts_with('[') && l.trim_end() != "[ERROR]" && !noise.iter().any(|n| l.starts_with(n)))
        .map(|l| roots.iter().fold(l.to_string(), |l, r| l.replace(r.as_str(), "")))
        .filter(|l| seen.insert(l.clone()))
        .collect();
    Ok((o.status.success(), text.join("\n")))
}

/// Item path -> hash for Claude-owned source files at a snapshot.
fn items_at(p: &Project, git: &Git, sha: &str, files: &[String]) -> BTreeMap<String, String> {
    let srcs: BTreeMap<String, String> = files.iter().filter(|f| p.lang.is_src(f) && !p.is_protected(f)).filter_map(|f| git.show(sha, f).map(|s| (f.clone(), s))).collect();
    items::hash_map(p.lang, &srcs)
}

fn numbered(src: &str) -> String {
    src.lines().enumerate().map(|(i, l)| format!("{:>4} | {l}\n", i + 1)).collect()
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

/// Make sure a journal exists. A fresh journal starts with your files
/// (authored by you) followed by everything else (authored by claude).
pub fn ensure_journal(p: &Project, git: &Git) -> Result<String> {
    if let Some(j) = git.journal() {
        return Ok(j);
    }
    // A new branch: fork from the journal of the branch it came from.
    if let Some(src) = fork_source(git) {
        git.set_ref(&git.ref_name("journal"), &src.sha)?;
        let last = match &src.remote {
            None => git.resolve(&git.ref_for("last", &src.branch)),
            Some(r) => git.resolve(&format!("refs/wish-remote/{r}/{}", git.ref_for("last", &src.branch).trim_start_matches("refs/wish/"))),
        };
        if let Some(l) = last {
            git.set_ref(&git.ref_name("last"), &l)?;
        }
        return Ok(src.sha);
    }
    let mut mine = p.cfg.protected.files.clone();
    mine.push("wish.toml".into());
    let human = git.commit(&git.tree(Some(&mine))?, &[], Who::Human, "save: your files")?;
    let all = git.commit(&git.tree(None)?, &[&human], Who::Claude, "baseline: existing project")?;
    git.set_ref(&git.ref_name("journal"), &all)?;
    // Generated code already exists: treat it as the last green state, so the
    // next cycle regenerates for your edits instead of starting from scratch.
    if git.last().is_none() && p.src_files().iter().any(|f| !p.is_protected(f)) {
        git.set_ref(&git.ref_name("last"), &all)?;
    }
    Ok(all)
}

/// The journal a new branch forks from: the one whose code shares the most
/// recent history with yours (ties go to the same branch name, e.g. a
/// teammate's `master` journal fetched by `wish sync`).
fn fork_source(git: &Git) -> Option<crate::git::JournalRef> {
    let head = git.code_head()?;
    git.journals()
        .into_iter()
        .filter(|j| !(j.remote.is_none() && j.branch == git.branch()))
        .filter_map(|j| {
            let tip = git.code_tip(&j)?;
            let t = git.merge_base_time(&head, &tip);
            (t > 0).then(|| ((t, (j.branch == git.branch()) as u8), j))
        })
        .max_by_key(|(k, _)| *k)
        .map(|(_, j)| j)
}

/// Journals whose code you have merged into yours (a feature branch you
/// merged, or a teammate's pushed work you pulled) but whose wish history
/// isn't part of this branch's journal yet.
pub fn journals_to_merge(git: &Git, tip: &str) -> Vec<crate::git::JournalRef> {
    let Some(head) = git.code_head() else { return vec![] };
    git.journals()
        .into_iter()
        .filter(|j| !(j.remote.is_none() && j.branch == git.branch()))
        .filter(|j| git.code_tip(j).is_some_and(|t| git.is_ancestor(&t, &head)))
        .filter(|j| !git.is_ancestor(&j.sha, tip))
        .collect()
}

/// Record the current working tree as a save (authored by you), unless
/// nothing changed since the journal tip. If you merged other branches' code,
/// their journals are merged in as well. Returns the new tip.
pub fn record_save(git: &Git, tip: &str, msg: &str) -> Result<String> {
    let merges = journals_to_merge(git, tip);
    if !merges.is_empty() {
        let tree = git.tree(None)?;
        let names: Vec<String> = merges.iter().map(|j| match &j.remote { Some(r) => format!("{r}/{}", j.branch), None => j.branch.clone() }).collect();
        let mut parents: Vec<&str> = vec![tip];
        parents.extend(merges.iter().map(|j| j.sha.as_str()));
        let body = format!("merge: {}\n\nYour code now contains {}; its wish history joins this branch's.\n", names.join(", "), names.join(", "));
        let m = git.commit(&tree, &parents, Who::Human, &body)?;
        git.set_ref(&git.ref_name("journal"), &m)?;
        return Ok(m);
    }
    let tree = git.tree(None)?;
    if git.tree_of(tip).as_deref() == Some(tree.as_str()) {
        return Ok(tip.to_string());
    }
    let files: Vec<String> = git.changed(tip, &tree).unwrap_or_default().into_iter().map(|(_, f)| f).collect();
    let body = if files.is_empty() { msg.to_string() } else { format!("{msg}\n\nFiles: {}\n", files.join(", ")) };
    let save = git.commit(&tree, &[tip], Who::Human, &body)?;
    git.set_ref(&git.ref_name("journal"), &save)?;
    Ok(save)
}

/// Your lines inside Claude-owned source files at snapshot `sha` (the journal tip).
fn human_lines_in_owned(p: &Project, git: &Git, sha: &str) -> BTreeMap<String, Vec<String>> {
    let mut m = BTreeMap::new();
    for f in p.src_files().into_iter().filter(|f| !p.is_protected(f)) {
        let Some(src) = git.show(sha, &f) else { continue };
        if let Some(whos) = provenance::of(git, &f, &src) {
            let lines = provenance::human_lines(&whos, &src);
            if !lines.is_empty() {
                m.insert(f, lines);
            }
        }
    }
    m
}

/// The journal as a readable story, oldest first, capped at `budget` bytes
/// (older entries are dropped first).
pub fn history_story(git: &Git, budget: usize) -> String {
    let Some(tip) = git.journal() else { return String::new() };
    let mut parts: Vec<String> = Vec::new();
    for e in git.history(&tip) {
        let mut s = String::new();
        for side in &e.side {
            s.push_str(&format!("- (merged in another branch's history, last: {})\n", git.subject(side)));
        }
        match (e.who, &e.parent) {
            (Who::Human, Some(parent)) => {
                let d = git.diff(parent, &e.sha, &[]).unwrap_or_default();
                let d: String = d.lines().filter(|l| !l.starts_with("index ")).collect::<Vec<_>>().join("\n");
                s.push_str(&format!("### you: {}\n```diff\n{}\n```\n", e.subject, truncate(&d, 3000)));
            }
            (Who::Human, None) => s.push_str(&format!("### you: {}\n", e.subject)),
            (Who::Claude, _) if e.subject.starts_with("undo") => s.push_str(&format!("### you undid Claude's cycle: {}\n", e.subject.trim_start_matches("undo: "))),
            (Who::Claude, _) => s.push_str(&format!("### claude: {}\n", e.subject)),
        }
        parts.push(s);
    }
    // newest first from git; keep the newest that fit, then print oldest first
    let mut kept: Vec<String> = Vec::new();
    let mut used = 0;
    for p in &parts {
        if used + p.len() > budget {
            break;
        }
        used += p.len();
        kept.push(p.clone());
    }
    let dropped = parts.len() - kept.len();
    kept.reverse();
    let mut out = String::new();
    if dropped > 0 {
        out.push_str(&format!("({dropped} older entries omitted)\n"));
    }
    out.push_str(&kept.concat());
    let attic = git.attic();
    if !attic.is_empty() {
        out.push_str("### abandoned attempts (reverted, never kept)\n");
        for a in attic.iter().take(10) {
            out.push_str(&format!("- {a}\n"));
        }
    }
    out
}

struct PromptInput<'a> {
    diff: &'a Option<String>,
    owned_edits: &'a str,
    human_owned: &'a BTreeMap<String, Vec<String>>,
    check_errors: &'a str,
    opts: &'a Options,
    history: Option<String>,
}

fn build_prompt(p: &Project, i: &PromptInput) -> String {
    let mut s = String::new();
    s.push_str("# The user's code (protected)\n\n");
    for (f, src) in p.protected_files() {
        s.push_str(&format!("## {f}\n```{}\n{}```\n\n", p.lang.name(), numbered(&src)));
    }
    match i.diff {
        None => s.push_str("# What changed\nFirst cycle: nothing has been generated yet. Build the vocabulary from scratch.\n\n"),
        Some(d) if d.is_empty() && i.owned_edits.is_empty() => s.push_str("# What changed\nThe user's code did not change since the last cycle. Only fix what is broken.\n\n"),
        Some(d) => {
            if !d.is_empty() {
                s.push_str(&format!("# What the user changed in their files since the last cycle (your regeneration scope)\n```diff\n{d}```\n\n"));
            }
        }
    }
    if !i.owned_edits.is_empty() {
        s.push_str(&format!("# What the user changed inside generated files (wishes; fulfil them)\n```diff\n{}```\n\n", truncate(i.owned_edits, 8000)));
    }
    if !i.human_owned.is_empty() {
        s.push_str("# Lines the user wrote inside generated files (keep verbatim, make them true)\n");
        for (f, lines) in i.human_owned {
            s.push_str(&format!("## {f}\n"));
            for l in lines {
                s.push_str(&format!("    {l}\n"));
            }
        }
        s.push('\n');
    }
    if let Some(n) = &i.opts.note {
        s.push_str(&format!("# Note from the user for this cycle\n{n}\n\n"));
    }
    if let Mode::Implement { file, item } = &i.opts.mode {
        s.push_str(&format!("# Scope\n{}\n\n", match (item, file) {
            (Some(it), _) => format!("Implement only `{it}`. Every other prototype item stays mocked, exactly as it is (keep their `# Mocked` sections). Touch other code only where `{it}` needs it."),
            (None, Some(f)) => format!("Implement only the prototype items in {f}. Leave other files as they are unless {f} needs something from them."),
            (None, None) => "Implement every prototype (`# Mocked`) item in the project.".into(),
        }));
        let mocked: Vec<String> = p
            .owned_files()
            .iter()
            .filter(|(f, _)| p.lang.is_src(f) && file.as_ref().is_none_or(|x| x == *f))
            .flat_map(|(f, src)| items::parse(p.lang, src).unwrap_or_default().into_iter().filter_map(move |it| it.mocked().map(|m| format!("- `{}` ({f}): {}", it.path, m.join("; ")))))
            .collect();
        if !mocked.is_empty() {
            s.push_str(&format!("# Prototype items still mocked\n{}\n\n", mocked.join("\n")));
        }
        if let Some(h) = &i.history {
            s.push_str(&format!("# History: how this prototype came about (oldest first)\n{h}\n\n"));
        }
    }
    let owned: Vec<_> = p.owned_files().into_keys().collect();
    s.push_str(&format!("# Generated files\n{}\n\n", if owned.is_empty() { "(none yet)".into() } else { owned.join(", ") }));
    if !i.check_errors.is_empty() {
        s.push_str(&format!("# Current `{}` output\n```\n{}\n```\n\n", p.lang.check_cmd(), truncate(i.check_errors, 6000)));
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

/// Run one cycle. Returns Ok(true) if the project compiles at the end.
pub fn compile(p: &mut Project, sink: &mut dyn Sink, opts: &Options) -> Result<bool> {
    let git = Git::open(&p.root)?;
    let Some(_lock) = git.try_lock() else {
        sink.emit(Event::Done { ok: false, summary: "another wish process is already running a cycle for this project".into() });
        return Ok(false);
    };
    let tip = ensure_journal(p, &git)?;
    let pre = record_save(&git, &tip, "save")?;
    let last = git.last();
    let implement = matches!(opts.mode, Mode::Implement { .. });

    // What you changed since the last green cycle: in your files, and inside generated files.
    let diff = match &last {
        Some(l) => Some(git.diff(l, &pre, &p.cfg.protected.files)?),
        None => None,
    };
    let owned_src: Vec<String> = p.src_files().into_iter().filter(|f| !p.is_protected(f)).collect();
    // Only this save's edits: between the previous journal tip and now, only you edited.
    let owned_edits = if owned_src.is_empty() || pre == tip { String::new() } else { git.diff(&tip, &pre, &owned_src)? };

    sink.emit(Event::Status { msg: "checking".into() });
    let (ok, errors) = match check(p) {
        Ok(r) => r,
        Err(e) => {
            sink.emit(Event::Done { ok: false, summary: format!("{e:#}") });
            return Ok(false);
        }
    };
    let quiet = !opts.force && opts.note.is_none() && !implement;
    // You merged other branches' code (and with it, their wish history): if it
    // compiles, the merge is simply the new green state; nothing to regenerate.
    let merged = pre != tip && git.subject(&pre).starts_with("merge:");
    if ok && merged && quiet {
        git.set_ref(&git.ref_name("last"), &pre)?;
        sink.emit(Event::Check { ok: true, errors: String::new() });
        sink.emit(Event::Done { ok: true, summary: format!("recorded {}", git.subject(&pre)) });
        return Ok(true);
    }
    if ok && last.is_none() && quiet {
        // Compiles but has no green cycle yet (e.g. an existing project): take it as the baseline.
        git.set_ref(&git.ref_name("last"), &pre)?;
        sink.emit(Event::Check { ok: true, errors: String::new() });
        sink.emit(Event::Done { ok: true, summary: "project already compiles: recorded it as the baseline".into() });
        return Ok(true);
    }
    if ok && quiet && diff.as_deref() == Some("") && owned_edits.is_empty() {
        sink.emit(Event::Check { ok: true, errors: String::new() });
        sink.emit(Event::Done { ok: true, summary: "nothing changed; vocabulary is up to date".into() });
        return Ok(true);
    }

    let human_owned = human_lines_in_owned(p, &git, &pre);
    // Your edits in this save: recorded in the cycle commit as what triggered it.
    let triggered_by = if pre == tip { vec![] } else { crate::journal::trigger_lines(&git.diff(&tip, &pre, &[format!("*.{}", p.lang.ext())]).unwrap_or_default(), 20) };
    let history = if implement {
        git.set_ref(&git.ref_name("prototype"), &pre)?;
        Some(history_story(&git, 20_000))
    } else {
        None
    };
    sink.emit(Event::Status { msg: match (&opts.mode, &diff) {
        (Mode::Implement { item: Some(it), .. }, _) => format!("implementing `{it}`"),
        (Mode::Implement { file: Some(f), .. }, _) => format!("implementing the prototype in {f}"),
        (Mode::Implement { .. }, _) => "implementing the prototype".into(),
        (_, None) => "first cycle: asking Claude to build the prototype".into(),
        (_, Some(d)) if d.is_empty() && owned_edits.is_empty() => "asking Claude to fix the build".into(),
        _ => "your code changed: asking Claude to regenerate".into(),
    }});
    let prompt = build_prompt(p, &PromptInput { diff: &diff, owned_edits: &owned_edits, human_owned: &human_owned, check_errors: if ok { "" } else { &errors }, opts, history });
    let rules = claude::rules(p.lang, implement);
    let report = match claude::run(p, &prompt, &rules, sink) {
        Ok(r) => r,
        Err(e) => {
            sink.emit(Event::Done { ok: false, summary: format!("{e:#}") });
            return Ok(false);
        }
    };
    let post = git.snapshot(&[&pre], Who::Claude, "attempt")?;

    // ---- verify (backstop for the hook) ----
    let mut violations = p.verify_adopted();
    for f in &p.cfg.protected.files {
        let old = git.show(&pre, f).unwrap_or_default();
        let new = git.show(&post, f).unwrap_or_default();
        match wiring_only(p.lang, &old, &new) {
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
    for (f, lines) in &human_owned {
        let new = git.show(&post, f).unwrap_or_default();
        for l in provenance::missing(lines, &new) {
            violations.push(format!("{f}: Claude removed or changed your line `{l}`"));
        }
    }
    if implement {
        violations.extend(contract_changes(p, &git, &pre, &post));
    }
    if !violations.is_empty() {
        for v in &violations {
            sink.emit(Event::Violation { msg: v.clone() });
        }
        git.restore(&pre, &post)?;
        // Keep the attempt in the attic: reachable for history, out of blame.
        let attempt = git.commit(&git.tree_of(&post).unwrap_or_default(), &[&pre], Who::Claude, &format!("reverted attempt: {}", violations[0]))?;
        git.to_attic(&attempt)?;
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
    let total = items::hash_map(p.lang, &p.owned_files().into_iter().filter(|(k, _)| p.lang.is_src(k)).collect::<BTreeMap<_, _>>()).len();
    let untouched = total.saturating_sub(added.len() + changed.len());
    sink.emit(Event::Changes { added, changed, removed, untouched });

    // ---- record decisions & suggestions ----
    let mut ds = p.decisions();
    let mut decision_notes = Vec::new();
    for d in report.decisions {
        let id = ds.iter().map(|d| d.id).max().unwrap_or(0) + 1;
        decision_notes.push(format!("{id} (proposed) {}: {}", d.title, d.choice));
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
    if implement {
        for (f, src) in p.owned_files().iter().filter(|(f, _)| p.lang.is_src(f)) {
            if let Mode::Implement { file: Some(only), .. } = &opts.mode {
                if only != f {
                    continue;
                }
            }
            let target = if let Mode::Implement { item: Some(t), .. } = &opts.mode { Some(t.clone()) } else { None };
            for it in items::parse(p.lang, src).unwrap_or_default().into_iter().filter(|i| i.mocked().is_some()) {
                if target.as_ref().is_some_and(|t| &it.path != t && !it.path.starts_with(&format!("{t}::"))) {
                    continue;
                }
                sink.emit(Event::Warning { msg: format!("`{}` ({f}) is still marked `# Mocked`", it.path) });
            }
        }
    }

    let (ok, errors) = check(p).unwrap_or_else(|e| (false, format!("{e:#}")));
    sink.emit(Event::Check { ok, errors });
    let label = match (&opts.mode, ok) {
        (Mode::Implement { .. }, true) => "implemented",
        (Mode::Prototype, true) => "cycle ok",
        (_, false) => "cycle failed",
    };
    let message = crate::journal::CycleMessage {
        label: label.into(),
        summary: report.summary.clone(),
        why: report.changes.iter().map(|c| crate::journal::Why::new(&c.item, &c.why)).collect(),
        triggered_by,
        decisions: decision_notes,
        mode: if implement { "implement".into() } else { "prototype".into() },
    };
    // Your lines that Claude reformatted or expanded (`pub enum Color {}` → `{ … }`)
    // are committed as yours first, so blame keeps them yours and only what
    // Claude added around them is attributed to Claude.
    let mut base = pre.clone();
    let mut reform = Vec::new();
    for f in human_owned.keys() {
        let (Some(old), Some(new)) = (git.show(&pre, f), p.read(f)) else { continue };
        if let Some(whos) = provenance::of(&git, f, &old) {
            if let Some(mid) = provenance::reformatted(&old, &whos, &new) {
                reform.push((f.clone(), mid));
            }
        }
    }
    if !reform.is_empty() {
        let files: Vec<&str> = reform.iter().map(|(f, _)| f.as_str()).collect();
        let tree = git.tree_with(&pre, &reform)?;
        base = git.commit(&tree, &[&pre], Who::Human, &format!("{REFORMAT}\n\nClaude expanded or reformatted lines you wrote; they stay yours.\n\nFiles: {}\n", files.join(", ")))?;
    }
    let fin = git.snapshot(&[&base], Who::Claude, &message.render())?;
    git.set_ref(&git.ref_name("journal"), &fin)?;
    if ok {
        git.set_ref(&git.ref_name("last"), &fin)?;
    }
    sink.emit(Event::Done { ok, summary: report.summary });
    Ok(ok)
}

/// Public signatures and type definitions that changed between two snapshots.
fn contract_changes(p: &Project, git: &Git, pre: &str, post: &str) -> Vec<String> {
    let mut out = Vec::new();
    for f in p.src_files().into_iter().filter(|f| !p.is_protected(f)) {
        let Some(old) = git.show(pre, &f) else { continue };
        let new = git.show(post, &f).and_then(|s| items::parse(p.lang, &s).ok()).unwrap_or_default();
        for it in items::parse(p.lang, &old).unwrap_or_default() {
            let Some(sig) = &it.sig_hash else { continue };
            match new.iter().find(|n| n.path == it.path) {
                Some(n) if n.sig_hash.as_ref() == Some(sig) => {}
                Some(_) => out.push(format!("{f}: implementing changed the public contract of `{}`", it.path)),
                None => {
                    // moved to another file is fine if the contract survived
                    let elsewhere = p.src_files().iter().filter_map(|g| git.show(post, g)).flat_map(|s| items::parse(p.lang, &s).unwrap_or_default()).any(|n| n.path == it.path && n.sig_hash.as_ref() == Some(sig));
                    if !elsewhere {
                        out.push(format!("{f}: implementing removed public `{}`", it.path));
                    }
                }
            }
        }
    }
    out
}

pub fn build(p: &Project) -> (bool, String) {
    let built = match p.lang {
        Lang::Rust => run_tool(p, "cargo", &["build", "--quiet"]).map(|o| (o.status.success(), String::from_utf8_lossy(&o.stderr).to_string())),
        // Maven's compile is the build: the app runs from target/classes.
        Lang::Java => mvn_compile(p),
    };
    built.unwrap_or_else(|e| (false, format!("{e:#}")))
}

/// Launch the app as the live preview.
pub fn spawn_preview(p: &Project, piped: bool) -> std::io::Result<Child> {
    // Run the built program directly: killing it then really stops the app
    // (killing `cargo run` or `mvn exec` would orphan it), and the build
    // tool's own output stays out.
    let mut c = match p.lang {
        Lang::Rust => {
            let name = p.read("Cargo.toml").and_then(|t| t.parse::<toml::Table>().ok()).and_then(|t| t.get("package")?.get("name")?.as_str().map(String::from)).unwrap_or_else(|| "main".into());
            let target = std::env::var("CARGO_TARGET_DIR").map(std::path::PathBuf::from).unwrap_or_else(|_| p.root.join("target"));
            Command::new(target.join("debug").join(&name))
        }
        Lang::Java => {
            let mut c = Command::new("java");
            c.args(["-cp", "target/classes", &p.main_class()]);
            c
        }
    };
    c.current_dir(&p.root);
    if piped {
        c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    }
    c.spawn()
}
