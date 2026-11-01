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

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CycleMessage {
    /// "cycle ok", "implemented", "cycle failed", "save", "undo", ...
    pub label: String,
    pub summary: String,
    /// (item path, why)
    pub why: Vec<(String, String)>,
    /// Your diff lines that triggered the cycle, as "file: +line".
    pub triggered_by: Vec<String>,
    pub decisions: Vec<String>,
    pub mode: String,
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
            for (item, why) in &self.why {
                s.push_str(&format!("- {item}: {}\n", why.replace('\n', " ")));
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
                t if t.starts_with("Code-Head: ") => {}
                "" if sec != Sec::Summary => {}
                t => match sec {
                    Sec::Summary => summary.push(t),
                    Sec::Why => {
                        if let Some((item, why)) = t.strip_prefix("- ").and_then(|x| x.split_once(": ")) {
                            m.why.push((item.to_string(), why.to_string()));
                        }
                    }
                    Sec::Trigger => m.triggered_by.push(t.to_string()),
                    Sec::Decisions => m.decisions.push(t.strip_prefix("- ").unwrap_or(t).to_string()),
                },
            }
        }
        m.summary = summary.join("\n").trim().to_string();
        m
    }

    /// The reason for an item, trying the item itself and then its parents
    /// (`Mode::Windowed` falls back to `Mode`).
    pub fn why_for(&self, item: &str) -> Option<&str> {
        let mut path = item;
        loop {
            if let Some((_, w)) = self.why.iter().find(|(i, _)| i == path) {
                return Some(w);
            }
            path = path.rsplit_once("::")?.0;
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
            why: vec![("Mode::Windowed".into(), "main.rs now calls Mode::Windowed(800, 600).".into()), ("impl ApplicationHandler for Handler".into(), "applies the size".into())],
            triggered_by: vec!["src/main.rs: +    open(Mode::Windowed(800, 600))".into()],
            decisions: vec!["3 (proposed) softbuffer".into()],
            mode: "prototype".into(),
        };
        let text = m.render();
        assert!(text.starts_with("cycle ok: Added `Mode::Windowed`.\n"));
        assert_eq!(CycleMessage::parse(&text), m);
    }

    #[test]
    fn why_falls_back_to_parent() {
        let m = CycleMessage { why: vec![("Mode".into(), "because".into())], ..Default::default() };
        assert_eq!(m.why_for("Mode::Windowed"), Some("because"));
        assert_eq!(m.why_for("App::new"), None);
    }

    #[test]
    fn triggers() {
        let d = "diff --git a/src/main.rs b/src/main.rs\nindex 1..2\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1 +1 @@\n-    a()\n+    b()\n \n";
        assert_eq!(trigger_lines(d, 20), ["src/main.rs: -    a()", "src/main.rs: +    b()"]);
    }
}
