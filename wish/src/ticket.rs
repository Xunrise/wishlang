//! Ticket context: which problems of a Jira ticket the generated code solves.
//!
//! wish never talks to Jira. An external workflow writes the ticket's notes
//! to `<main checkout>/.work/<KEY>/` (`intent.md`, `spec.md`); the key comes
//! from the branch name. On a ticket's first cycle Claude distils the notes
//! into `problems.json` (which you may edit), every cycle then tags each item
//! it generates with the problems it serves, and the journal keeps those tags.
//! [`trace`] reads them back: per problem, the items that serve it.

use crate::events::{Event, Sink};
use crate::git::{FileStat, Git};
use crate::journal::{CycleMessage, same_item};
use crate::project::Project;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const MAX_PROBLEMS: usize = 8;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Problem {
    pub id: String,
    pub text: String,
    /// Titles of the Gherkin scenarios covering the problem.
    #[serde(default)]
    pub scenarios: Vec<String>,
}

/// A ticket cycle in progress: the key and the problems Claude tags items with.
pub struct Ticket {
    pub key: String,
    pub problems: Vec<Problem>,
}

/// Problems each item serves, united over a ticket's cycles. An item listed
/// with no problems was generated for none of them.
pub type Problems = BTreeMap<String, BTreeSet<String>>;

/// The ticket key of a branch: `feature|fix|chore/<KEY>[-slug]` or `proto/<KEY>`.
pub fn key(branch: &str) -> Option<String> {
    let re = regex::Regex::new(r"^(?:(?:feature|fix|chore)/([A-Z][A-Z0-9]+-\d+)(?:-.*)?|proto/([A-Z][A-Z0-9]+-\d+))$").unwrap();
    let c = re.captures(branch)?;
    c.get(1).or(c.get(2)).map(|m| m.as_str().to_string())
}

/// `.work/<KEY>/` in the main checkout (the parent of the repository's common git dir), if it exists.
pub fn notes_dir(root: &Path, key: &str) -> Option<PathBuf> {
    let out = Command::new("git").args(["rev-parse", "--path-format=absolute", "--git-common-dir"]).current_dir(root).output().ok()?;
    let common = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    let dir = common.parent()?.join(".work").join(key);
    dir.is_dir().then_some(dir)
}

/// The ticket's problems: empty if `problems.json` does not exist yet, an error if it is broken.
pub fn problems(dir: &Path) -> Result<Vec<Problem>> {
    let path = dir.join("problems.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// 1 to 8 problems with unique ids `P1`, `P2`, ... and some text each.
pub fn validate(ps: &[Problem]) -> Result<()> {
    if ps.is_empty() || ps.len() > MAX_PROBLEMS {
        bail!("a ticket needs 1 to {MAX_PROBLEMS} problems, got {}", ps.len());
    }
    let id = regex::Regex::new(r"^P\d+$").unwrap();
    let mut seen = BTreeSet::new();
    for p in ps {
        if !id.is_match(&p.id) {
            bail!("problem id {:?} must look like P1", p.id);
        }
        if !seen.insert(&p.id) {
            bail!("problem id {} is used twice", p.id);
        }
        if p.text.trim().is_empty() {
            bail!("problem {} has no text", p.id);
        }
    }
    Ok(())
}

/// Validate and write `problems.json` (through a temporary file, so it is never half-written).
pub fn save_problems(dir: &Path, ps: &[Problem]) -> Result<()> {
    validate(ps)?;
    let tmp = dir.join("problems.json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(ps)? + "\n")?;
    std::fs::rename(&tmp, dir.join("problems.json"))?;
    Ok(())
}

/// First `# ` heading of `intent.md`, without the key.
fn title(dir: &Path, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("intent.md")).ok()?;
    let heading = text.lines().find_map(|l| l.strip_prefix("# "))?;
    Some(heading.trim().trim_start_matches(key).trim_start_matches(|c: char| c.is_whitespace() || "—–-:".contains(c)).to_string())
}

/// `intent.md` and `spec.md` of the ticket, in that order.
fn notes(dir: &Path) -> Vec<(&'static str, String)> {
    ["intent.md", "spec.md"].into_iter().filter_map(|f| Some((f, std::fs::read_to_string(dir.join(f)).ok()?))).collect()
}

/// The ticket of a branch, ready for a cycle: its problems (extracted from the
/// notes by one Claude call if `problems.json` does not exist yet). None when
/// the branch has no ticket or the notes aren't there; when the problems can't
/// be had, a warning says why and the cycle runs without ticket mode.
pub fn start(p: &Project, branch: &str, sink: &mut dyn Sink) -> Option<Ticket> {
    let key = key(branch)?;
    let dir = notes_dir(&p.root, &key)?;
    match ensure_problems(p, &dir, &key) {
        Ok(problems) => Some(Ticket { key, problems }),
        Err(e) => {
            sink.emit(Event::Warning { msg: format!("ticket {key}: {e:#}; running without ticket mode") });
            None
        }
    }
}

fn ensure_problems(p: &Project, dir: &Path, key: &str) -> Result<Vec<Problem>> {
    let existing = problems(dir)?;
    if !existing.is_empty() || dir.join("problems.json").exists() {
        validate(&existing).context("problems.json")?;
        return Ok(existing);
    }
    let notes = notes(dir);
    if notes.is_empty() {
        bail!("{} has neither intent.md nor spec.md to extract problems from", dir.display());
    }
    let extracted = extract(&claude_json(p, &extraction_prompt(key, &notes))?)?;
    save_problems(dir, &extracted)?;
    Ok(extracted)
}

fn extraction_prompt(key: &str, notes: &[(&str, String)]) -> String {
    let mut s = format!(
        "Below are the notes for ticket {key}. Extract the problems this ticket solves, as seen by the people who use the result: 2 to 6 short statements (one plain sentence each) with ids \"P1\", \"P2\", ... Add to each the titles of the Gherkin scenarios that cover it (an empty list if there are none).\n\n"
    );
    for (name, text) in notes {
        s.push_str(&format!("## {name}\n{text}\n\n"));
    }
    s
}

fn extraction_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"problems": {"type": "array", "items": {
            "type": "object",
            "properties": {
                "id": {"type": "string", "description": "P1, P2, ..."},
                "text": {"type": "string", "description": "One plain sentence"},
                "scenarios": {"type": "array", "items": {"type": "string"}}
            },
            "required": ["id", "text", "scenarios"]
        }}},
        "required": ["problems"]
    })
}

/// One structured Claude call; returns the CLI's JSON result.
fn claude_json(p: &Project, prompt: &str) -> Result<Value> {
    let mut cmd = p.command(p.var("WISH_CLAUDE").unwrap_or_else(|| "claude".into()));
    cmd.current_dir(&p.root).env(crate::session::IN_CYCLE, "1").args(["-p", "--output-format", "json"]).args(["--json-schema", &extraction_schema().to_string()]);
    if let Some(model) = p.var("WISH_MODEL") {
        cmd.args(["--model", &model]);
    }
    let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().context("starting `claude` to extract the problems")?;
    child.stdin.take().unwrap().write_all(prompt.as_bytes())?;
    let out = child.wait_with_output()?;
    serde_json::from_slice(&out.stdout).with_context(|| format!("claude returned no result: {}", String::from_utf8_lossy(&out.stderr).trim()))
}

/// The validated problems of a Claude result.
fn extract(result: &Value) -> Result<Vec<Problem>> {
    if result["is_error"] == true {
        bail!("claude reported an error: {}", result["result"].as_str().unwrap_or("unknown"));
    }
    let ps: Vec<Problem> = serde_json::from_value(result["structured_output"]["problems"].clone()).context("claude returned no problem list")?;
    validate(&ps).context("claude's problem list")?;
    Ok(ps)
}

/// Prompt section naming the problems.
pub fn prompt_section(t: &Ticket) -> String {
    let mut s = format!("# Ticket {}\nProblems this work should solve:\n", t.key);
    for p in &t.problems {
        s.push_str(&format!("- {}: {}\n", p.id, p.text));
    }
    s.push_str("Every entry in the report's `changes` must carry `problems`: the ids above that this item serves. Use [] when it serves none of them; never invent an id.\n\n");
    s
}

// ---- tracing ----------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct Reason {
    pub sha: String,
    pub when: String,
    pub why: String,
}

/// A generated item, where it lives now, and why each cycle made it.
#[derive(Debug, Clone, Serialize)]
pub struct TraceItem {
    pub item: String,
    pub file: String,
    pub line: usize,
    pub reasons: Vec<Reason>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TraceProblem {
    pub id: String,
    pub text: String,
    pub scenarios: Vec<String>,
    pub items: Vec<TraceItem>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Trace {
    pub key: String,
    pub title: Option<String>,
    /// intent.md and spec.md.
    pub body: String,
    pub notes_dir: Option<String>,
    pub problems: Vec<TraceProblem>,
    /// Items generated in a ticket cycle that serve none of the problems.
    pub untraced: Vec<TraceItem>,
    pub cycles: usize,
    /// The ticket's latest cycle, and the commit before its first one.
    pub head: Option<String>,
    pub base: Option<String>,
    /// Files changed over the whole ticket.
    pub files: Vec<FileStat>,
    #[serde(skip)]
    pub union: Problems,
    pub branch: String,
    /// The project directory of the checkout that has the branch, if any (files open from there).
    #[serde(skip)]
    pub checkout: Option<PathBuf>,
}

/// Unite the problems each item served over cycles (a later cycle adds problems, never removes them).
pub fn union(cycles: &[CycleMessage]) -> Problems {
    let mut out = Problems::new();
    for m in cycles {
        for w in &m.why {
            out.entry(w.item.clone()).or_default().extend(w.problems.iter().cloned());
        }
    }
    out
}

/// The problems recorded for an item, over every journal path that names it (see `same_item`).
pub fn problems_of(u: &Problems, item: &str) -> Option<BTreeSet<String>> {
    let mut hits = u.iter().filter(|(name, _)| same_item(name, item)).peekable();
    hits.peek()?;
    Some(hits.flat_map(|(_, served)| served.iter().cloned()).collect())
}

/// The ticket of the current branch, traced through the journal.
pub fn trace(p: &Project, git: &Git) -> Result<Trace> {
    trace_branch(p, git, git.branch())
}

/// A ticket branch that has a journal, for picking a ticket to look at.
#[derive(Debug, Clone, Serialize)]
pub struct TicketRef {
    pub key: String,
    pub branch: String,
    pub title: Option<String>,
    /// The branch checked out here.
    pub current: bool,
    /// The worktree that has the branch checked out, if any.
    pub checkout: Option<String>,
}

/// Every local ticket branch with a journal, the current one first, then by key.
pub fn tickets(p: &Project, git: &Git) -> Vec<TicketRef> {
    let trees = git.worktrees();
    let mut out: Vec<TicketRef> = git
        .journals()
        .into_iter()
        .filter(|j| j.remote.is_none())
        .filter_map(|j| {
            let key = key(&j.branch)?;
            Some(TicketRef {
                title: notes_dir(&p.root, &key).and_then(|d| title(&d, &key)),
                current: j.branch == git.branch(),
                checkout: trees.get(&j.branch).map(|d| d.display().to_string()),
                key,
                branch: j.branch,
            })
        })
        .collect();
    out.sort_by(|a, b| (!a.current, &a.key, &a.branch).cmp(&(!b.current, &b.key, &b.branch)));
    out
}

/// The trace of the ticket on `branch`, which need not be checked out here: its items are then
/// located in the branch's journal (its latest snapshot), not on disk.
pub fn trace_branch(p: &Project, git: &Git, branch: &str) -> Result<Trace> {
    let key = key(branch).with_context(|| format!("branch {branch} has no ticket key (feature|fix|chore/KEY-… or proto/KEY)"))?;
    let dir = notes_dir(&p.root, &key);
    let ps = dir.as_deref().map(problems).transpose()?.unwrap_or_default();
    let here = branch == git.branch();
    let journal = git.resolve(&git.ref_for("journal", branch)).with_context(|| format!("{branch} has no journal yet"))?;
    // The ticket's cycles, newest first.
    let cycles: Vec<_> = git
        .history(&journal)
        .into_iter()
        .filter_map(|e| {
            let m = CycleMessage::parse(&format!("{}\n\n{}", e.subject, e.body));
            (m.ticket.as_deref() == Some(key.as_str())).then_some((e.sha, e.age, m))
        })
        .collect();
    let messages: Vec<CycleMessage> = cycles.iter().rev().map(|c| c.2.clone()).collect();
    let union = union(&messages);
    let (head, base) = match (cycles.first(), cycles.last()) {
        (Some(newest), Some(oldest)) => (Some(newest.0.clone()), Some(format!("{}^", oldest.0))),
        _ => (None, None),
    };
    let files = match (&head, &base) {
        (Some(h), Some(b)) => git.numstat(b, h).unwrap_or_default(),
        _ => vec![],
    };
    let locations = if here { p.item_locations() } else { snapshot_locations(p, git, &journal, &files) };
    // The items serving a problem (or, with None, serving none), each once.
    let items = |only: Option<&str>| {
        locations
            .iter()
            .filter(|(path, _)| problems_of(&union, path).is_some_and(|served| only.map_or(served.is_empty(), |id| served.contains(id))))
            .map(|(path, (file, line))| {
                let reasons = cycles
                    .iter()
                    .flat_map(|(sha, when, m)| m.why.iter().filter(|w| same_item(&w.item, path) && only.is_none_or(|id| w.problems.iter().any(|x| x == id))).map(move |w| Reason { sha: sha.clone(), when: when.clone(), why: w.why.clone() }))
                    .collect();
                TraceItem { item: path.clone(), file: file.clone(), line: *line, reasons }
            })
            .collect::<Vec<_>>()
    };
    let sorted = |mut items: Vec<TraceItem>| {
        items.sort_by(|a, b| (&a.file, a.line).cmp(&(&b.file, b.line)));
        items
    };
    let problems = ps
        .iter()
        .map(|pr| TraceProblem {
            id: pr.id.clone(),
            text: pr.text.clone(),
            scenarios: pr.scenarios.clone(),
            items: sorted(items(Some(&pr.id))),
        })
        .collect();
    let untraced = sorted(items(None));
    let checkout = if here { Some(p.root.clone()) } else { git.worktrees().get(branch).map(|d| git.project_in(d)) };
    let body = dir.as_deref().map(|d| notes(d).into_iter().map(|(_, t)| t).collect::<Vec<_>>().join("\n\n---\n\n")).unwrap_or_default();
    Ok(Trace {
        title: dir.as_deref().and_then(|d| title(d, &key)),
        notes_dir: dir.map(|d| d.display().to_string()),
        key,
        body,
        problems,
        untraced,
        cycles: cycles.len(),
        head,
        base,
        files,
        union,
        branch: branch.to_string(),
        checkout,
    })
}

/// Where each item of the ticket's files is in snapshot `sha`: item path -> (file, 1-based line).
fn snapshot_locations(p: &Project, git: &Git, sha: &str, files: &[FileStat]) -> std::collections::HashMap<String, (String, usize)> {
    let mut out = std::collections::HashMap::new();
    for f in files.iter().filter(|f| p.lang.is_src(&f.path)) {
        let Some(src) = git.show(sha, &f.path) else { continue };
        for it in crate::items::parse(p.lang, &src).unwrap_or_default() {
            out.entry(it.path).or_insert((f.path.clone(), it.line));
        }
    }
    out
}

/// The ticket as a terminal tree: problems with the `file:line` of each item that serves them.
pub fn render(t: &Trace) -> String {
    let mut s = format!("{} {}\n", t.key, t.title.as_deref().unwrap_or(""));
    s.push_str(&match &t.notes_dir {
        Some(d) => format!("problems: {}/problems.json\n", d),
        None => format!("no notes found at .work/{} in the main checkout\n", t.key),
    });
    s.push_str(&format!("{} cycle(s), {} file(s) changed\n", t.cycles, t.files.len()));
    let items = |s: &mut String, items: &[TraceItem]| {
        for i in items {
            s.push_str(&format!("    {}:{}  {}\n", i.file, i.line, i.item));
        }
    };
    for p in &t.problems {
        s.push_str(&format!("\n{}  {}\n", p.id, p.text));
        items(&mut s, &p.items);
    }
    if !t.untraced.is_empty() {
        s.push_str("\nUntraced (serve no problem)\n");
        items(&mut s, &t.untraced);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::Why;

    fn p(id: &str) -> Problem {
        Problem { id: id.into(), text: "Something is wrong".into(), scenarios: vec![] }
    }

    #[test]
    fn keys_come_from_feature_fix_chore_and_proto_branches() {
        for (branch, want) in [("feature/DEMO-1-window", "DEMO-1"), ("fix/AB2-17", "AB2-17"), ("chore/PROJ-123-bump-deps", "PROJ-123"), ("proto/DEMO-1", "DEMO-1")] {
            assert_eq!(key(branch).as_deref(), Some(want), "{branch}");
        }
        for branch in ["main", "master", "feature/foo", "ABC-1", "feature/ABC", "proto/DEMO-1-x", "release/DEMO-1", "feature/demo-1", "wish/DEMO-1"] {
            assert_eq!(key(branch), None, "{branch}");
        }
    }

    fn cycle(items: &[(&str, &[&str])]) -> CycleMessage {
        CycleMessage { why: items.iter().map(|(i, ps)| Why { item: i.to_string(), why: "because".into(), problems: ps.iter().map(|s| s.to_string()).collect() }).collect(), ..Default::default() }
    }

    #[test]
    fn union_adds_problems_and_never_replaces() {
        let u = union(&[cycle(&[("Window.open", &["P1"]), ("Window.close", &[])]), cycle(&[("Window.open", &["P2"]), ("Window.close", &["P3"]), ("Mode", &[])]), cycle(&[("Window.open", &[])])]);
        assert_eq!(u["Window.open"], BTreeSet::from(["P1".to_string(), "P2".to_string()]));
        assert_eq!(u["Window.close"], BTreeSet::from(["P3".to_string()]));
        assert!(u["Mode"].is_empty());
    }

    #[test]
    fn untraced_means_tagged_empty_in_a_ticket_cycle() {
        // Only items that appear in a ticket cycle are listed; an item with an empty union serves nothing.
        let u = union(&[cycle(&[("A", &["P1"]), ("B", &[])])]);
        assert_eq!(problems_of(&u, "A").map(|s| s.is_empty()), Some(false));
        // A journal path with a parameter list names the item without one, and the reverse.
        let u = union(&[cycle(&[("App.create(String)", &["P1"]), ("Window.open", &["P2"]), ("Window.open(Mode)", &["P3"])])]);
        assert_eq!(problems_of(&u, "App.create"), Some(BTreeSet::from(["P1".to_string()])));
        assert_eq!(problems_of(&u, "Window.open(int, int)"), Some(BTreeSet::from(["P2".to_string()])));
        assert_eq!(problems_of(&u, "Window.open(Mode)"), Some(BTreeSet::from(["P2".to_string(), "P3".to_string()])));
        let u = union(&[cycle(&[("A", &["P1"]), ("B", &[])])]);
        assert_eq!(problems_of(&u, "B").map(|s| s.is_empty()), Some(true));
        assert!(problems_of(&u, "Generated.beforeTheTicket").is_none());
        // An overload falls back to its bare name.
        assert!(problems_of(&u, "A(int, int)").is_some());
    }

    #[test]
    fn validation() {
        assert!(validate(&[p("P1"), p("P2")]).is_ok());
        assert!(validate(&[]).is_err());
        assert!(validate(&(1..=9).map(|i| p(&format!("P{i}"))).collect::<Vec<_>>()).is_err());
        assert!(validate(&[p("P1"), p("P1")]).is_err());
        assert!(validate(&[p("one")]).is_err());
        assert!(validate(&[Problem { text: "  ".into(), ..p("P1") }]).is_err());
    }

    #[test]
    fn extraction_is_validated() {
        let ok = json!({"structured_output": {"problems": [{"id": "P1", "text": "Slow", "scenarios": ["Fast"]}]}});
        assert_eq!(extract(&ok).unwrap()[0].scenarios, ["Fast"]);
        assert!(extract(&json!({"structured_output": {"problems": []}})).is_err());
        assert!(extract(&json!({"result": "no structure"})).is_err());
        assert!(extract(&json!({"is_error": true, "result": "boom"})).is_err());
    }

    #[test]
    fn problems_file_roundtrip() {
        let dir = std::env::temp_dir().join(format!("wish-ticket-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(problems(&dir).unwrap().is_empty());
        save_problems(&dir, &[p("P1")]).unwrap();
        assert_eq!(problems(&dir).unwrap(), [p("P1")]);
        assert!(save_problems(&dir, &[]).is_err());
        assert_eq!(problems(&dir).unwrap(), [p("P1")]); // a rejected save leaves the file alone
        std::fs::write(dir.join("problems.json"), "[{").unwrap();
        assert!(problems(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn renders_a_tree_with_locations() {
        let item = |name: &str, line| TraceItem { item: name.into(), file: "src/App.java".into(), line, reasons: vec![] };
        let t = Trace {
            key: "DEMO-1".into(),
            title: Some("Open the window".into()),
            body: String::new(),
            notes_dir: Some("/repo/.work/DEMO-1".into()),
            problems: vec![TraceProblem { id: "P1".into(), text: "No window".into(), scenarios: vec![], items: vec![item("App.create", 11)] }],
            untraced: vec![item("App.run", 14)],
            cycles: 2,
            head: None,
            base: None,
            files: vec![],
            union: Problems::new(),
            branch: "feature/DEMO-1".into(),
            checkout: None,
        };
        let text = render(&t);
        assert!(text.starts_with("DEMO-1 Open the window\nproblems: /repo/.work/DEMO-1/problems.json\n2 cycle(s), 0 file(s) changed\n"));
        assert!(text.contains("P1  No window\n    src/App.java:11  App.create\n"));
        assert!(text.contains("Untraced (serve no problem)\n    src/App.java:14  App.run\n"));
    }
}
