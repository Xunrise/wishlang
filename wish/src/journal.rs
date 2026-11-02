//! Journal commit messages: the reasoning behind generated code.
//!
//! Every Claude cycle is committed to the journal with a structured message,
//! readable in `git log` and parsed back by the hover (blame → commit → why):
//!
//! ```text
//! cycle ok: Added Mode::Windowed
//!
//! Added `Mode::Windowed(width, height)` …
//!
//! Why:
//! - Mode::Windowed: main.rs now calls Mode::Windowed(800, 600).
//!
//! Triggered-by:
//! src/main.rs: +    App::new("Demo").window().open(Mode::Windowed(800, 600)).run()
//!
//! Decisions:
//! - 3 (proposed) Software painting: softbuffer 0.4
//!
//! Mode: prototype
//! ```
//!
//! A cycle for a ticket (see `ticket.rs`) tags every item with the problems it
//! serves and names the ticket: `- Mode::Windowed [P1,P3]: why` (`[]` for an
//! item that serves none) and a final `Ticket: KEY` line.

/// The reason for one item of a cycle.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Why {
    pub item: String,
    pub why: String,
    /// Ticket problems the item serves (empty outside ticket mode).
    pub problems: Vec<String>,
}

impl Why {
    pub fn new(item: &str, why: &str) -> Self {
        Self { item: item.into(), why: why.into(), problems: vec![] }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CycleMessage {
    /// "cycle ok", "implemented", "cycle failed", "save", "undo", ...
    pub label: String,
    pub summary: String,
    pub why: Vec<Why>,
    /// Your diff lines that triggered the cycle, as "file: +line".
    pub triggered_by: Vec<String>,
    pub decisions: Vec<String>,
    pub mode: String,
    /// The ticket key, for a cycle that traced its items to a ticket's problems.
    pub ticket: Option<String>,
}

impl CycleMessage {
    pub fn render(&self) -> String {
        let first = self.summary.lines().next().unwrap_or("").trim();
        let mut subject: String = first.chars().take(72).collect();
        if subject.len() < first.len() {
            subject.push('…');
        }
        let mut s = format!("{}: {subject}\n", self.label);
        if !self.summary.trim().is_empty() {
            s.push_str(&format!("\n{}\n", self.summary.trim()));
        }
        if !self.why.is_empty() {
            s.push_str("\nWhy:\n");
            for w in &self.why {
                let tag = if self.ticket.is_some() { format!(" [{}]", w.problems.join(",")) } else { String::new() };
                s.push_str(&format!("- {}{tag}: {}\n", w.item, w.why.replace('\n', " ")));
            }
        }
        if !self.triggered_by.is_empty() {
            s.push_str("\nTriggered-by:\n");
            for l in &self.triggered_by {
                s.push_str(&format!("{l}\n"));
            }
        }
        if !self.decisions.is_empty() {
            s.push_str("\nDecisions:\n");
            for d in &self.decisions {
                s.push_str(&format!("- {d}\n"));
            }
        }
        if !self.mode.is_empty() {
            s.push_str(&format!("\nMode: {}\n", self.mode));
        }
        if let Some(key) = &self.ticket {
            s.push_str(&format!("Ticket: {key}\n"));
        }
        s
    }

    pub fn parse(msg: &str) -> Self {
        let mut m = CycleMessage::default();
        let mut lines = msg.lines();
        if let Some(subject) = lines.next() {
            m.label = subject.split_once(": ").map(|x| x.0).unwrap_or(subject).to_string();
        }
        #[derive(PartialEq)]
        enum Sec {
            Summary,
            Why,
            Trigger,
            Decisions,
        }
        let mut sec = Sec::Summary;
        let mut summary = Vec::new();
        for l in lines {
            match l.trim_end() {
                "Why:" => sec = Sec::Why,
                "Triggered-by:" => sec = Sec::Trigger,
                "Decisions:" => sec = Sec::Decisions,
                t if t.starts_with("Mode: ") => m.mode = t[6..].to_string(),
                t if t.starts_with("Ticket: ") => m.ticket = Some(t[8..].to_string()),
                t if t.starts_with("Code-Head: ") => {}
                "" if sec != Sec::Summary => {}
                t => match sec {
                    Sec::Summary => summary.push(t),
                    Sec::Why => {
                        if let Some((item, why)) = t.strip_prefix("- ").and_then(|x| x.split_once(": ")) {
                            m.why.push(Why::new(item, why));
                        }
                    }
                    Sec::Trigger => m.triggered_by.push(t.to_string()),
                    Sec::Decisions => m.decisions.push(t.strip_prefix("- ").unwrap_or(t).to_string()),
                },
            }
        }
        m.summary = summary.join("\n").trim().to_string();
        // Tags exist only in ticket cycles (elsewhere `[u8]` may end an item path).
        if m.ticket.is_some() {
            let tag = regex::Regex::new(r"\s\[((?:[\w-]+(?:,[\w-]+)*)?)\]$").unwrap();
            for w in &mut m.why {
                if let Some(c) = tag.captures(&w.item) {
                    w.problems = c[1].split(',').filter(|p| !p.is_empty()).map(String::from).collect();
                    w.item.truncate(c.get(0).unwrap().start());
                }
            }
        }
        m
    }

    /// The reason for an item, trying the item itself (exactly, then up to
    /// its parameter list, see `same_item`) and then its parents (`Mode::Windowed`
    /// falls back to `Mode`).
    pub fn why_for(&self, item: &str) -> Option<&str> {
        item_chain(item).into_iter().find_map(|p| self.why.iter().find(|w| w.item == p).or_else(|| self.why.iter().find(|w| same_item(&w.item, p))).map(|w| w.why.as_str()))
    }
}

/// Whether a journal item path names a parsed item: the same path, or the same
/// path up to the parameter list when either side has none (a journal
/// `App.create(String)` for the item `App.create`, a journal `Window.open` for
/// the overloads `Window.open(int, int)`). Whitespace inside the parentheses
/// does not matter.
pub fn same_item(journal: &str, item: &str) -> bool {
    let (a, b) = (compact(journal), compact(item));
    a == b || ((!a.contains('(') || !b.contains('(')) && a.split('(').next() == b.split('(').next())
}

/// The path with the whitespace inside its parameter list removed.
fn compact(path: &str) -> String {
    match path.split_once('(') {
        Some((head, params)) => format!("{head}({}", params.split_whitespace().collect::<String>()),
        None => path.to_string(),
    }
}

/// An item path and its fallbacks, most specific first: the path itself, its
/// bare name when it is an overload, then each parent.
pub fn item_chain(item: &str) -> Vec<&str> {
    let mut chain = vec![item];
    let mut path = item.split('(').next().unwrap_or(item);
    loop {
        if chain.last() != Some(&path) {
            chain.push(path);
        }
        match path.rfind("::").into_iter().chain(path.rfind('.')).max() {
            Some(cut) => path = &path[..cut],
            None => return chain,
        }
    }
}

/// Turn a unified diff into "file: +line" / "file: -line" entries (max `cap`).
pub fn trigger_lines(diff: &str, cap: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut file = String::new();
    let mut extra = 0;
    for l in diff.lines() {
        if let Some(f) = l.strip_prefix("+++ ") {
            file = f.trim_start_matches("b/").to_string();
        } else if l.starts_with("--- ") || l.starts_with("diff ") || l.starts_with("index ") || l.starts_with("@@") {
        } else if (l.starts_with('+') || l.starts_with('-')) && !l[1..].trim().is_empty() {
            if out.len() < cap {
                let line: String = l.chars().take(160).collect();
                out.push(format!("{file}: {line}"));
            } else {
                extra += 1;
            }
        }
    }
    if extra > 0 {
        out.push(format!("(+{extra} more lines)"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let m = CycleMessage {
            label: "cycle ok".into(),
            summary: "Added `Mode::Windowed`.\nSecond line.".into(),
            why: vec![Why::new("Mode::Windowed", "main.rs now calls Mode::Windowed(800, 600)."), Why::new("impl ApplicationHandler for Handler", "applies the size")],
            triggered_by: vec!["src/main.rs: +    open(Mode::Windowed(800, 600))".into()],
            decisions: vec!["3 (proposed) softbuffer".into()],
            mode: "prototype".into(),
            ticket: None,
        };
        let text = m.render();
        assert!(text.starts_with("cycle ok: Added `Mode::Windowed`.\n"));
        assert_eq!(CycleMessage::parse(&text), m);
    }

    #[test]
    fn why_falls_back_to_parent() {
        let m = CycleMessage { why: vec![Why::new("Mode", "because")], ..Default::default() };
        assert_eq!(m.why_for("Mode::Windowed"), Some("because"));
        assert_eq!(m.why_for("App::new"), None);
    }

    #[test]
    fn why_falls_back_for_overloads() {
        let m = CycleMessage { why: vec![Why::new("Window.open", "bare"), Why::new("Window.open(int)", "exact"), Why::new("Mode", "parent")], ..Default::default() };
        assert_eq!(m.why_for("Window.open(int)"), Some("exact"));
        assert_eq!(m.why_for("Window.open(int, int)"), Some("bare"));
        assert_eq!(m.why_for("Mode.WINDOWED"), Some("parent"));
        assert_eq!(m.why_for("Window.close(int)"), None);
    }

    #[test]
    fn triggers() {
        let d = "diff --git a/src/main.rs b/src/main.rs\nindex 1..2\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1 +1 @@\n-    a()\n+    b()\n \n";
        assert_eq!(trigger_lines(d, 20), ["src/main.rs: -    a()", "src/main.rs: +    b()"]);
    }

    #[test]
    fn ticket_tags_roundtrip() {
        let why = |item: &str, ps: &[&str]| Why { item: item.into(), why: "because".into(), problems: ps.iter().map(|s| s.to_string()).collect() };
        let m = CycleMessage {
            label: "cycle ok".into(),
            summary: "Added windows.".into(),
            why: vec![why("Window.open(int[], int)", &["P1", "P3"]), why("Mode", &[]), why("impl From<A> for [u8]", &["P2"])],
            mode: "prototype".into(),
            ticket: Some("DEMO-1".into()),
            ..Default::default()
        };
        let text = m.render();
        assert!(text.contains("- Window.open(int[], int) [P1,P3]: because\n"));
        assert!(text.contains("- Mode []: because\n"));
        assert!(text.ends_with("Mode: prototype\nTicket: DEMO-1\n"));
        assert_eq!(CycleMessage::parse(&text), m);
    }

    #[test]
    fn old_format_has_no_tags_and_no_ticket() {
        // Without a `Ticket:` line a trailing `[..]` belongs to the item, and an empty list is not a tag.
        let m = CycleMessage::parse("cycle ok: x\n\nWhy:\n- impl From<A> for [u8]: because\n- Mode: why\n\nMode: prototype\n");
        assert_eq!(m.ticket, None);
        assert_eq!(m.why, [Why::new("impl From<A> for [u8]", "because"), Why::new("Mode", "why")]);
        // A ticket cycle's `[]` and an old entry's missing tag both parse to no problems, but only one is in ticket mode.
        let t = CycleMessage::parse("cycle ok: x\n\nWhy:\n- Mode []: why\n\nTicket: DEMO-1\n");
        assert_eq!((t.why[0].problems.is_empty(), t.ticket.as_deref()), (true, Some("DEMO-1")));
        assert_eq!(t.why[0].item, "Mode");
    }

    #[test]
    fn items_match_up_to_their_parameter_list() {
        assert!(same_item("App.create(String)", "App.create"));
        assert!(same_item("Window.open", "Window.open(Mode)"));
        assert!(same_item("Window.open(int,int)", "Window.open(int, int)"));
        assert!(!same_item("Window.open(int)", "Window.open(int, int)"));
        assert!(!same_item("Window.open", "Window.openAll"));
        let m = CycleMessage { why: vec![Why { item: "App.create(String)".into(), why: "journal".into(), problems: vec![] }], ..Default::default() };
        assert_eq!(m.why_for("App.create"), Some("journal"));
    }

    #[test]
    fn item_chain_lists_fallbacks() {
        assert_eq!(item_chain("Window.open(int, java.util.List)"), ["Window.open(int, java.util.List)", "Window.open", "Window"]);
        assert_eq!(item_chain("Mode::Windowed"), ["Mode::Windowed", "Mode"]);
        assert_eq!(item_chain("Mode"), ["Mode"]);
    }
}
