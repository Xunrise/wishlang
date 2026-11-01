//! The user's project: `wish.toml`, file ownership, and wishlang state.
//!
//! Ownership model:
//! - `[protected] files`: the user's files. Claude may only *add* single-line
//!   `mod …;` / `use …;` wiring to them (see [`wiring_only`]).
//! - `[adopted]`: items the user trusts; hashed, Claude may not change them.
//! - every other `src/**/*.rs` plus `Cargo.toml`: Claude-owned.
//! - `wish.toml`, `DECISIONS.md`, `.wish/`: tool-owned, Claude may not touch.

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub protected: Protected,
    #[serde(default)]
    pub adopted: BTreeMap<String, Adopted>,
    #[serde(default)]
    pub constraints: Constraints,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Protected {
    pub files: Vec<String>,
}
impl Default for Protected {
    fn default() -> Self {
        Self { files: vec!["src/main.rs".into()] }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Adopted {
    pub file: String,
    pub hash: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Constraints {
    #[serde(default)]
    pub rules: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Proposed,
    Accepted,
    Rejected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    pub id: u32,
    pub kind: String,
    pub title: String,
    pub choice: String,
    #[serde(default)]
    pub alternatives: Vec<String>,
    pub why: String,
    #[serde(default)]
    pub crates: Vec<String>,
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Suggestion {
    pub id: u32,
    pub file: String,
    pub reason: String,
    pub replace: String,
    pub with: String,
    pub status: Status,
}

pub struct Project {
    pub root: PathBuf,
    pub cfg: Config,
}

impl Project {
    pub fn discover() -> Result<Self> {
        let cwd = std::env::current_dir()?;
        Self::discover_from(&cwd).ok_or_else(|| anyhow!("no wish.toml found here or in any parent (run `wish init` in a cargo project)"))
    }

    pub fn discover_from(start: &Path) -> Option<Self> {
        let mut dir = Some(start);
        while let Some(d) = dir {
            if d.join("wish.toml").is_file() {
                return Self::load(d).ok();
            }
            dir = d.parent();
        }
        None
    }

    pub fn load(root: &Path) -> Result<Self> {
        let text = fs::read_to_string(root.join("wish.toml"))?;
        let cfg = toml::from_str(&text).context("parsing wish.toml")?;
        Ok(Self { root: root.to_path_buf(), cfg })
    }

    pub fn save(&self) -> Result<()> {
        let mut text = String::from("# wishlang project file. Edit [protected] and [constraints] freely;\n# [adopted] is maintained by `wish adopt` / `wish unadopt`.\n\n");
        text.push_str(&toml::to_string_pretty(&self.cfg)?);
        fs::write(self.root.join("wish.toml"), text)?;
        Ok(())
    }

    pub fn state_dir(&self) -> PathBuf {
        self.root.join(".wish")
    }

    /// Path relative to the project root, with forward slashes, if inside it.
    pub fn rel(&self, p: &Path) -> Option<String> {
        let abs = if p.is_absolute() { p.to_path_buf() } else { self.root.join(p) };
        let abs = normalize(&abs);
        abs.strip_prefix(normalize(&self.root)).ok().map(|r| r.to_string_lossy().replace('\\', "/"))
    }

    pub fn is_protected(&self, rel: &str) -> bool {
        self.cfg.protected.files.iter().any(|f| f == rel)
    }

    pub fn is_tool_owned(&self, rel: &str) -> bool {
        rel == "wish.toml" || rel == "DECISIONS.md" || rel == ".wish" || rel.starts_with(".wish/")
    }

    pub fn read(&self, rel: &str) -> Option<String> {
        fs::read_to_string(self.root.join(rel)).ok()
    }

    /// All `.rs` files under `src/` (relative paths), sorted.
    pub fn rust_files(&self) -> Vec<String> {
        let mut out = Vec::new();
        walk(&self.root.join("src"), &mut out);
        let mut rels: Vec<String> = out.iter().filter_map(|p| self.rel(p)).collect();
        rels.sort();
        rels
    }

    /// Claude-owned files and their contents (Rust sources + Cargo.toml).
    pub fn owned_files(&self) -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        for f in self.rust_files() {
            if !self.is_protected(&f) {
                if let Some(s) = self.read(&f) {
                    m.insert(f, s);
                }
            }
        }
        if let Some(s) = self.read("Cargo.toml") {
            m.insert("Cargo.toml".into(), s);
        }
        m
    }

    pub fn protected_files(&self) -> BTreeMap<String, String> {
        self.cfg.protected.files.iter().map(|f| (f.clone(), self.read(f).unwrap_or_default())).collect()
    }

    /// Locate an item by path in any Rust file.
    pub fn find_item(&self, path: &str) -> Result<(String, crate::items::Item)> {
        for f in self.rust_files() {
            let Some(src) = self.read(&f) else { continue };
            if let Ok(items) = crate::items::parse(&src) {
                if let Some(i) = items.into_iter().find(|i| i.path == path) {
                    return Ok((f, i));
                }
            }
        }
        bail!("no item `{path}` found (try `wish items`)")
    }

    /// Check every adopted item still hashes the same. Returns violations.
    pub fn verify_adopted(&self) -> Vec<String> {
        let mut bad = Vec::new();
        for (path, a) in &self.cfg.adopted {
            match self.read(&a.file).map(|s| crate::items::parse(&s)) {
                Some(Ok(items)) => match items.iter().find(|i| &i.path == path) {
                    Some(i) if i.hash == a.hash => {}
                    Some(_) => bad.push(format!("adopted `{path}` was modified in {}", a.file)),
                    None => bad.push(format!("adopted `{path}` is missing from {}", a.file)),
                },
                Some(Err(_)) => bad.push(format!("{} no longer parses (contains adopted `{path}`)", a.file)),
                None => bad.push(format!("{} is gone (contained adopted `{path}`)", a.file)),
            }
        }
        bad
    }

    // ---- decisions & suggestions -------------------------------------------

    pub fn decisions(&self) -> Vec<Decision> {
        load_json(&self.state_dir().join("decisions.json"))
    }

    pub fn save_decisions(&self, d: &[Decision]) -> Result<()> {
        save_json(&self.state_dir().join("decisions.json"), d)?;
        fs::write(self.root.join("DECISIONS.md"), render_decisions(d))?;
        Ok(())
    }

    pub fn suggestions(&self) -> Vec<Suggestion> {
        load_json(&self.state_dir().join("suggestions.json"))
    }

    pub fn save_suggestions(&self, s: &[Suggestion]) -> Result<()> {
        save_json(&self.state_dir().join("suggestions.json"), s)
    }

    /// Crates in `[dependencies]` without a non-rejected decision naming them.
    pub fn undecided_dependencies(&self) -> Vec<String> {
        let Some(text) = self.read("Cargo.toml") else { return vec![] };
        let Ok(doc) = text.parse::<toml::Table>() else { return vec![] };
        let deps: Vec<String> = doc.get("dependencies").and_then(|d| d.as_table()).map(|t| t.keys().cloned().collect()).unwrap_or_default();
        let decided: Vec<String> = self.decisions().into_iter().filter(|d| d.status != Status::Rejected).flat_map(|d| d.crates).collect();
        deps.into_iter().filter(|d| !decided.iter().any(|c| c == d)).collect()
    }
}

/// True if `new` differs from `old` only by *inserted* lines that are blank or
/// single-line `mod x;` / `use …;` declarations. This is the one change Claude
/// may make to a protected file: wiring it up to the generated modules.
pub fn wiring_only(old: &str, new: &str) -> Result<(), String> {
    let re = regex::Regex::new(r"^\s*(pub(\([^)]*\))?\s+)?(mod\s+\w+|use\s+[^;]+);\s*$").unwrap();
    let diff = similar::TextDiff::from_lines(old, new);
    for ch in diff.iter_all_changes() {
        let line = ch.value().trim_end_matches('\n');
        match ch.tag() {
            similar::ChangeTag::Equal => {}
            similar::ChangeTag::Delete => return Err(format!("removes or changes the line `{}`", line.trim())),
            similar::ChangeTag::Insert => {
                if !(line.trim().is_empty() || re.is_match(line)) {
                    return Err(format!("adds the line `{}` (only single-line `mod`/`use` wiring may be added)", line.trim()));
                }
            }
        }
    }
    Ok(())
}

fn render_decisions(ds: &[Decision]) -> String {
    let mut s = String::from("# Decisions\n\nArchitectural and dependency decisions made while implementing this project.\nGenerated by wishlang from `.wish/decisions.json`; change status with `wish decide <id> accept|reject`.\n");
    for d in ds {
        let status = match d.status {
            Status::Proposed => "proposed",
            Status::Accepted => "accepted",
            Status::Rejected => "rejected",
        };
        s.push_str(&format!("\n## {:03} {} ({status})\n\n- **Kind:** {}\n- **Choice:** {}\n", d.id, d.title, d.kind, d.choice));
        if !d.crates.is_empty() {
            s.push_str(&format!("- **Crates:** {}\n", d.crates.join(", ")));
        }
        if !d.alternatives.is_empty() {
            s.push_str(&format!("- **Alternatives:** {}\n", d.alternatives.join("; ")));
        }
        s.push_str(&format!("- **Why:** {}\n", d.why));
        if let Some(r) = &d.reason {
            s.push_str(&format!("- **Rejected because:** {r}\n"));
        }
    }
    s
}

fn load_json<T: for<'de> Deserialize<'de> + Default>(p: &Path) -> T {
    fs::read_to_string(p).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn save_json<T: Serialize + ?Sized>(p: &Path, v: &T) -> Result<()> {
    fs::create_dir_all(p.parent().unwrap())?;
    fs::write(p, serde_json::to_string_pretty(v)?)?;
    Ok(())
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::wiring_only;

    #[test]
    fn wiring() {
        let old = "fn main() {\n    go()\n}\n";
        assert!(wiring_only(old, "mod app;\nuse app::*;\n\nfn main() {\n    go()\n}\n").is_ok());
        assert!(wiring_only(old, "fn main() {\n    go2()\n}\n").is_err());
        assert!(wiring_only(old, "fn helper() {}\nfn main() {\n    go()\n}\n").is_err());
        assert!(wiring_only(old, "// hi\nfn main() {\n    go()\n}\n").is_err());
    }
}
