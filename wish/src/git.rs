//! Git as the source of truth for "what changed" and "who wrote it".
//!
//! Snapshots are commits built with a *temporary index*, so the user's
//! branch, index and HEAD are never touched. Two hidden refs per project:
//!
//! - `refs/wish/<prefix>journal`: every save (authored by **you**) and every
//!   Claude cycle (authored by **claude**). Its first-parent chain is the
//!   current line of history; reverted attempts and undone cycles hang off as
//!   second parents, so they stay in the history without affecting blame.
//!   `git blame --first-parent` on it gives line-level provenance.
//! - `refs/wish/<prefix>last`: the last green cycle (a journal commit). Your
//!   edits since then are the regeneration scope.

use anyhow::{Context, Result, bail};
use std::io::Write;
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
    fn from_mail(mail: &str) -> Who {
        if mail.contains("claude@wishlang") { Who::Claude } else { Who::Human }
    }
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub sha: String,
    pub who: Who,
    pub subject: String,
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
}

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
        Ok(Self { root: root.to_path_buf(), prefix, index })
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
        let mut args = vec!["commit-tree", tree, "-m", msg];
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

    pub fn ref_name(&self, name: &str) -> String {
        format!("refs/wish/{}{name}", self.prefix)
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

    /// `refs/heads/wish/log` (or `refs/heads/wish/<project-path>/log`). Not
    /// `.../journal`, which would be ambiguous with `refs/wish/.../journal`.
    pub fn journal_branch(&self) -> String {
        format!("refs/heads/wish/{}log", self.prefix)
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
        let out = self.run(&["blame", "--first-parent", "--porcelain", "--contents", "-", &journal, "--", rel], false, Who::Human, Some(contents)).ok()?;
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
        let Ok(out) = self.git(&["log", "--first-parent", "--format=%H%x1f%P%x1f%ae%x1f%s%x1f%b%x1e", rev]) else { return vec![] };
        out.split('\x1e')
            .filter_map(|rec| {
                let f: Vec<&str> = rec.trim_start_matches('\n').split('\x1f').collect();
                if f.len() < 5 {
                    return None;
                }
                Some(Entry { sha: f[0].into(), parent: f[1].split_whitespace().next().map(String::from), side: f[1].split_whitespace().skip(1).map(String::from).collect(), who: Who::from_mail(f[2]), subject: f[3].into() })
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
