//! Git as the source of truth for "what changed".
//!
//! Every cycle snapshots the project directory into a git tree/commit using a
//! *temporary index*, so the user's branch, index and HEAD are never touched.
//! The last green cycle lives under a hidden ref, `refs/wish/<prefix>last`,
//! whose parent chain is the history of cycles (`git log refs/wish/last`).

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

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

    fn git(&self, args: &[&str], temp_index: bool) -> Result<String> {
        let mut c = Command::new("git");
        c.args(args).current_dir(&self.root);
        if temp_index {
            c.env("GIT_INDEX_FILE", &self.index);
        }
        c.env("GIT_AUTHOR_NAME", "wishlang").env("GIT_AUTHOR_EMAIL", "wish@localhost");
        c.env("GIT_COMMITTER_NAME", "wishlang").env("GIT_COMMITTER_EMAIL", "wish@localhost");
        let out = c.output()?;
        if !out.status.success() {
            bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    /// Commit the project directory as it is on disk (honouring .gitignore).
    pub fn snapshot(&self, parent: Option<&str>, msg: &str) -> Result<String> {
        let _ = std::fs::remove_file(&self.index);
        // Build output never belongs in a snapshot, even without a .gitignore.
        // (Git errors on an exclude pathspec naming an already-ignored path,
        // so only add it when target/ isn't ignored.)
        let mut args = vec!["add", "-A", "--", "."];
        if self.git(&["check-ignore", "-q", "target"], false).is_err() {
            args.push(":(exclude)target");
        }
        self.git(&args, true)?;
        let tree = self.git(&["write-tree"], true)?;
        let _ = std::fs::remove_file(&self.index);
        let mut args = vec!["commit-tree", tree.trim(), "-m", msg];
        if let Some(p) = parent {
            args.extend(["-p", p]);
        }
        Ok(self.git(&args, false)?.trim().to_string())
    }

    pub fn ref_name(&self, name: &str) -> String {
        format!("refs/wish/{}{name}", self.prefix)
    }

    pub fn resolve(&self, r: &str) -> Option<String> {
        self.git(&["rev-parse", "--verify", "--quiet", r], false).ok().map(|s| s.trim().to_string())
    }

    pub fn set_ref(&self, r: &str, sha: &str) -> Result<()> {
        self.git(&["update-ref", r, sha], false).map(|_| ())
    }

    /// Unified diff between two snapshots, limited to `paths` (relative).
    pub fn diff(&self, a: &str, b: &str, paths: &[String]) -> Result<String> {
        let mut args = vec!["diff", "--relative", "--no-color", a, b, "--"];
        args.extend(paths.iter().map(String::as_str));
        self.git(&args, false)
    }

    /// File content at a snapshot (path relative to the project root).
    pub fn show(&self, sha: &str, rel: &str) -> Option<String> {
        self.git(&["show", &format!("{sha}:./{rel}")], false).ok()
    }

    /// (status, path) for files that differ between two snapshots.
    pub fn changed(&self, a: &str, b: &str) -> Result<Vec<(char, String)>> {
        let out = self.git(&["diff", "--relative", "--name-status", "--no-renames", a, b], false)?;
        Ok(out
            .lines()
            .filter_map(|l| {
                let (st, path) = l.split_once('\t')?;
                Some((st.chars().next()?, path.to_string()))
            })
            .collect())
    }

    /// Put the working tree back to snapshot `to`, undoing everything that
    /// changed since then (as recorded by snapshot `from`).
    pub fn restore(&self, to: &str, from: &str) -> Result<()> {
        let mut back = Vec::new();
        for (st, path) in self.changed(to, from)? {
            if st == 'A' {
                let _ = std::fs::remove_file(self.root.join(&path));
            } else {
                back.push(path);
            }
        }
        if !back.is_empty() {
            let mut args = vec!["restore".to_string(), format!("--source={to}"), "--worktree".into(), "--".into()];
            args.extend(back);
            self.git(&args.iter().map(String::as_str).collect::<Vec<_>>(), false)?;
        }
        Ok(())
    }
}
