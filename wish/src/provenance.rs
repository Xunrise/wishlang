//! Line-level provenance: which lines you wrote and which Claude generated.
//!
//! Computed from `git blame` on the journal (see `git.rs`), so it needs no
//! state of its own, survives reformatting of untouched lines, and covers
//! unsaved buffers (lines not in the journal yet are yours).

use crate::git::{Git, Who};

/// A run of consecutive lines with the same author (0-based, inclusive).
#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub start: usize,
    pub end: usize,
    pub who: Who,
}

pub fn of(git: &Git, rel: &str, contents: &str) -> Option<Vec<Who>> {
    git.blame(rel, contents)
}

/// Group lines into blocks. Blank lines join the block they sit in.
pub fn blocks(whos: &[Who], contents: &str) -> Vec<Block> {
    let lines: Vec<&str> = contents.lines().collect();
    let mut out: Vec<Block> = Vec::new();
    for (i, who) in whos.iter().enumerate() {
        let blank = lines.get(i).is_none_or(|l| l.trim().is_empty());
        match out.last_mut() {
            Some(b) if b.who == *who || blank => b.end = i,
            _ => out.push(Block { start: i, end: i, who: *who }),
        }
    }
    out
}

/// A line worth protecting: has real content, not just braces or punctuation.
fn meaningful(line: &str) -> bool {
    line.chars().any(|c| c.is_alphanumeric())
}

/// Your meaningful lines in `contents`, trimmed.
pub fn human_lines(whos: &[Who], contents: &str) -> Vec<String> {
    contents.lines().zip(whos).filter(|(l, w)| **w == Who::Human && meaningful(l)).map(|(l, _)| l.trim().to_string()).collect()
}

fn squash(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Does `new` still carry your line `human`? Whitespace and formatting may
/// change, a trailing comma may be added or dropped, and an empty body `{}`
/// you wrote may be expanded (`pub enum Color {}` → `pub enum Color {` with
/// generated content below): you wrote the declaration, Claude fills it in.
pub fn carries(human: &str, new: &str) -> bool {
    let (h, n) = (squash(human), squash(new));
    if h == n || h.trim_end_matches(',') == n.trim_end_matches(',') {
        return true;
    }
    h.strip_suffix("{}").is_some_and(|stem| n == format!("{stem}{{"))
}

/// Which of your lines are missing from `new`. Each of your lines must be
/// carried (see [`carries`]) by a distinct line of `new`, so Claude may move
/// and reformat code around them but not drop them.
pub fn missing(human: &[String], new: &str) -> Vec<String> {
    let lines: Vec<&str> = new.lines().collect();
    let mut used = vec![false; lines.len()];
    let mut out = Vec::new();
    for h in human {
        // Prefer an exact match, then a relaxed one.
        let pick = (0..lines.len()).find(|&i| !used[i] && lines[i].trim() == h.trim()).or_else(|| (0..lines.len()).find(|&i| !used[i] && carries(h, lines[i])));
        match pick {
            Some(i) => used[i] = true,
            None => out.push(h.clone()),
        }
    }
    out
}

/// Your lines that Claude reformatted or expanded: a copy of `old` in which
/// each such line is replaced by its new form, or None if nothing changed.
/// Committed (as you) before Claude's cycle, this keeps those lines yours in
/// blame, while whatever Claude added around them stays Claude's.
pub fn reformatted(old: &str, whos: &[Who], new: &str) -> Option<String> {
    let new_lines: Vec<&str> = new.lines().collect();
    let mut used = vec![false; new_lines.len()];
    let mut out: Vec<String> = Vec::new();
    let mut changed = false;
    for (l, who) in old.lines().zip(whos.iter().chain(std::iter::repeat(&Who::Human))) {
        if *who == Who::Human && meaningful(l) {
            if let Some(i) = (0..new_lines.len()).find(|&i| !used[i] && new_lines[i].trim() == l.trim()) {
                used[i] = true;
            } else if let Some(i) = (0..new_lines.len()).find(|&i| !used[i] && carries(l, new_lines[i])) {
                used[i] = true;
                out.push(new_lines[i].to_string());
                changed = true;
                continue;
            }
        }
        out.push(l.to_string());
    }
    changed.then(|| out.join("\n") + "\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use Who::*;

    #[test]
    fn grouping() {
        let src = "a\nb\n\nc\nd\n";
        let b = blocks(&[Claude, Claude, Human, Human, Claude], src);
        assert_eq!(b, [Block { start: 0, end: 2, who: Claude }, Block { start: 3, end: 3, who: Human }, Block { start: 4, end: 4, who: Claude }]);
    }

    #[test]
    fn expanding_your_declaration_is_fine() {
        let human = vec!["pub enum Color {}".to_string()];
        let new = "pub enum Color {\n    Red,\n    Green,\n}\n";
        assert!(missing(&human, new).is_empty());
        assert!(missing(&vec!["let x = foo(1,2);".to_string()], "let x = foo(1, 2);").is_empty());
        assert_eq!(missing(&human, "pub enum Colour {\n}\n"), ["pub enum Color {}"]);
        let old = "/// gen\npub enum Color {}\n";
        assert_eq!(reformatted(old, &[Claude, Human], new).unwrap(), "/// gen\npub enum Color {\n");
        assert!(reformatted(old, &[Claude, Human], "/// gen\npub enum Color {}\n").is_none());
    }

    #[test]
    fn your_lines_must_survive() {
        let human = human_lines(&[Claude, Human, Human], "fn x() {\n    // fade in slowly\n}\n");
        assert_eq!(human, ["// fade in slowly"]); // `}` isn't worth protecting
        assert!(missing(&human, "fn y() {\n  // fade in slowly\n  go();\n}").is_empty());
        assert_eq!(missing(&human, "fn y() {}"), ["// fade in slowly"]);
    }
}
