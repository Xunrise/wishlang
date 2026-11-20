//! Code Claude writes outside wish: in an ordinary Claude Code session, or in commits made before wish
//! was set up. Both land in the journal like a cycle, so the code shows as generated, with reasons, and
//! is traced to the ticket's problems.
//!
//! - `wish session` runs as a Claude Code hook (user settings, see the README):
//!   - `UserPromptSubmit`: what is on disk now is yours, so it's recorded as a save.
//!   - `Stop`: what changed during Claude's turn is Claude's. If the turn changed items, the hook first
//!     sends Claude back once to explain them (`wish explain`); then it commits a `session` entry.
//!   It does nothing inside wish's own cycles (`WISH_CYCLE` is set there), while a cycle holds the lock,
//!   or when no wish project is at or right below the session's directory.
//! - `wish import <since>`: one entry per commit in `<since>..HEAD`, for code written before.

use crate::git::{Git, Who};
use crate::items;
use crate::journal::{CycleMessage, Why, same_item};
use crate::project::Project;
use crate::ticket::{self, Problem};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Set for every Claude process wish starts itself; the session hooks ignore those.
pub const IN_CYCLE: &str = "WISH_CYCLE";

/// What Claude reports for a turn through `wish explain`.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Explanation {
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub changes: Vec<Change>,
    /// The ticket's problems, when `problems.json` doesn't exist yet.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<Problem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Change {
    pub item: String,
    #[serde(default)]
    pub why: String,
    #[serde(default)]
    pub problems: Vec<String>,
}

/// One turn of a session, kept between its two hooks.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Turn {
    prompts: Vec<String>,
}

// ---- hooks -------------------------------------------------------------------

/// `wish session`: the hook JSON is on stdin; what Claude Code should do is printed on stdout.
pub fn hook() -> Result<()> {
    let mut input = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut input)?;
    if std::env::var_os(IN_CYCLE).is_some() {
        return Ok(());
    }
    let v: Value = serde_json::from_str(&input)?;
    let cwd = PathBuf::from(v["cwd"].as_str().unwrap_or("."));
    let session = v["session_id"].as_str().unwrap_or("unknown").to_string();
    let mut asks = Vec::new();
    for p in projects_near(&cwd) {
        let r = match v["hook_event_name"].as_str() {
            Some("UserPromptSubmit") => before_turn(&p, &session, v["prompt"].as_str().unwrap_or("")).map(|()| None),
            Some("Stop") => after_turn(&p, &session, v["stop_hook_active"] == true, v["transcript_path"].as_str()),
            _ => Ok(None),
        };
        match r {
            Ok(Some(ask)) => asks.push(ask),
            Ok(None) => crate::daemon::notify_changed(&p.root),
            // A hook must never break the session: say what went wrong, carry on.
            Err(e) => eprintln!("wish: {}: {e:#}", p.root.display()),
        }
    }
    if !asks.is_empty() {
        println!("{}", json!({"decision": "block", "reason": asks.join("\n\n")}));
    }
    Ok(())
}

/// The wish projects a session in `cwd` works on: the one `cwd` is in, else those in its direct subdirectories.
fn projects_near(cwd: &Path) -> Vec<Project> {
    if let Some(p) = Project::discover_from(cwd) {
        return vec![p];
    }
    let Ok(rd) = std::fs::read_dir(cwd) else { return vec![] };
    let mut dirs: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|d| d.join("wish.toml").is_file()).collect();
    dirs.sort();
    dirs.iter().filter_map(|d| Project::load(d).ok()).collect()
}

/// Kept in the git directory, so the turn's own bookkeeping is never part of a snapshot.
fn turn_file(git: &Git, session: &str, ext: &str) -> PathBuf {
    let safe: String = session.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
    git.private_file(&format!("session-{safe}.{ext}"))
}

fn before_turn(p: &Project, session: &str, prompt: &str) -> Result<()> {
    let git = Git::open(&p.root)?;
    let Some(_lock) = git.try_lock() else { return Ok(()) }; // a wish cycle is running and records itself
    let tip = crate::engine::ensure_journal(p, &git)?;
    // While another session's turn is open, what is on disk may be its half-done work: don't claim it
    // as the user's. That session's Stop records it as Claude's (with any edits of yours in between,
    // erring towards showing code as generated rather than hiding generated code as yours).
    if !other_turn_open(&git, session) {
        crate::engine::record_save(&git, &tip, "save: before a Claude Code turn")?;
    }
    let path = turn_file(&git, session, "turn.json");
    let mut turn: Turn = read_json(&path).unwrap_or_default();
    turn.prompts.push(prompt.trim().to_string());
    write_json(&path, &turn)
}

/// Turns older than this are from sessions that ended without their Stop hook.
const STALE_TURN: std::time::Duration = std::time::Duration::from_secs(6 * 3600);

/// Is a turn of another session open on this project (its prompt seen, its Stop not yet)?
fn other_turn_open(git: &Git, session: &str) -> bool {
    let mine = turn_file(git, session, "turn.json");
    let probe = turn_file(git, "", "turn.json");
    let (Some(dir), Some(name)) = (probe.parent(), probe.file_name().and_then(|n| n.to_str())) else { return false };
    let (prefix, suffix) = name.split_once("session-").map(|(a, _)| (format!("{a}session-"), ".turn.json")).unwrap_or_default();
    let Ok(rd) = std::fs::read_dir(dir) else { return false };
    rd.flatten().any(|e| {
        let n = e.file_name().to_string_lossy().to_string();
        let fresh = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age < STALE_TURN);
        n.starts_with(&prefix) && n.ends_with(suffix) && e.path() != mine && fresh
    })
}

/// Record the turn, or (once) ask Claude to explain it first. Returns what to ask.
fn after_turn(p: &Project, session: &str, asked: bool, transcript: Option<&str>) -> Result<Option<String>> {
    let git = Git::open(&p.root)?;
    let Some(_lock) = git.try_lock() else { return Ok(None) };
    let tip = crate::engine::ensure_journal(p, &git)?;
    let tree = git.tree(None)?;
    if git.tree_of(&tip).as_deref() == Some(tree.as_str()) {
        let _ = std::fs::remove_file(turn_file(&git, session, "turn.json"));
        return Ok(None); // the turn changed nothing here
    }
    let changed = changed_items(p, &git, &tip, &tree)?;
    let explained: Option<Explanation> = read_json(&turn_file(&git, session, "explain.json"));
    let ticket = Ticket::of(p, &git);
    if explained.is_none() && !asked && !changed.is_empty() {
        return Ok(Some(ask(p, session, &changed, ticket.as_ref())));
    }
    let turn: Turn = read_json(&turn_file(&git, session, "turn.json")).unwrap_or_default();
    let e = explained.unwrap_or_else(|| Explanation { summary: transcript.and_then(last_answer).unwrap_or_default(), ..Default::default() });
    let ticket = ticket.map(|t| t.with_problems(&e.problems)).transpose()?.flatten();
    let summary = if e.summary.trim().is_empty() { "Claude Code session".to_string() } else { e.summary.clone() };
    let msg = CycleMessage {
        label: "session".into(),
        summary: summary.clone(),
        why: whys(&changed, &e.changes, &summary),
        triggered_by: turn.prompts.iter().filter(|s| !s.is_empty()).map(|s| format!("prompt: {}", one_line(s, 200))).collect(),
        decisions: vec![],
        mode: "session".into(),
        ticket: ticket.map(|t| t.key),
    };
    let sha = git.commit(&tree, &[&tip], Who::Claude, &msg.render())?;
    git.set_ref(&git.ref_name("journal"), &sha)?;
    // The next cycle diffs your edits from here: Claude's session is not yours to regenerate for.
    git.set_ref(&git.ref_name("last"), &sha)?;
    for ext in ["turn.json", "explain.json"] {
        let _ = std::fs::remove_file(turn_file(&git, session, ext));
    }
    Ok(None)
}

/// What the Stop hook asks Claude: one line per changed item, the problems, and the command to report with.
fn ask(p: &Project, session: &str, changed: &[(String, String)], ticket: Option<&Ticket>) -> String {
    let exe = std::env::current_exe().map(|e| e.display().to_string()).unwrap_or_else(|_| "wishlang".into());
    let mut s = format!(
        "wish records the code Claude writes in {} (shown as generated, with your reasons, in the editor and `wish log`). \
         Before you finish, explain the items this turn changed:\n",
        p.root.display()
    );
    for (item, file) in changed.iter().take(80) {
        s.push_str(&format!("- `{item}` ({file})\n"));
    }
    if changed.len() > 80 {
        s.push_str(&format!("- … and {} more\n", changed.len() - 80));
    }
    let mut example = json!({"summary": "1-3 sentences: what you changed and why", "changes": [{"item": changed[0].0, "why": "1-2 concrete sentences: what in the request or the code led to it", "problems": ["P1"]}]});
    match ticket {
        Some(t) if !t.problems.is_empty() => {
            s.push_str(&format!("\nThe branch is ticket {}. Tag each item with the problems it serves ([] if none; never invent an id):\n", t.key));
            for pr in &t.problems {
                s.push_str(&format!("- {}: {}\n", pr.id, pr.text));
            }
        }
        Some(t) => {
            s.push_str(&format!(
                "\nThe branch is ticket {key}, which has no problems.json yet. Distil 2 to 6 problems from {dir}/intent.md and spec.md \
                 (one plain sentence each, as the people who use the result see them, ids P1, P2, …, with the titles of the Gherkin \
                 scenarios covering each), include them as `problems`, and tag each item with the ids it serves.\n",
                key = t.key,
                dir = t.dir.display()
            ));
            example["problems"] = json!([{"id": "P1", "text": "…", "scenarios": ["…"]}]);
        }
        None => {
            example["changes"][0].as_object_mut().unwrap().remove("problems");
        }
    }
    s.push_str(&format!(
        "\nPipe the JSON into this command (one entry per item above):\n  {exe} explain --session {session} <<'EOF'\n  {}\n  EOF\nThen end your turn with one short line; don't repeat your answer.",
        serde_json::to_string(&example).unwrap()
    ));
    s
}

/// `wish explain --session <id>`: Claude's explanation of its turn (JSON on stdin), kept for the Stop hook.
pub fn explain(session: &str) -> Result<String> {
    let mut input = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut input)?;
    let e: Explanation = serde_json::from_str(&input).context("expected {\"summary\": …, \"changes\": [{\"item\", \"why\", \"problems\"}]}")?;
    if !e.problems.is_empty() {
        ticket::validate(&e.problems).context("problems")?;
    }
    let cwd = std::env::current_dir()?;
    let projects = projects_near(&cwd);
    if projects.is_empty() {
        bail!("no wish project at or below {}", cwd.display());
    }
    for p in &projects {
        let git = Git::open(&p.root)?;
        if let Some(t) = Ticket::of(p, &git) {
            let ids: Vec<&str> = if e.problems.is_empty() { t.problems.iter().map(|x| x.id.as_str()).collect() } else { e.problems.iter().map(|x| x.id.as_str()).collect() };
            if let Some(bad) = e.changes.iter().flat_map(|c| &c.problems).find(|id| !ids.contains(&id.as_str())) {
                bail!("{bad} is not a problem of ticket {} (known: {})", t.key, ids.join(", "));
            }
        }
        write_json(&turn_file(&git, session, "explain.json"), &e)?;
    }
    Ok(format!("recorded the explanation of {} items; finish your turn now", e.changes.len()))
}

// ---- import ------------------------------------------------------------------

/// `wish import <since>`: rebuild this branch's journal from git history. The code at `since` is yours;
/// each commit of `since..HEAD` that touches the project is Claude's, explained by its commit message
/// (and, per commit, by `report`: `{"<sha>": {"summary"?, "changes": [...], "problems"?}}`); uncommitted
/// changes are yours. Replaces the branch's journal (the old tip is printed, to get it back).
pub fn import(p: &Project, since: &str, report: Option<&Path>) -> Result<String> {
    let git = Git::open(&p.root)?;
    let Some(_lock) = git.try_lock() else { bail!("a wish cycle is running; try again when it's done") };
    let base = git.resolve(&format!("{since}^{{commit}}")).with_context(|| format!("{since} is not a commit"))?;
    let reports: BTreeMap<String, Explanation> = match report {
        Some(f) => serde_json::from_str(&std::fs::read_to_string(f)?).with_context(|| format!("parsing {}", f.display()))?,
        None => BTreeMap::new(),
    };
    let ticket = Ticket::of(p, &git);
    let from_reports: Vec<Problem> = reports.values().find(|e| !e.problems.is_empty()).map(|e| e.problems.clone()).unwrap_or_default();
    let ticket = ticket.map(|t| t.with_problems(&from_reports)).transpose()?.flatten();
    let old = git.journal();

    let mut tip = git.commit(&git.tree_at(&base)?, &[], Who::Human, &format!("save: your code at {}\n\nCode-Head: {base}\n", short(&base)))?;
    let mut imported = 0;
    for sha in git.first_parent_commits(&base)? {
        let tree = git.tree_at(&sha)?;
        if git.tree_of(&tip).as_deref() == Some(tree.as_str()) {
            continue; // the commit doesn't touch this project
        }
        let changed = changed_items(p, &git, &tip, &tree)?;
        let (subject, body) = git.subject_body(&sha)?;
        let e = reports.iter().find(|(k, _)| sha.starts_with(k.as_str()) || k.starts_with(&sha)).map(|(_, e)| e.clone()).unwrap_or_default();
        let summary = if e.summary.trim().is_empty() { format!("{subject}\n\n{body}").trim().to_string() } else { e.summary.clone() };
        // Untagged items of an unreported commit aren't untraced, just unknown: keep the ticket off then.
        let traced = ticket.as_ref().filter(|_| !e.changes.is_empty());
        let msg = CycleMessage {
            label: "imported".into(),
            summary,
            why: whys(&changed, &e.changes, &subject),
            triggered_by: vec![format!("commit {} {subject}", short(&sha))],
            decisions: vec![],
            mode: "imported".into(),
            ticket: traced.map(|t| t.key.clone()),
        };
        tip = git.commit(&tree, &[&tip], Who::Claude, &format!("{}\nCode-Head: {sha}\n", msg.render()))?;
        imported += 1;
    }
    let now = git.tree(None)?;
    if git.tree_of(&tip).as_deref() != Some(now.as_str()) {
        tip = git.commit(&now, &[&tip], Who::Human, "save: your uncommitted changes")?;
    }
    git.set_ref(&git.ref_name("journal"), &tip)?;
    git.set_ref(&git.ref_name("last"), &tip)?;
    let was = old.map(|o| format!(" (the previous journal was {})", short(&o))).unwrap_or_default();
    Ok(format!("imported {imported} commits since {} as Claude's{was}", short(&base)))
}

// ---- shared --------------------------------------------------------------------

/// The branch's ticket and its notes, if wish traces this branch.
struct Ticket {
    key: String,
    dir: PathBuf,
    problems: Vec<Problem>,
}

impl Ticket {
    fn of(p: &Project, git: &Git) -> Option<Ticket> {
        let key = ticket::key(git.branch())?;
        let dir = ticket::notes_dir(&p.root, &key)?;
        Some(Ticket { problems: ticket::problems(&dir).unwrap_or_default(), key, dir })
    }

    /// With problems: the existing ones, else `proposed` (saved as problems.json). None when there are none.
    fn with_problems(mut self, proposed: &[Problem]) -> Result<Option<Ticket>> {
        if self.problems.is_empty() && !proposed.is_empty() {
            ticket::save_problems(&self.dir, proposed)?;
            self.problems = proposed.to_vec();
        }
        Ok((!self.problems.is_empty()).then_some(self))
    }
}

/// Items added or changed between two snapshots, as (item path, file).
fn changed_items(p: &Project, git: &Git, from: &str, to: &str) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for (status, f) in git.changed(from, to)? {
        if status == 'D' || !p.lang.is_src(&f) || p.is_build_output(&f) {
            continue;
        }
        let parse = |sha: &str| git.show(sha, &f).and_then(|s| items::parse(p.lang, &s).ok()).unwrap_or_default();
        let before: BTreeMap<String, String> = parse(from).into_iter().map(|i| (i.path, i.hash)).collect();
        for i in parse(to) {
            if before.get(&i.path) != Some(&i.hash) {
                out.push((i.path, f.clone()));
            }
        }
    }
    Ok(out)
}

/// One `Why` per changed item: Claude's, else the fallback reason.
fn whys(changed: &[(String, String)], given: &[Change], fallback: &str) -> Vec<Why> {
    let fallback = one_line(fallback, 160);
    changed
        .iter()
        .map(|(item, _)| match given.iter().find(|c| c.item == *item).or_else(|| given.iter().find(|c| same_item(&c.item, item))) {
            Some(c) => Why { item: item.clone(), why: one_line(if c.why.trim().is_empty() { &fallback } else { &c.why }, 400), problems: c.problems.clone() },
            None => Why { item: item.clone(), why: fallback.clone(), problems: vec![] },
        })
        .collect()
}

/// Claude's last answer in a session transcript (JSONL), for a turn it didn't explain.
fn last_answer(transcript: &str) -> Option<String> {
    let text = std::fs::read_to_string(transcript).ok()?;
    text.lines().rev().filter_map(|l| serde_json::from_str::<Value>(l).ok()).filter(|v| v["type"] == "assistant").find_map(|v| {
        let parts: Vec<String> = v["message"]["content"].as_array()?.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str().map(String::from)).collect();
        (!parts.is_empty()).then(|| one_line(&parts.join(" "), 600))
    })
}

fn one_line(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max { flat } else { flat.chars().take(max).collect::<String>() + "…" }
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(8)]
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn write_json<T: Serialize>(path: &Path, v: &T) -> Result<()> {
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(path, serde_json::to_string_pretty(v)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as Cmd;

    /// A repository with the wish project in `backend/` (as in a monorepo) on a ticket branch.
    fn repo(name: &str) -> (PathBuf, Project) {
        let dir = std::env::temp_dir().join(format!("wish-session-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let java = dir.join("backend/src/main/java/demo");
        std::fs::create_dir_all(&java).unwrap();
        std::fs::write(dir.join("backend/pom.xml"), "<project/>").unwrap();
        std::fs::write(dir.join("backend/wish.toml"), "[protected]\nfiles = [\"src/main/java/demo/Main.java\"]\n").unwrap();
        std::fs::write(java.join("Main.java"), "package demo;\n\npublic class Main {\n    public static void main(String[] args) {\n    }\n}\n").unwrap();
        std::fs::write(dir.join(".gitignore"), ".work/\n").unwrap();
        let notes = dir.join(".work/DEMO-1");
        std::fs::create_dir_all(&notes).unwrap();
        std::fs::write(notes.join("problems.json"), r#"[{"id": "P1", "text": "The demo window does not open"}]"#).unwrap();
        git(&dir, &["init", "-q"]);
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-qm", "init"]);
        git(&dir, &["checkout", "-qb", "feature/DEMO-1-window"]);
        let p = Project::load(&dir.join("backend")).unwrap();
        (dir, p)
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let o = Cmd::new("git").args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args).current_dir(dir).output().unwrap();
        assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).to_string()
    }

    const APP: &str = "package demo;\n\npublic class App {\n    public static App create(String n) {\n        return new App();\n    }\n\n    public void run() {\n    }\n}\n";

    fn whos(g: &Git, rel: &str, text: &str) -> Vec<Who> {
        g.blame_lines(rel, text).unwrap().into_iter().map(|(w, _)| w).collect()
    }

    #[test]
    fn a_session_turn_is_explained_then_recorded_as_claudes() {
        let (dir, p) = repo("turn");
        assert_eq!(projects_near(&dir).len(), 1, "found from the repository root");
        before_turn(&p, "s-1", "Add an App the demo can run").unwrap();
        // Claude writes a file (however: Write, Edit or a Bash heredoc) and edits yours.
        std::fs::write(p.root.join("src/main/java/demo/App.java"), APP).unwrap();
        std::fs::write(p.root.join("src/main/java/demo/Main.java"), "package demo;\n\npublic class Main {\n    public static void main(String[] args) {\n        App.create(\"x\").run();\n    }\n}\n").unwrap();

        let ask = after_turn(&p, "s-1", false, None).unwrap().expect("asks for reasons first");
        assert!(ask.contains("`App.create`") && ask.contains("`App.run`") && ask.contains("P1: The demo window does not open"), "{ask}");
        assert!(ask.contains("explain --session s-1"), "{ask}");

        let g = Git::open(&p.root).unwrap();
        let e = Explanation { summary: "Added App.".into(), changes: vec![Change { item: "App.create".into(), why: "Main needs an App.".into(), problems: vec!["P1".into()] }], problems: vec![] };
        write_json(&turn_file(&g, "s-1", "explain.json"), &e).unwrap();
        assert!(after_turn(&p, "s-1", true, None).unwrap().is_none());

        let g = Git::open(&p.root).unwrap();
        let tip = g.journal().unwrap();
        let msg = CycleMessage::parse(&g.message(&tip).unwrap().0);
        assert_eq!((msg.label.as_str(), msg.ticket.as_deref(), msg.mode.as_str()), ("session", Some("DEMO-1"), "session"));
        let why = |item: &str| msg.why.iter().find(|w| w.item == item).cloned().unwrap();
        assert_eq!(why("App.create"), Why { item: "App.create".into(), why: "Main needs an App.".into(), problems: vec!["P1".into()] });
        // unexplained items fall back to the summary and serve no problem (so they show as untraced)
        assert_eq!(why("App.run"), Why { item: "App.run".into(), why: "Added App.".into(), problems: vec![] });
        assert_eq!(msg.triggered_by, ["prompt: Add an App the demo can run"]);
        assert!(whos(&g, "src/main/java/demo/App.java", APP).iter().all(|w| *w == Who::Claude));
        let main = std::fs::read_to_string(p.root.join("src/main/java/demo/Main.java")).unwrap();
        assert_eq!(whos(&g, "src/main/java/demo/Main.java", &main), [Who::Human, Who::Human, Who::Human, Who::Human, Who::Claude, Who::Human, Who::Human]);
        assert_eq!(g.last().as_deref(), Some(tip.as_str()));

        // A turn that changes nothing here records nothing.
        before_turn(&p, "s-1", "What does App do?").unwrap();
        assert!(after_turn(&p, "s-1", false, None).unwrap().is_none());
        assert_eq!(Git::open(&p.root).unwrap().journal(), Some(tip));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn your_edits_before_a_turn_stay_yours() {
        let (dir, p) = repo("yours");
        crate::engine::ensure_journal(&p, &Git::open(&p.root).unwrap()).unwrap();
        std::fs::write(p.root.join("src/main/java/demo/Mine.java"), "package demo;\nclass Mine {}\n").unwrap();
        before_turn(&p, "s-2", "go").unwrap();
        std::fs::write(p.root.join("src/main/java/demo/App.java"), APP).unwrap();
        assert!(after_turn(&p, "s-2", true, None).unwrap().is_none(), "asks only once");
        let g = Git::open(&p.root).unwrap();
        assert!(whos(&g, "src/main/java/demo/Mine.java", "package demo;\nclass Mine {}\n").iter().all(|w| *w == Who::Human));
        assert!(whos(&g, "src/main/java/demo/App.java", APP).iter().all(|w| *w == Who::Claude));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_prompt_in_another_session_does_not_claim_a_running_turns_code() {
        let (dir, p) = repo("concurrent");
        crate::engine::ensure_journal(&p, &Git::open(&p.root).unwrap()).unwrap();
        before_turn(&p, "busy", "build App").unwrap();
        // session "busy" is mid-turn and has written App; another session gets a prompt now
        std::fs::write(p.root.join("src/main/java/demo/App.java"), APP).unwrap();
        before_turn(&p, "other", "unrelated question").unwrap();
        let _ = after_turn(&p, "other", true, None).unwrap();
        let _ = after_turn(&p, "busy", true, None).unwrap();
        let g = Git::open(&p.root).unwrap();
        assert!(whos(&g, "src/main/java/demo/App.java", APP).iter().all(|w| *w == Who::Claude));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_records_each_commit_touching_the_project_as_claudes() {
        let (dir, p) = repo("import");
        let base = git(&dir, &["rev-parse", "HEAD"]).trim().to_string();
        std::fs::write(p.root.join("src/main/java/demo/App.java"), APP).unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-qm", "Add App\n\nSo Main can run something."]);
        std::fs::write(dir.join("README.md"), "outside the project\n").unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-qm", "Docs only"]);
        let sha = git(&dir, &["rev-parse", "HEAD~1"]).trim().to_string();
        let report = dir.join("report.json");
        std::fs::write(&report, json!({ &sha[..10]: {"changes": [{"item": "App.create", "why": "Main calls it.", "problems": ["P1"]}]}}).to_string()).unwrap();

        let out = import(&p, &base, Some(&report)).unwrap();
        assert!(out.starts_with("imported 1 commits"), "{out}");
        let g = Git::open(&p.root).unwrap();
        let log = g.history(&g.journal().unwrap());
        let labels: Vec<String> = log.iter().map(|e| CycleMessage::parse(&g.message(&e.sha).unwrap().0).label).collect();
        assert_eq!(labels, ["imported", "save"], "newest first: the commit, then your code at the base");
        let imported = CycleMessage::parse(&g.message(&log[0].sha).unwrap().0);
        assert_eq!(imported.ticket.as_deref(), Some("DEMO-1"));
        assert_eq!(imported.why.iter().map(|w| (w.item.as_str(), w.problems.len())).collect::<Vec<_>>(), [("App", 0), ("App.create", 1), ("App.run", 0)]);
        assert!(imported.summary.contains("So Main can run something."));
        assert!(whos(&g, "src/main/java/demo/App.java", APP).iter().all(|w| *w == Who::Claude));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tickets_of_other_branches_and_worktrees_can_be_traced() {
        let (dir, p) = repo("tickets");
        let explain = |p: &Project, sid: &str, item: &str| {
            let g = Git::open(&p.root).unwrap();
            let e = Explanation { summary: "s".into(), changes: vec![Change { item: item.into(), why: format!("{item} why"), problems: vec!["P1".into()] }], problems: vec![] };
            write_json(&turn_file(&g, sid, "explain.json"), &e).unwrap();
        };
        before_turn(&p, "a", "app").unwrap();
        std::fs::write(p.root.join("src/main/java/demo/App.java"), APP).unwrap();
        explain(&p, "a", "App.create");
        after_turn(&p, "a", false, None).unwrap();

        // A second ticket in its own worktree, with its notes in the main checkout.
        let notes = dir.join(".work/DEMO-2");
        std::fs::create_dir_all(&notes).unwrap();
        std::fs::write(notes.join("intent.md"), "# DEMO-2 — Say goodbye\n").unwrap();
        std::fs::write(notes.join("problems.json"), r#"[{"id": "P1", "text": "Nobody says goodbye"}]"#).unwrap();
        let wt = dir.with_file_name(format!("{}-wt", dir.file_name().unwrap().to_string_lossy()));
        let _ = std::fs::remove_dir_all(&wt);
        git(&dir, &["worktree", "add", "-q", "-b", "feature/DEMO-2-bye", wt.to_str().unwrap()]);
        let q = Project::load(&wt.join("backend")).unwrap();
        before_turn(&q, "b", "bye").unwrap();
        std::fs::write(q.root.join("src/main/java/demo/Bye.java"), "package demo;\n\npublic class Bye {\n    public static void say() {\n    }\n}\n").unwrap();
        explain(&q, "b", "Bye.say");
        after_turn(&q, "b", false, None).unwrap();

        let g = Git::open(&p.root).unwrap();
        let list = ticket::tickets(&p, &g);
        let got: Vec<(&str, bool, bool)> = list.iter().map(|t| (t.key.as_str(), t.current, t.checkout.is_some())).collect();
        assert_eq!(got, [("DEMO-1", true, true), ("DEMO-2", false, true)]);
        assert_eq!(list[1].title.as_deref(), Some("Say goodbye"));

        // Traced from the main checkout: located in DEMO-2's journal, opened from its worktree.
        let t = ticket::trace_branch(&p, &g, "feature/DEMO-2-bye").unwrap();
        assert_eq!(t.key, "DEMO-2");
        let items: Vec<(&str, &str, usize)> = t.problems[0].items.iter().map(|i| (i.item.as_str(), i.file.as_str(), i.line)).collect();
        assert_eq!(items, [("Bye.say", "src/main/java/demo/Bye.java", 4)]);
        assert_eq!(t.checkout.as_deref().map(|c| c.ends_with("backend")), Some(true));
        // Canonical: on macOS the temp dir is a symlink (/var -> /private/var).
        assert!(t.checkout.unwrap().canonicalize().unwrap().starts_with(wt.canonicalize().unwrap()));
        // ... and this branch's own ticket is unaffected.
        assert_eq!(ticket::trace(&p, &g).unwrap().problems[0].items.iter().map(|i| i.item.as_str()).collect::<Vec<_>>(), ["App.create"]);
        let _ = std::fs::remove_dir_all(&wt);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
