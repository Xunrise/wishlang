//! Git as the source of truth for "what changed" and "who wrote it".
//!
//! Snapshots are commits built with a *temporary index*, so the user's
//! branch, index and HEAD are never touched. Hidden refs per project, **per
//! code branch** (wish's history follows the shape of yours):
//!
//! - `refs/wish/<prefix>journal/<branch>`: every save (authored by **you**)
//!   and every Claude cycle (authored by **claude**). A new branch's journal
//!   forks from the journal of the branch it came from; when you merge a
//!   branch's code, its journal is merged too. `git blame` on it gives
//!   line-level provenance, through merges.
//! - `refs/wish/<prefix>last/<branch>`: the last green cycle. Your edits since
//!   then are the regeneration scope.
//! - `refs/wish/<prefix>prototype/<branch>`: the prototype frozen by implement.
//! - `refs/wish/<prefix>attic/<sha>`: reverted attempts, kept out of blame.
//! - `refs/heads/wish/<prefix>log/<branch>`: a branch mirror of the journal
//!   for git tooling (Zed's git graph, lazygit).
//! - `refs/wish-remote/<remote>/…`: journals fetched by `wish sync`.
//!
//! Every journal commit carries a `Code-Head: <sha>` trailer: the code commit
//! it was made on.

use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::io::Write;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Who {
    Human,
    Claude,
}

impl Who {
    fn ident(self) -> (&'static str, &'static str) {
        match self {
            Who::Human => ("you", "human@wishlang"),
            Who::Claude => ("claude", "claude@wishlang"),
        }
    }
    pub fn name(self) -> &'static str {
        self.ident().0
    }
    fn from_mail(mail: &str) -> Who {
        if mail.contains("claude@wishlang") { Who::Claude } else { Who::Human }
    }
}

/// A journal found in the repository: local, or fetched from a remote.
#[derive(Debug, Clone)]
pub struct JournalRef {
    pub branch: String,
    pub remote: Option<String>,
    pub sha: String,
}

/// Lines added and removed in one file (`git diff --numstat`).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FileStat {
    pub path: String,
    pub added: u32,
    pub removed: u32,
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub sha: String,
    pub who: Who,
    pub subject: String,
    /// The rest of the commit message.
    pub body: String,
    /// "5 minutes ago".
    pub age: String,
    /// Files changed against the first parent.
    pub files: Vec<FileStat>,
    pub parent: Option<String>,
    /// Parents beyond the first: reverted attempts / undone cycles.
    pub side: Vec<String>,
}

/// Held while a cycle runs; released on drop.
pub struct CycleLock(PathBuf);

impl Drop for CycleLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub struct Git {
    root: PathBuf,
    /// Project dir relative to the repo root, e.g. "examples/demo/" (or "").
    prefix: String,
    index: PathBuf,
    /// The checked-out code branch ("detached" without one).
    branch: String,
}

/// Wish's per-branch ref kinds.
const PER_BRANCH: [&str; 3] = ["journal", "last", "prototype"];

impl Git {
    pub fn open(root: &Path) -> Result<Self> {
        let out = Command::new("git").args(["rev-parse", "--show-prefix", "--git-path", "wish-index"]).current_dir(root).output().context("running git")?;
        if !out.status.success() {
            bail!("wishlang uses git to track changes: {} is not in a git repository (run `git init`)", root.display());
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let mut lines = text.lines();
        let prefix = lines.next().unwrap_or("").to_string();
        let index = root.join(lines.next().unwrap_or(".git/wish-index"));
        let branch = Command::new("git")
            .args(["symbolic-ref", "--quiet", "--short", "HEAD"])
            .current_dir(root)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|| "detached".into());
        let g = Self { root: root.to_path_buf(), prefix, index, branch };
        g.migrate();
        Ok(g)
    }

    /// Journals from before per-branch refs (`refs/wish/<prefix>journal`)
    /// become the current branch's journal.
    fn migrate(&self) {
        for name in PER_BRANCH {
            let old = format!("refs/wish/{}{name}", self.prefix);
            if let Ok(sha) = self.git(&["rev-parse", "--verify", "--quiet", &format!("{old}^{{commit}}")]) {
                let sha = sha.trim().to_string();
                let _ = self.git(&["update-ref", "-d", &old]);
                let _ = self.git(&["update-ref", &self.ref_name(name), &sha]);
            }
        }
        let old_log = format!("refs/heads/wish/{}log", self.prefix);
        if let Ok(sha) = self.git(&["rev-parse", "--verify", "--quiet", &old_log]) {
            let _ = self.git(&["update-ref", "-d", &old_log]);
            let _ = self.git(&["update-ref", &self.journal_branch(), sha.trim()]);
        }
    }

    pub fn branch(&self) -> &str {
        &self.branch
    }

    /// The checked-out code commit, if any.
    pub fn code_head(&self) -> Option<String> {
        self.resolve("HEAD")
    }

    fn run(&self, args: &[&str], temp_index: bool, who: Who, stdin: Option<&str>) -> Result<String> {
        let mut c = Command::new("git");
        c.args(args).current_dir(&self.root);
        if temp_index {
            c.env("GIT_INDEX_FILE", &self.index);
        }
        let (name, mail) = who.ident();
        c.env("GIT_AUTHOR_NAME", name).env("GIT_AUTHOR_EMAIL", mail);
        c.env("GIT_COMMITTER_NAME", "wishlang").env("GIT_COMMITTER_EMAIL", "wish@localhost");
        c.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = c.spawn()?;
        if let Some(s) = stdin {
            child.stdin.take().unwrap().write_all(s.as_bytes())?;
        }
        let out = child.wait_with_output()?;
        if !out.status.success() {
            bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    fn git(&self, args: &[&str]) -> Result<String> {
        self.run(args, false, Who::Human, None)
    }

    /// Tree of the project directory as on disk (honouring .gitignore),
    /// optionally limited to some paths.
    pub fn tree(&self, only: Option<&[String]>) -> Result<String> {
        let _ = std::fs::remove_file(&self.index);
        let mut args: Vec<&str> = vec!["add", "-A", "--"];
        match only {
            Some(paths) => args.extend(paths.iter().map(String::as_str).filter(|p| self.root.join(p).exists())),
            None => {
                args.push(".");
                // Build output never belongs in a snapshot, even without a
                // .gitignore. (Git errors on an exclude pathspec naming an
                // already-ignored path, so only add it when needed.)
                if self.git(&["check-ignore", "-q", "target"]).is_err() {
                    args.push(":(exclude)target");
                }
            }
        }
        if args.len() > 3 {
            self.run(&args, true, Who::Human, None)?;
        }
        let tree = self.run(&["write-tree"], true, Who::Human, None)?;
        let _ = std::fs::remove_file(&self.index);
        Ok(tree.trim().to_string())
    }

    /// Tree of commit `base` with some files (relative paths) replaced.
    pub fn tree_with(&self, base: &str, files: &[(String, String)]) -> Result<String> {
        let _ = std::fs::remove_file(&self.index);
        self.run(&["read-tree", base], true, Who::Human, None)?;
        for (rel, content) in files {
            let blob = self.run(&["hash-object", "-w", "--stdin"], false, Who::Human, Some(content))?;
            let path = format!("{}{rel}", self.prefix);
            self.run(&["update-index", "--add", "--cacheinfo", &format!("100644,{},{path}", blob.trim())], true, Who::Human, None)?;
        }
        let tree = self.run(&["write-tree"], true, Who::Human, None)?;
        let _ = std::fs::remove_file(&self.index);
        Ok(tree.trim().to_string())
    }

    pub fn commit(&self, tree: &str, parents: &[&str], who: Who, msg: &str) -> Result<String> {
        // Record which code commit this journal state was made on.
        let msg = match self.code_head() {
            Some(head) if !msg.contains("\nCode-Head: ") => format!("{}\n\nCode-Head: {head}\n", msg.trim_end()),
            _ => msg.to_string(),
        };
        let mut args = vec!["commit-tree", tree, "-m", &msg];
        for p in parents {
            args.extend(["-p", p]);
        }
        Ok(self.run(&args, false, who, None)?.trim().to_string())
    }

    /// Commit the project directory as it is on disk.
    pub fn snapshot(&self, parents: &[&str], who: Who, msg: &str) -> Result<String> {
        let tree = self.tree(None)?;
        self.commit(&tree, parents, who, msg)
    }

    /// Tree of commit `rev` limited to the project (its directory, at its place in the repository),
    /// the shape of a journal snapshot.
    pub fn tree_at(&self, rev: &str) -> Result<String> {
        if self.prefix.is_empty() {
            return self.tree_of(rev).with_context(|| format!("{rev} has no tree"));
        }
        let _ = std::fs::remove_file(&self.index);
        self.run(&["read-tree", "--empty"], true, Who::Human, None)?;
        let sub = format!("{rev}:{}", self.prefix.trim_end_matches('/'));
        if self.resolve(&sub).is_some() {
            self.run(&["read-tree", &format!("--prefix={}", self.prefix), &sub], true, Who::Human, None)?;
        }
        let tree = self.run(&["write-tree"], true, Who::Human, None)?;
        let _ = std::fs::remove_file(&self.index);
        Ok(tree.trim().to_string())
    }

    /// The code commits after `base` up to HEAD, oldest first, along the first parent.
    pub fn first_parent_commits(&self, base: &str) -> Result<Vec<String>> {
        Ok(self.git(&["rev-list", "--reverse", "--first-parent", &format!("{base}..HEAD")])?.lines().map(String::from).collect())
    }

    /// A commit's subject and body.
    pub fn subject_body(&self, sha: &str) -> Result<(String, String)> {
        let out = self.git(&["log", "-1", "--format=%s%x1f%b", sha])?;
        let (s, b) = out.split_once('\x1f').unwrap_or((out.as_str(), ""));
        Ok((s.trim().to_string(), b.trim().to_string()))
    }

    /// A file of wish's own in the git directory (never in a snapshot), per project.
    pub fn private_file(&self, name: &str) -> PathBuf {
        self.index.with_file_name(format!("wish-{}{name}", self.prefix.replace('/', "_")))
    }

    pub fn tree_of(&self, sha: &str) -> Option<String> {
        self.resolve(&format!("{sha}^{{tree}}"))
    }

    /// A cross-process lock so two wish processes (e.g. two Zed windows on
    /// the same project) never run a cycle at the same time. Lives in .git,
    /// so it never ends up in a snapshot. Stale locks (> 15 min) are taken over.
    pub fn try_lock(&self) -> Option<CycleLock> {
        let path = self.index.with_file_name(format!("wish-cycle-{}.lock", self.prefix.replace('/', "_")));
        if let Ok(meta) = std::fs::metadata(&path) {
            let stale = meta.modified().ok().and_then(|m| m.elapsed().ok()).is_some_and(|age| age.as_secs() > 900);
            if !stale {
                return None;
            }
            let _ = std::fs::remove_file(&path);
        }
        std::fs::OpenOptions::new().write(true).create_new(true).open(&path).ok()?;
        Some(CycleLock(path))
    }

    /// Per-branch ref for the current branch: `refs/wish/<prefix><name>/<branch>`.
    pub fn ref_name(&self, name: &str) -> String {
        self.ref_for(name, &self.branch)
    }

    pub fn ref_for(&self, name: &str, branch: &str) -> String {
        format!("refs/wish/{}{name}/{branch}", self.prefix)
    }

    /// Keep a reverted attempt reachable without putting it in the journal.
    pub fn to_attic(&self, sha: &str) -> Result<()> {
        self.git(&["update-ref", &format!("refs/wish/{}attic/{}", self.prefix, &sha[..12.min(sha.len())]), sha]).map(|_| ())
    }

    /// Subjects of reverted attempts, newest first.
    pub fn attic(&self) -> Vec<String> {
        self.git(&["for-each-ref", "--sort=-committerdate", "--format=%(subject)", &format!("refs/wish/{}attic/", self.prefix)])
            .map(|o| o.lines().map(String::from).collect())
            .unwrap_or_default()
    }

    /// Every journal of this project: local ones for each branch, and ones
    /// fetched from remotes by `wish sync`.
    /// Branch -> the directory of the worktree that has it checked out (this checkout included).
    pub fn worktrees(&self) -> BTreeMap<String, PathBuf> {
        let mut out = BTreeMap::new();
        let Ok(o) = self.git(&["worktree", "list", "--porcelain"]) else { return out };
        let mut dir = None;
        for l in o.lines() {
            if let Some(d) = l.strip_prefix("worktree ") {
                dir = Some(PathBuf::from(d));
            } else if let (Some(b), Some(d)) = (l.strip_prefix("branch refs/heads/"), &dir) {
                out.insert(b.to_string(), d.clone());
            }
        }
        out
    }

    /// The project directory inside another checkout of the repository (the same `prefix` below it).
    pub fn project_in(&self, checkout: &Path) -> PathBuf {
        checkout.join(self.prefix.trim_end_matches('/'))
    }

    pub fn journals(&self) -> Vec<JournalRef> {
        let mut out = Vec::new();
        let local = format!("refs/wish/{}journal/", self.prefix);
        if let Ok(o) = self.git(&["for-each-ref", "--format=%(refname) %(objectname)", &local]) {
            for l in o.lines() {
                if let Some((r, sha)) = l.split_once(' ') {
                    out.push(JournalRef { branch: r.trim_start_matches(&local).to_string(), remote: None, sha: sha.into() });
                }
            }
        }
        if let Ok(o) = self.git(&["for-each-ref", "--format=%(refname) %(objectname)", "refs/wish-remote/"]) {
            for l in o.lines() {
                let Some((r, sha)) = l.split_once(' ') else { continue };
                let Some(rest) = r.strip_prefix("refs/wish-remote/") else { continue };
                let Some((remote, path)) = rest.split_once('/') else { continue };
                let Some(branch) = path.strip_prefix(&format!("{}journal/", self.prefix)) else { continue };
                out.push(JournalRef { branch: branch.into(), remote: Some(remote.into()), sha: sha.into() });
            }
        }
        out
    }

    /// The code tip a journal belongs to: the local branch, or the remote-tracking branch.
    pub fn code_tip(&self, j: &JournalRef) -> Option<String> {
        match &j.remote {
            None => self.resolve(&format!("refs/heads/{}", j.branch)),
            Some(r) => self.resolve(&format!("refs/remotes/{r}/{}", j.branch)),
        }
    }

    pub fn is_ancestor(&self, a: &str, b: &str) -> bool {
        self.git(&["merge-base", "--is-ancestor", a, b]).is_ok()
    }

    /// Commit time of the merge-base of two commits (0 if none).
    pub fn merge_base_time(&self, a: &str, b: &str) -> i64 {
        self.git(&["merge-base", a, b])
            .ok()
            .and_then(|mb| self.git(&["log", "-1", "--format=%ct", mb.trim()]).ok())
            .and_then(|t| t.trim().parse().ok())
            .unwrap_or(0)
    }

    /// Point another branch's journal at `sha` (and its log mirror).
    pub fn set_branch_journal(&self, branch: &str, sha: &str) -> Result<()> {
        self.git(&["update-ref", &self.ref_for("journal", branch), sha])?;
        self.git(&["update-ref", &format!("refs/heads/wish/{}log/{branch}", self.prefix), sha]).map(|_| ())
    }

    /// Push this project's journals (all branches) to a remote.
    pub fn push_journals(&self, remote: &str) -> Result<String> {
        let spec = format!("refs/wish/{}*:refs/wish/{}*", self.prefix, self.prefix);
        let out = Command::new("git").args(["push", remote, &spec]).current_dir(&self.root).output()?;
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        if !out.status.success() {
            bail!("git push failed:\n{}", text.trim());
        }
        Ok(text.trim().to_string())
    }

    /// Fetch a remote's journals into `refs/wish-remote/<remote>/…`.
    pub fn fetch_journals(&self, remote: &str) -> Result<()> {
        let spec = format!("+refs/wish/*:refs/wish-remote/{remote}/*");
        let out = Command::new("git").args(["fetch", remote, &spec]).current_dir(&self.root).output()?;
        if !out.status.success() {
            bail!("git fetch failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(())
    }

    pub fn resolve(&self, r: &str) -> Option<String> {
        self.git(&["rev-parse", "--verify", "--quiet", r]).ok().map(|s| s.trim().to_string())
    }

    pub fn set_ref(&self, r: &str, sha: &str) -> Result<()> {
        self.git(&["update-ref", r, sha])?;
        // Mirror the journal to a local branch so ordinary git tooling (Zed's
        // git graph, lazygit, `git log`) can browse what wish did.
        if r == self.ref_name("journal") {
            self.git(&["update-ref", &self.journal_branch(), sha])?;
        }
        Ok(())
    }

    /// `refs/heads/wish/<project-path>/log/<branch>`. Not `.../journal`, which
    /// would be ambiguous with `refs/wish/.../journal`.
    pub fn journal_branch(&self) -> String {
        format!("refs/heads/wish/{}log/{}", self.prefix, self.branch)
    }

    pub fn journal(&self) -> Option<String> {
        self.resolve(&self.ref_name("journal"))
    }

    pub fn last(&self) -> Option<String> {
        self.resolve(&self.ref_name("last"))
    }

    /// Unified diff between two snapshots, limited to `paths` (relative; empty = all).
    pub fn diff(&self, a: &str, b: &str, paths: &[String]) -> Result<String> {
        let mut args = vec!["diff", "--relative", "--no-color", a, b, "--"];
        args.extend(paths.iter().map(String::as_str));
        self.git(&args)
    }

    /// File content at a snapshot (path relative to the project root).
    pub fn show(&self, sha: &str, rel: &str) -> Option<String> {
        self.git(&["show", &format!("{sha}:./{rel}")]).ok()
    }

    /// Lines added and removed per file between two snapshots.
    pub fn numstat(&self, a: &str, b: &str) -> Result<Vec<FileStat>> {
        Ok(parse_numstat(&self.git(&["diff", "--relative", "--numstat", "--no-renames", a, b])?))
    }

    /// (status, path) for files that differ between two snapshots.
    pub fn changed(&self, a: &str, b: &str) -> Result<Vec<(char, String)>> {
        let out = self.git(&["diff", "--relative", "--name-status", "--no-renames", a, b])?;
        Ok(out
            .lines()
            .filter_map(|l| {
                let (st, path) = l.split_once('\t')?;
                Some((st.chars().next()?, path.to_string()))
            })
            .collect())
    }

    /// Who wrote each line of `contents` (the file's current text, saved or
    /// not), according to the journal. Lines not in the journal yet are being
    /// typed right now, so they're yours. None if there's no journal.
    pub fn blame(&self, rel: &str, contents: &str) -> Option<Vec<Who>> {
        Some(self.blame_lines(rel, contents)?.into_iter().map(|(w, _)| w).collect())
    }

    /// Like [`blame`](Self::blame), with the journal commit that introduced
    /// each line (None for lines not saved into the journal yet).
    pub fn blame_lines(&self, rel: &str, contents: &str) -> Option<Vec<(Who, Option<String>)>> {
        let journal = self.journal()?;
        let lines = contents.lines().count();
        if self.show(&journal, rel).is_none() {
            return Some(vec![(Who::Human, None); lines]); // a file you created since the last save
        }
        let out = self.run(&["blame", "--porcelain", "--contents", "-", &journal, "--", rel], false, Who::Human, Some(contents)).ok()?;
        // Blame follows lines through journal merges, so a line generated on a
        // feature branch stays Claude's (with that cycle's reasoning) after merging.
        let mut whos = std::collections::HashMap::new();
        let mut current = String::new();
        let mut result = Vec::with_capacity(lines);
        for l in out.lines() {
            if l.starts_with('\t') {
                let uncommitted = current.bytes().all(|b| b == b'0');
                let who = *whos.get(&current).unwrap_or(&Who::Human);
                result.push((who, (!uncommitted).then(|| current.clone())));
            } else if let Some(mail) = l.strip_prefix("author-mail ") {
                whos.insert(current.clone(), Who::from_mail(mail));
            } else if l.len() > 40 && l.as_bytes()[40] == b' ' && l[..40].bytes().all(|b| b.is_ascii_hexdigit()) {
                current = l[..40].to_string();
            }
        }
        Some(result)
    }

    /// A commit's full message and its age ("5 minutes ago").
    pub fn message(&self, sha: &str) -> Option<(String, String)> {
        let out = self.git(&["log", "-1", "--format=%B%x1f%ar", sha]).ok()?;
        let (msg, age) = out.rsplit_once('\x1f')?;
        Some((msg.trim().to_string(), age.trim().to_string()))
    }

    /// The journal's first-parent history, newest first.
    pub fn history(&self, rev: &str) -> Vec<Entry> {
        let Ok(out) = self.git(&["log", "--first-parent", "--relative", "--numstat", "--format=%x1e%H%x1f%P%x1f%ae%x1f%ar%x1f%s%x1f%b%x1f", rev]) else { return vec![] };
        out.split('\x1e')
            .filter_map(|rec| {
                let f: Vec<&str> = rec.trim_start_matches('\n').splitn(7, '\x1f').collect();
                let [sha, parents, mail, age, subject, body, stat] = f[..] else { return None };
                let mut parents = parents.split_whitespace();
                Some(Entry {
                    sha: sha.into(),
                    parent: parents.next().map(String::from),
                    side: parents.map(String::from).collect(),
                    who: Who::from_mail(mail),
                    subject: subject.into(),
                    body: body.trim().into(),
                    age: age.into(),
                    files: parse_numstat(stat),
                })
            })
            .collect()
    }

    /// Put the working tree back to snapshot `to`, undoing everything that
    /// changed since then (as recorded by snapshot `from`).
    pub fn restore(&self, to: &str, from: &str) -> Result<()> {
        self.restore_where(to, from, |_| true).map(|_| ())
    }

    /// Like [`restore`](Self::restore), limited to paths where `keep` is true.
    /// Returns the paths it changed.
    pub fn restore_where(&self, to: &str, from: &str, keep: impl Fn(&str) -> bool) -> Result<Vec<String>> {
        let mut back = Vec::new();
        let mut touched = Vec::new();
        for (st, path) in self.changed(to, from)? {
            // Build output is never restored or deleted, whatever a snapshot contains.
            if path == "target" || path.starts_with("target/") || !keep(&path) {
                continue;
            }
            touched.push(path.clone());
            if st == 'A' {
                let _ = std::fs::remove_file(self.root.join(&path));
            } else {
                back.push(path);
            }
        }
        if !back.is_empty() {
            let mut args = vec!["restore".to_string(), format!("--source={to}"), "--worktree".into(), "--".into()];
            args.extend(back);
            self.git(&args.iter().map(String::as_str).collect::<Vec<_>>())?;
        }
        Ok(touched)
    }

    /// Apply (or with `reverse`, un-apply) the changes between two snapshots
    /// to the working tree, limited to `paths` (relative to the project).
    pub fn apply_between(&self, a: &str, b: &str, paths: &[String], reverse: bool) -> Result<()> {
        let mut args = vec!["diff", "--binary", "--no-color", a, b, "--"];
        args.extend(paths.iter().map(String::as_str));
        let patch = self.git(&args)?;
        if patch.trim().is_empty() {
            return Ok(());
        }
        let top = self.git(&["rev-parse", "--show-toplevel"])?;
        let mut apply = vec!["-C", top.trim(), "apply", "--whitespace=nowarn"];
        if reverse {
            apply.push("-R");
        }
        self.run(&apply, false, Who::Human, Some(&patch)).map(|_| ())
    }

    /// First line of a commit's message.
    pub fn subject(&self, sha: &str) -> String {
        self.git(&["log", "-1", "--format=%s", sha]).map(|s| s.trim().to_string()).unwrap_or_default()
    }
}

/// Parse `git diff --numstat` lines (`added<TAB>removed<TAB>path`; binary files count as 0).
pub fn parse_numstat(text: &str) -> Vec<FileStat> {
    text.lines()
        .filter_map(|l| {
            let mut it = l.splitn(3, '\t');
            let (added, removed, path) = (it.next()?, it.next()?, it.next()?);
            Some(FileStat { path: path.into(), added: added.parse().unwrap_or(0), removed: removed.parse().unwrap_or(0) })
        })
        .collect()
}
