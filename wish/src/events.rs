//! Events emitted during a compile cycle.
//!
//! `wish compile --json` prints them as NDJSON, one per line. That stream is
//! the integration surface for editor plugins (Zed, nvim, ...): a plugin
//! spawns `wish compile --json` and renders what comes back.

use serde::Serialize;
use std::sync::mpsc::Sender;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Status { msg: String },
    Tool { name: String, detail: String },
    Says { text: String },
    Blocked { reason: String },
    Wiring { file: String, lines: Vec<String> },
    Changes { added: Vec<String>, changed: Vec<String>, removed: Vec<String>, untouched: usize },
    Decision { id: u32, kind: String, title: String, choice: String },
    Suggestion { id: u32, file: String, reason: String },
    Warning { msg: String },
    Violation { msg: String },
    Check { ok: bool, errors: String },
    Done { ok: bool, summary: String },
    App { line: String },
}

pub trait Sink: Send {
    fn emit(&mut self, e: Event);
}

pub struct Human;
impl Sink for Human {
    fn emit(&mut self, e: Event) {
        for l in render(&e) {
            println!("{l}");
        }
    }
}

pub struct Json;
impl Sink for Json {
    fn emit(&mut self, e: Event) {
        println!("{}", serde_json::to_string(&e).unwrap());
    }
}

pub struct Channel(pub Sender<Event>);
impl Sink for Channel {
    fn emit(&mut self, e: Event) {
        let _ = self.0.send(e);
    }
}

const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const CYAN: &str = "\x1b[36m";
const MAG: &str = "\x1b[35m";
const R: &str = "\x1b[0m";

/// Human-readable rendering, shared by `wish compile` and `wish dev`.
pub fn render(e: &Event) -> Vec<String> {
    match e {
        Event::Status { msg } => vec![format!("{BOLD}▸ {msg}{R}")],
        Event::Tool { name, detail } => vec![format!("{DIM}  · {name} {detail}{R}")],
        Event::Says { text } => text.lines().filter(|l| !l.trim().is_empty()).map(|l| format!("{DIM}  │ {l}{R}")).collect(),
        Event::Blocked { reason } => vec![format!("{YELLOW}  ⛨ blocked: {reason}{R}")],
        Event::Wiring { file, lines } => vec![format!("{CYAN}  ⇢ wired {file}: {}{R}", lines.join("  "))],
        Event::Changes { added, changed, removed, untouched } => {
            let mut v = vec![format!("{BOLD}▸ vocabulary changes{R}")];
            if added.is_empty() && changed.is_empty() && removed.is_empty() {
                v.push(format!("{DIM}  (none){R}"));
            }
            v.extend(added.iter().map(|i| format!("{GREEN}  + {i}{R}")));
            v.extend(changed.iter().map(|i| format!("{YELLOW}  ~ {i}{R}")));
            v.extend(removed.iter().map(|i| format!("{RED}  - {i}{R}")));
            if *untouched > 0 {
                v.push(format!("{DIM}  = {untouched} item(s) untouched{R}"));
            }
            v
        }
        Event::Decision { id, kind, title, choice } => vec![format!("{MAG}  ⚑ decision {id} ({kind}, proposed): {title}: {choice}{R}"), format!("{DIM}    accept/reject with `wish decide {id} accept|reject [reason]`{R}")],
        Event::Suggestion { id, file, reason } => vec![format!("{CYAN}  ✎ suggestion {id} for {file}: {reason}{R}"), format!("{DIM}    review with `wish suggestions`, then `wish accept {id}` / `wish reject {id}`{R}")],
        Event::Warning { msg } => vec![format!("{YELLOW}  ! {msg}{R}")],
        Event::Violation { msg } => vec![format!("{RED}  ✗ {msg}{R}")],
        Event::Check { ok: true, .. } => vec![format!("{GREEN}  ✓ cargo check passed{R}")],
        Event::Check { ok: false, errors } => {
            let mut v = vec![format!("{RED}  ✗ cargo check failed{R}")];
            v.extend(errors.lines().take(20).map(|l| format!("{DIM}    {l}{R}")));
            v
        }
        Event::Done { ok, summary } => {
            let mark = if *ok { format!("{GREEN}✓") } else { format!("{RED}✗") };
            let mut v = vec![format!("{mark} {}{R}", if *ok { "done" } else { "failed" })];
            v.extend(summary.lines().map(|l| format!("  {l}")));
            v
        }
        Event::App { line } => vec![format!("{DIM}[app]{R} {line}")],
    }
}
