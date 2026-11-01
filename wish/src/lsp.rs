//! `wish lsp`: wishlang as a language server, so any LSP editor (Zed first)
//! becomes the wishlang IDE. It runs next to rust-analyzer and provides:
//!
//! - blue (Information) diagnostics for code that **will be generated**
//!   (references the vocabulary doesn't define yet) and for **prototype**
//!   items (doc comment has a `# Mocked` section);
//! - semantic tokens marking every **generated** line (Zed paints them blue),
//!   and a hover that blames the line: who wrote it, when, and **why**;
//! - on save: a compile cycle with progress in the status bar, then the
//!   preview app restarts;
//! - code actions: adopt/unadopt, accept/reject suggestions, implement the
//!   prototype (file or project), undo, regenerate, jump between blocks.

use crate::commands;
use crate::engine::{self, Mode, Options};
use crate::events::{Event, Sink};
use crate::git::{Git, Who};
use crate::items;
use crate::pending;
use crate::preview::Preview;
use crate::project::{Project, Status};
use crate::provenance;
use anyhow::Result;
use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

type Callback = Box<dyn FnOnce(Value) + Send>;

struct State {
    root: Option<PathBuf>,
    /// Open buffers: relative path -> text.
    docs: BTreeMap<String, String>,
    busy: bool,
    /// A cycle requested while one was running; runs next.
    queued: Option<Options>,
    preview: Preview,
    /// Diagnostics from the last `cargo check`, by relative path.
    check: BTreeMap<String, Vec<Value>>,
    /// Files we published diagnostics for (to clear them later).
    published: std::collections::BTreeSet<String>,
    next_id: i32,
    pending: HashMap<RequestId, Callback>,
}

#[derive(Clone)]
struct Server {
    state: Arc<Mutex<State>>,
    out: crossbeam_sender::Sender,
}

/// Lets worker threads talk to the client.
mod crossbeam_sender {
    use lsp_server::Message;
    #[derive(Clone)]
    pub struct Sender(pub crossbeam_channel::Sender<Message>);
    impl Sender {
        pub fn send(&self, m: Message) {
            let _ = self.0.send(m);
        }
    }
}

pub fn run() -> Result<()> {
    let (conn, io) = Connection::stdio();
    let (id, params) = conn.initialize_start()?;
    let root = root_from_init(&params).and_then(|r| Project::discover_from(&r)).map(|p| p.root);
    let caps = json!({
        "capabilities": {
            "textDocumentSync": {"openClose": true, "change": 1, "save": {"includeText": false}},
            "codeActionProvider": true,
            "hoverProvider": true,
            "inlayHintProvider": true,
            "codeLensProvider": {"resolveProvider": false},
            "semanticTokensProvider": {"legend": {"tokenTypes": ["generated"], "tokenModifiers": ["prototype"]}, "full": true},
            "executeCommandProvider": {"commands": ["wish.adopt", "wish.unadopt", "wish.accept", "wish.reject", "wish.decide", "wish.undo", "wish.implement", "wish.regenerate", "wish.nextBlock"]}
        },
        "serverInfo": {"name": "wishlang", "version": env!("CARGO_PKG_VERSION")}
    });
    conn.initialize_finish(id, caps)?;
    let server = Server {
        state: Arc::new(Mutex::new(State { root, docs: BTreeMap::new(), busy: false, queued: None, preview: Preview::default(), check: BTreeMap::new(), published: Default::default(), next_id: 1, pending: HashMap::new() })),
        out: crossbeam_sender::Sender(conn.sender.clone()),
    };
    // Start the journal now, before any edits, so provenance is right from the first keystroke.
    if let Some(p) = server.project() {
        if let Err(e) = Git::open(&p.root).and_then(|g| engine::ensure_journal(&p, &g)) {
            server.show(1, &format!("wishlang: {e:#}"));
        }
    }
    // Make the journal branch exist for older journals, and list pending suggestions right away.
    if let Some(p) = server.project() {
        if let Ok(g) = Git::open(&p.root) {
            if let Some(j) = g.journal() {
                let _ = g.set_ref(&g.ref_name("journal"), &j);
            }
        }
        server.publish_all();
    }
    if server.root().is_none() {
        server.log("wishlang: no wish.toml in this workspace; staying idle (run `wish init` in the project)");
    }

    for msg in &conn.receiver {
        match msg {
            Message::Request(req) => {
                if conn.handle_shutdown(&req)? {
                    break;
                }
                server.request(req);
            }
            Message::Notification(n) => server.notification(n),
            Message::Response(r) => {
                let cb = server.state.lock().unwrap().pending.remove(&r.id);
                if let Some(cb) = cb {
                    cb(r.response_result.unwrap_or(Value::Null));
                }
            }
        }
    }
    server.state.lock().unwrap().preview.stop();
    // Worker threads may still hold sender clones, so don't wait for the IO threads.
    drop(io);
    Ok(())
}

fn root_from_init(params: &Value) -> Option<PathBuf> {
    let uri = params["workspaceFolders"][0]["uri"].as_str().or(params["rootUri"].as_str())?;
    uri_to_path(uri)
}

// ---- URIs ------------------------------------------------------------------

pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&rest[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    Some(PathBuf::from(String::from_utf8_lossy(&out).to_string()))
}

pub fn path_to_uri(p: &Path) -> String {
    let mut s = String::from("file://");
    for b in p.to_string_lossy().bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

fn range(l0: u32, c0: u32, l1: u32, c1: u32) -> Value {
    json!({"start": {"line": l0, "character": c0}, "end": {"line": l1, "character": c1}})
}

fn line_len(text: &str, line: usize) -> u32 {
    text.lines().nth(line).map(|l| l.chars().count() as u32).unwrap_or(0)
}

// ---- server ----------------------------------------------------------------

impl Server {
    fn root(&self) -> Option<PathBuf> {
        self.state.lock().unwrap().root.clone()
    }

    fn project(&self) -> Option<Project> {
        Project::load(&self.root()?).ok()
    }

    fn notify(&self, method: &str, params: Value) {
        self.out.send(Message::Notification(Notification::new(method.into(), params)));
    }

    fn log(&self, msg: &str) {
        self.notify("window/logMessage", json!({"type": 4, "message": msg}));
    }

    fn show(&self, kind: u8, msg: &str) {
        self.notify("window/showMessage", json!({"type": kind, "message": msg}));
    }

    /// Send a request to the client; `cb` gets the result.
    fn ask(&self, method: &str, params: Value, cb: Callback) {
        let id = {
            let mut st = self.state.lock().unwrap();
            let id = RequestId::from(st.next_id);
            st.next_id += 1;
            st.pending.insert(id.clone(), cb);
            id
        };
        self.out.send(Message::Request(Request::new(id, method.into(), params)));
    }

    fn reply(&self, id: RequestId, result: Value) {
        self.out.send(Message::Response(Response::new_ok(id, result)));
    }

    /// Find the wish project from a file, if this server doesn't have one yet
    /// (e.g. Zed opened the repo root, and wish.toml lives in a subfolder).
    fn discover_from_uri(&self, uri: &str) {
        if self.root().is_some() {
            return;
        }
        let Some(path) = uri_to_path(uri) else { return };
        let Some(p) = path.parent().and_then(Project::discover_from) else { return };
        self.state.lock().unwrap().root = Some(p.root.clone());
        self.log(&format!("wishlang: serving project {}", p.root.display()));
        if let Ok(g) = Git::open(&p.root) {
            let _ = engine::ensure_journal(&p, &g);
        }
        self.publish_all();
    }

    fn rel_of_uri(&self, uri: &str) -> Option<String> {
        let p = self.project()?;
        p.rel(&uri_to_path(uri)?)
    }

    fn uri_of_rel(&self, rel: &str) -> Option<String> {
        Some(path_to_uri(&self.root()?.join(rel)))
    }

    /// A file's text: the open buffer if there is one, else the disk.
    fn text(&self, rel: &str) -> Option<String> {
        if let Some(t) = self.state.lock().unwrap().docs.get(rel) {
            return Some(t.clone());
        }
        std::fs::read_to_string(self.root()?.join(rel)).ok()
    }

    // ---- notifications ----

    fn notification(&self, n: Notification) {
        let uri = n.params["textDocument"]["uri"].as_str().unwrap_or("").to_string();
        self.discover_from_uri(&uri);
        let Some(rel) = self.rel_of_uri(&uri) else { return };
        match n.method.as_str() {
            "textDocument/didOpen" => {
                let text = n.params["textDocument"]["text"].as_str().unwrap_or("").to_string();
                self.state.lock().unwrap().docs.insert(rel.clone(), text);
                self.publish(&rel);
            }
            "textDocument/didChange" => {
                if let Some(text) = n.params["contentChanges"].as_array().and_then(|c| c.last()).and_then(|c| c["text"].as_str()) {
                    self.state.lock().unwrap().docs.insert(rel.clone(), text.to_string());
                    self.publish(&rel);
                    // What you type is yours at once: re-color without waiting for a save.
                    for refresh in ["workspace/semanticTokens/refresh", "workspace/inlayHint/refresh"] {
                        self.ask(refresh, Value::Null, Box::new(|_| {}));
                    }
                }
            }
            "textDocument/didClose" => {
                self.state.lock().unwrap().docs.remove(&rel);
            }
            "textDocument/didSave" if rel.ends_with(".rs") => self.cycle(Options::default()),
            _ => {}
        }
    }

    // ---- diagnostics ----

    fn diagnostics(&self, rel: &str) -> Vec<Value> {
        let Some(p) = self.project() else { return vec![] };
        let Some(text) = self.text(rel) else { return vec![] };
        let mut out = Vec::new();
        if rel.ends_with(".rs") {
            // Everything the project defines, using open buffers where they exist.
            let mut all = Vec::new();
            for f in p.rust_files().iter().filter(|f| f.as_str() != rel) {
                if let Some(src) = self.text(f) {
                    all.extend(items::parse(&src).unwrap_or_default());
                }
            }
            let known = pending::Known::from_items(&all);
            for pnd in pending::find(&text, &known).unwrap_or_default() {
                out.push(json!({
                    "range": range(pnd.start.0, pnd.start.1, pnd.end.0, pnd.end.1),
                    "severity": 3, "source": "wishlang",
                    "message": format!("✦ will be generated: {}", pnd.name),
                }));
            }
            for s in p.suggestions().into_iter().filter(|s| s.status == Status::Proposed && s.file == rel) {
                if let Some(at) = text.find(&s.replace) {
                    let l0 = text[..at].matches('\n').count() as u32;
                    let l1 = l0 + s.replace.matches('\n').count() as u32;
                    // Warning, so every suggestion is listed in Zed's project diagnostics.
                    out.push(json!({"range": range(l0, 0, l1, line_len(&text, l1 as usize)), "severity": 2, "source": "wishlang",
                        "code": format!("suggestion-{}", s.id),
                        "message": format!("✎ suggestion {}: {}\nproposed:\n{}", s.id, s.reason, s.with)}));
                }
            }
        }
        out.extend(self.state.lock().unwrap().check.get(rel).cloned().unwrap_or_default());
        out
    }

    fn publish(&self, rel: &str) {
        let Some(uri) = self.uri_of_rel(rel) else { return };
        let diags = self.diagnostics(rel);
        self.state.lock().unwrap().published.insert(rel.to_string());
        self.notify("textDocument/publishDiagnostics", json!({"uri": uri, "diagnostics": diags}));
    }

    /// Republish everything open or previously published, and refresh hints.
    fn publish_all(&self) {
        let rels: std::collections::BTreeSet<String> = {
            let st = self.state.lock().unwrap();
            st.docs.keys().chain(st.published.iter()).chain(st.check.keys()).cloned().collect::<std::collections::BTreeSet<_>>()
        };
        // Files with pending suggestions too, so they all appear in Zed's project diagnostics.
        let mut rels = rels;
        if let Some(p) = self.project() {
            rels.extend(p.suggestions().into_iter().filter(|s| s.status == Status::Proposed).map(|s| s.file));
        }
        for r in rels {
            self.publish(&r);
        }
        for refresh in ["workspace/semanticTokens/refresh", "workspace/inlayHint/refresh", "workspace/codeLens/refresh"] {
            self.ask(refresh, Value::Null, Box::new(|_| {}));
        }
    }

    // ---- requests ----

    fn request(&self, req: Request) {
        if let Some(uri) = req.params["textDocument"]["uri"].as_str() {
            self.discover_from_uri(uri);
        }
        let result = match req.method.as_str() {
            "textDocument/semanticTokens/full" => self.semantic_tokens(&req.params),
            "textDocument/hover" => self.hover(&req.params),
            "textDocument/inlayHint" => self.inlay_hints(&req.params),
            "textDocument/codeLens" => self.code_lenses(&req.params),
            "textDocument/codeAction" => self.code_actions(&req.params),
            "workspace/executeCommand" => {
                self.execute(&req.params);
                Value::Null
            }
            _ => Value::Null,
        };
        self.reply(req.id, result);
    }

    /// Every generated line as a `generated` semantic token; Zed's token rule
    /// paints it blue. Lines inside a still-mocked item also get `prototype`.
    fn semantic_tokens(&self, params: &Value) -> Value {
        let empty = json!({"data": []});
        let Some(rel) = params["textDocument"]["uri"].as_str().and_then(|u| self.rel_of_uri(u)) else { return empty };
        let (Some(text), Some(root)) = (self.text(&rel), self.root()) else { return empty };
        let Ok(git) = Git::open(&root) else { return empty };
        let Some(whos) = provenance::of(&git, &rel, &text) else { return empty };
        let mocked: Vec<(usize, usize)> = items::parse(&text).unwrap_or_default().iter().filter(|i| i.mocked().is_some()).map(|i| (i.line, i.end_line)).collect();
        json!({"data": encode_tokens(&whos, &text, &mocked)})
    }

    /// Blame the hovered line and show the reasoning recorded in its journal commit.
    fn hover(&self, params: &Value) -> Value {
        let Some(rel) = params["textDocument"]["uri"].as_str().and_then(|u| self.rel_of_uri(u)) else { return Value::Null };
        let line = params["position"]["line"].as_u64().unwrap_or(0) as usize;
        let (Some(text), Some(root)) = (self.text(&rel), self.root()) else { return Value::Null };
        let Ok(git) = Git::open(&root) else { return Value::Null };
        let Some(lines) = git.blame_lines(&rel, &text) else { return Value::Null };
        let Some((who, sha)) = lines.get(line).cloned() else { return Value::Null };
        if text.lines().nth(line).is_none_or(|l| l.trim().is_empty()) {
            return Value::Null;
        }
        let md = hover_markdown(&git, &text, line, who, sha.as_deref());
        json!({"contents": {"kind": "markdown", "value": md}, "range": range(line as u32, 0, line as u32, line_len(&text, line))})
    }

    /// Always-visible annotations: `✎` on your lines inside generated files,
    /// and a `◇ prototype` tag after the signature of every still-mocked item.
    fn inlay_hints(&self, params: &Value) -> Value {
        let Some(rel) = params["textDocument"]["uri"].as_str().and_then(|u| self.rel_of_uri(u)) else { return json!([]) };
        let (Some(text), Some(root)) = (self.text(&rel), self.root()) else { return json!([]) };
        let mut hints = Vec::new();
        if let Some(whos) = Git::open(&root).ok().and_then(|g| provenance::of(&g, &rel, &text)) {
            // One marker per block of your lines, only where they mix with generated code.
            if whos.contains(&Who::Claude) {
                let lines: Vec<&str> = text.lines().collect();
                for b in provenance::blocks(&whos, &text).into_iter().filter(|b| b.who == Who::Human) {
                    let Some(first) = (b.start..=b.end).find(|i| lines.get(*i).is_some_and(|l| !l.trim().is_empty())) else { continue };
                    let n = (b.start..=b.end).filter(|i| lines.get(*i).is_some_and(|l| !l.trim().is_empty())).count();
                    let label = if n == 1 { "✎ yours".to_string() } else { format!("✎ yours · {n} lines") };
                    hints.push(json!({"position": {"line": first, "character": line_len(&text, first)}, "label": label, "paddingLeft": true, "tooltip": "Written by you. Protected: Claude keeps these lines and makes them true."}));
                }
            }
        }
        for it in items::parse(&text).unwrap_or_default() {
            if let Some(m) = it.mocked() {
                let l = it.line.max(1) - 1;
                let what: String = m.join("; ").chars().take(80).collect();
                let label = if what.is_empty() { "◇ prototype".to_string() } else { format!("◇ prototype · mocked: {what}") };
                hints.push(json!({"position": {"line": l, "character": line_len(&text, l)}, "label": label, "paddingLeft": true, "tooltip": "Still mocked. Use the ▶ Implement lens above to make it real."}));
            }
        }
        json!(hints)
    }

    /// Clickable actions above code: implement a prototype item, and accept or
    /// reject Claude's suggestions in place.
    fn code_lenses(&self, params: &Value) -> Value {
        let Some(rel) = params["textDocument"]["uri"].as_str().and_then(|u| self.rel_of_uri(u)) else { return json!([]) };
        let (Some(text), Some(p)) = (self.text(&rel), self.project()) else { return json!([]) };
        let lines: Vec<&str> = text.lines().collect();
        let lens = |line: usize, title: String, command: &str, args: Value| json!({"range": range(line as u32, 0, line as u32, 0), "command": {"title": title, "command": command, "arguments": args}});
        let mut out = Vec::new();
        let mocked: Vec<items::Item> = items::parse(&text).unwrap_or_default().into_iter().filter(|i| i.mocked().is_some()).collect();
        if mocked.len() > 1 {
            out.push(lens(0, format!("▶ Implement all {} prototype items in this file", mocked.len()), "wish.implement", json!([rel, null])));
        }
        for it in &mocked {
            // Put the lens above the item's doc comment and attributes.
            let mut start = it.line.max(1) - 1;
            while start > 0 && lines.get(start - 1).is_some_and(|l| { let t = l.trim_start(); t.starts_with("///") || t.starts_with("#[") }) {
                start -= 1;
            }
            out.push(lens(start, format!("▶ Implement `{}`", it.path), "wish.implement", json!([rel, it.path])));
        }
        for s in p.suggestions().into_iter().filter(|s| s.status == Status::Proposed && s.file == rel) {
            if let Some(at) = text.find(&s.replace) {
                let l = text[..at].matches('\n').count();
                let reason: String = s.reason.chars().take(90).collect();
                out.push(lens(l, format!("✓ Accept suggestion {}: {reason}", s.id), "wish.accept", json!([s.id])));
                out.push(lens(l, "✗ Reject".into(), "wish.reject", json!([s.id])));
            }
        }
        json!(out)
    }

    fn blocks(&self, rel: &str) -> Option<Vec<provenance::Block>> {
        let text = self.text(rel)?;
        let git = Git::open(&self.root()?).ok()?;
        let whos = provenance::of(&git, rel, &text)?;
        Some(provenance::blocks(&whos, &text))
    }

    fn code_actions(&self, params: &Value) -> Value {
        let uri = params["textDocument"]["uri"].as_str().unwrap_or("");
        let Some(rel) = self.rel_of_uri(uri) else { return json!([]) };
        let Some(p) = self.project() else { return json!([]) };
        let line = params["range"]["start"]["line"].as_u64().unwrap_or(0) as usize;
        let cmd = |title: String, command: &str, args: Value| json!({"title": title, "kind": "quickfix", "command": {"title": title, "command": command, "arguments": args}});
        let mut actions = Vec::new();

        // Suggestions under the cursor, found by position (not only via the
        // diagnostic Zed passes along, which the project diagnostics view may omit).
        let mut ids: Vec<u32> = params["context"]["diagnostics"].as_array().into_iter().flatten().filter_map(|d| d["code"].as_str()?.strip_prefix("suggestion-")?.parse().ok()).collect();
        let end_line = params["range"]["end"]["line"].as_u64().unwrap_or(line as u64) as usize;
        if let Some(t) = self.text(&rel) {
            for s in p.suggestions().into_iter().filter(|s| s.status == Status::Proposed && s.file == rel) {
                if let Some(at) = t.find(&s.replace) {
                    let l0 = t[..at].matches('\n').count();
                    let l1 = l0 + s.replace.matches('\n').count();
                    if line <= l1 && end_line >= l0 && !ids.contains(&s.id) {
                        ids.push(s.id);
                    }
                }
            }
        }
        let suggestions = p.suggestions();
        for id in ids {
            let reason: String = suggestions.iter().find(|s| s.id == id).map(|s| s.reason.chars().take(70).collect()).unwrap_or_default();
            let mut accept = cmd(format!("✓ Accept suggestion {id}: {reason}"), "wish.accept", json!([id]));
            accept["isPreferred"] = json!(true);
            actions.push(accept);
            actions.push(cmd(format!("✗ Reject suggestion {id}"), "wish.reject", json!([id])));
        }
        let text = self.text(&rel).unwrap_or_default();
        let own = items::parse(&text).unwrap_or_default();
        if !p.is_protected(&rel) {
            // The item whose name is on, or nearest above, the cursor line.
            if let Some(it) = own.iter().filter(|i| i.line > 0 && i.line - 1 <= line).max_by_key(|i| i.line) {
                if p.cfg.adopted.contains_key(&it.path) {
                    actions.push(cmd(format!("wishlang: give `{}` back to Claude (unadopt)", it.path), "wish.unadopt", json!([it.path])));
                } else {
                    actions.push(cmd(format!("wishlang: adopt `{}` (protect it from Claude)", it.path), "wish.adopt", json!([it.path])));
                }
            }
            if own.iter().any(|i| i.mocked().is_some()) {
                actions.push(cmd("wishlang: implement prototype in this file".into(), "wish.implement", json!([rel, null])));
            }
        }
        actions.push(cmd("wishlang: next generated block ↓".into(), "wish.nextBlock", json!([uri, line, "claude"])));
        actions.push(cmd("wishlang: next block written by you ↓".into(), "wish.nextBlock", json!([uri, line, "human"])));
        actions.push(cmd("wishlang: implement the whole prototype".into(), "wish.implement", json!([])));
        actions.push(cmd("wishlang: regenerate now".into(), "wish.regenerate", json!([])));
        actions.push(cmd("wishlang: undo last cycle".into(), "wish.undo", json!([])));
        json!(actions)
    }

    fn execute(&self, params: &Value) {
        let Some(mut p) = self.project() else { return };
        let args = params["arguments"].as_array().cloned().unwrap_or_default();
        let s = |i: usize| args.get(i).and_then(|v| v.as_str()).unwrap_or("").to_string();
        let n = |i: usize| args.get(i).and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let result = match params["command"].as_str().unwrap_or("") {
            "wish.adopt" => commands::adopt(&mut p, &s(0)),
            "wish.unadopt" => commands::unadopt(&mut p, &s(0)),
            "wish.accept" => match commands::accept(&mut p, n(0)) {
                // Accepting changes your code, so regenerate right away.
                Ok(msg) => {
                    self.show(3, &msg);
                    self.publish_all();
                    self.cycle(Options::default());
                    return;
                }
                e => e,
            },
            "wish.reject" => commands::reject(&p, n(0)),
            "wish.decide" => commands::decide(&p, n(0), s(1) == "accept", args.get(2).and_then(|v| v.as_str()).map(String::from)),
            "wish.undo" => match commands::undo(&p) {
                Ok(msg) => {
                    self.rebuild_preview();
                    Ok(msg)
                }
                e => e,
            },
            "wish.implement" => {
                let file = args.first().and_then(|v| v.as_str()).map(String::from);
                let item = args.get(1).and_then(|v| v.as_str()).map(String::from);
                self.cycle(commands::implement_options(file, item));
                return;
            }
            "wish.regenerate" => {
                self.cycle(Options { force: true, ..Options::default() });
                return;
            }
            "wish.nextBlock" => {
                self.next_block(&s(0), n(1) as usize, s(2) == "human");
                return;
            }
            _ => return,
        };
        match result {
            Ok(msg) => self.show(3, &msg),
            Err(e) => self.show(1, &format!("{e:#}")),
        }
        self.publish_all();
    }

    fn next_block(&self, uri: &str, line: usize, human: bool) {
        let Some(rel) = self.rel_of_uri(uri) else { return };
        let Some(blocks) = self.blocks(&rel) else { return };
        let want = if human { Who::Human } else { Who::Claude };
        let target = blocks.iter().find(|b| b.who == want && b.start > line).or_else(|| blocks.iter().find(|b| b.who == want));
        match target {
            Some(b) => self.ask("window/showDocument", json!({"uri": uri, "takeFocus": true, "selection": range(b.start as u32, 0, b.start as u32, 0)}), Box::new(|_| {})),
            None => self.show(3, if human { "no lines written by you in this file" } else { "no generated lines in this file" }),
        }
    }

    // ---- cycles ----

    fn cycle(&self, opts: Options) {
        {
            let mut st = self.state.lock().unwrap();
            if st.root.is_none() {
                return;
            }
            if st.busy {
                // Keep the most specific request: an implement or a forced
                // regenerate beats a plain save.
                let keep_old = st.queued.as_ref().is_some_and(|q| q.force && !opts.force);
                if !keep_old {
                    st.queued = Some(opts);
                }
                return;
            }
            st.busy = true;
            st.check.clear();
        }
        let me = self.clone();
        std::thread::spawn(move || {
            let token = format!("wish-{}", std::process::id());
            let (tx, rx) = std::sync::mpsc::channel();
            me.ask("window/workDoneProgress/create", json!({"token": token}), Box::new(move |_| {
                let _ = tx.send(());
            }));
            let _ = rx.recv_timeout(std::time::Duration::from_secs(2));
            let title = match &opts.mode {
                Mode::Implement { .. } => "wishlang: implementing",
                Mode::Prototype => "wishlang",
            };
            me.notify("$/progress", json!({"token": token, "value": {"kind": "begin", "title": title, "message": "checking", "cancellable": false}}));
            let mut sink = LspSink { server: me.clone(), token: token.clone(), decisions: Vec::new(), check: None };
            let ok = match me.project() {
                Some(mut p) => engine::compile(&mut p, &mut sink, &opts).unwrap_or_else(|e| {
                    me.show(1, &format!("wishlang: {e:#}"));
                    false
                }),
                None => false,
            };
            if let Some(errors) = sink.check.take() {
                me.state.lock().unwrap().check = parse_cargo(&errors);
            }
            if ok {
                me.notify("$/progress", json!({"token": token, "value": {"kind": "report", "message": "building preview"}}));
                me.build_and_restart();
            }
            me.notify("$/progress", json!({"token": token, "value": {"kind": "end", "message": if ok { "done" } else { "failed" }}}));
            for (id, title, choice) in std::mem::take(&mut sink.decisions) {
                let m2 = me.clone();
                me.ask(
                    "window/showMessageRequest",
                    json!({"type": 3, "message": format!("wishlang decision {id}: {title}: {choice}"), "actions": [{"title": "Accept"}, {"title": "Reject"}, {"title": "Later"}]}),
                    Box::new(move |v| {
                        let Some(p) = m2.project() else { return };
                        let r = match v["title"].as_str() {
                            Some("Accept") => commands::decide(&p, id, true, None),
                            Some("Reject") => commands::decide(&p, id, false, Some("rejected in the editor".into())),
                            _ => return,
                        };
                        if let Ok(msg) = r {
                            m2.show(3, &msg);
                        }
                    }),
                );
            }
            me.publish_all();
            let again = {
                let mut st = me.state.lock().unwrap();
                st.busy = false;
                st.queued.take()
            };
            if let Some(next) = again {
                me.cycle(next);
            }
        });
    }

    fn build_and_restart(&self) {
        let Some(p) = self.project() else { return };
        let (ok, errors) = engine::build(&p);
        if !ok {
            self.show(1, &format!("wishlang: build failed\n{errors}"));
            return;
        }
        let me = self.clone();
        let r = self.state.lock().unwrap().preview.restart(&p, move |line| me.log(&format!("[app] {line}")));
        if let Err(e) = r {
            self.show(1, &format!("wishlang: could not start the app: {e}"));
        }
    }

    fn rebuild_preview(&self) {
        let me = self.clone();
        std::thread::spawn(move || me.build_and_restart());
    }
}

/// LSP semantic token data: one `generated` token per generated, non-blank
/// line (whole line), with the `prototype` modifier inside mocked items
/// (`mocked` holds 1-based inclusive line ranges).
pub fn encode_tokens(whos: &[Who], text: &str, mocked: &[(usize, usize)]) -> Vec<u32> {
    let mut data = Vec::new();
    let mut prev = 0usize;
    for (i, (l, who)) in text.lines().zip(whos).enumerate() {
        if *who != Who::Claude || l.trim().is_empty() {
            continue;
        }
        let proto = mocked.iter().any(|(a, b)| (*a..=*b).contains(&(i + 1)));
        data.extend([(i - prev) as u32, 0, l.encode_utf16().count() as u32, 0, proto as u32]);
        prev = i;
    }
    data
}

/// Hover text for one line: who wrote it, when, and why (from the journal commit).
fn hover_markdown(git: &Git, text: &str, line: usize, who: Who, sha: Option<&str>) -> String {
    use crate::journal::CycleMessage;
    let Some(sha) = sha else { return "✎ **yours**, not saved yet".into() };
    let Some((msg, age)) = git.message(sha) else { return String::new() };
    let short = &sha[..sha.len().min(8)];
    let m = CycleMessage::parse(&msg);
    if who == Who::Human {
        let mut s = format!("✎ **written by you** · saved {age}");
        if !m.summary.is_empty() {
            s.push_str(&format!("\n\n{}", m.summary));
        }
        return s;
    }
    let mut s = format!("⚙ **generated** · {} · {age}{}", m.label, if m.mode.is_empty() { String::new() } else { format!(" · {}", m.mode) });
    // The innermost item enclosing this line.
    let item = items::parse(text).unwrap_or_default().into_iter().filter(|i| i.line <= line + 1 && line + 1 <= i.end_line).min_by_key(|i| i.end_line - i.line);
    let why = item.as_ref().and_then(|i| m.why_for(&i.path).map(|w| (i.path.clone(), w.to_string()))).or_else(|| {
        // Methods inside trait impls aren't separate items: match the nearest
        // enclosing `fn name` against reasons given for `Type::name`.
        let re = regex::Regex::new(r"\bfn\s+(\w+)").unwrap();
        let lines: Vec<&str> = text.lines().collect();
        let start = item.as_ref().map(|i| i.line.saturating_sub(1)).unwrap_or(0);
        let name = (start..=line.min(lines.len().saturating_sub(1))).rev().find_map(|l| re.captures(lines[l]).map(|c| c[1].to_string()))?;
        m.why.iter().find(|(i, _)| i.rsplit("::").next() == Some(name.as_str())).map(|(i, w)| (i.clone(), w.clone()))
    });
    match why {
        Some((path, w)) => s.push_str(&format!("\n\n**`{path}`**: {w}")),
        None => {
            if !m.summary.is_empty() {
                s.push_str(&format!("\n\n{}", m.summary));
            }
        }
    }
    if !m.triggered_by.is_empty() {
        s.push_str(&format!("\n\n**Triggered by your change**\n```diff\n{}\n```", m.triggered_by.join("\n")));
    }
    if !m.decisions.is_empty() {
        s.push_str(&format!("\n\n**Decisions**: {}", m.decisions.join("; ")));
    }
    s.push_str(&format!("\n\n`git show {short}` for the full cycle"));
    s
}

/// Turn `cargo check --message-format short` output into diagnostics.
fn parse_cargo(text: &str) -> BTreeMap<String, Vec<Value>> {
    let re = regex::Regex::new(r"^([^:\s]+\.rs):(\d+):(\d+): (error|warning)(\[\w+\])?: (.*)$").unwrap();
    let mut m: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for l in text.lines() {
        if let Some(c) = re.captures(l) {
            let line = c[2].parse::<u32>().unwrap_or(1).saturating_sub(1);
            let col = c[3].parse::<u32>().unwrap_or(1).saturating_sub(1);
            let sev = if &c[4] == "error" { 1 } else { 2 };
            m.entry(c[1].to_string()).or_default().push(json!({"range": range(line, col, line, col + 1), "severity": sev, "source": "wishlang (cargo)", "message": c[6].to_string()}));
        }
    }
    m
}

struct LspSink {
    server: Server,
    token: String,
    decisions: Vec<(u32, String, String)>,
    check: Option<String>,
}

impl Sink for LspSink {
    fn emit(&mut self, e: Event) {
        let plain: Vec<String> = crate::events::render(&e).iter().map(|l| strip_ansi(l)).collect();
        for l in &plain {
            self.server.log(l);
        }
        let report = match &e {
            Event::Status { msg } => Some(msg.clone()),
            Event::Tool { name, detail } => Some(format!("Claude · {name} {detail}")),
            Event::Blocked { reason } => Some(format!("blocked: {reason}")),
            Event::Changes { added, changed, removed, untouched } => Some(format!("+{} ~{} -{} ={untouched}", added.len(), changed.len(), removed.len())),
            _ => None,
        };
        if let Some(r) = report {
            let r: String = r.chars().take(120).collect();
            self.server.notify("$/progress", json!({"token": self.token, "value": {"kind": "report", "message": r}}));
        }
        match e {
            Event::Decision { id, title, choice, .. } => self.decisions.push((id, title, choice)),
            Event::Check { errors, .. } => self.check = Some(errors),
            Event::Violation { msg } => self.server.show(1, &format!("wishlang: {msg}")),
            Event::Done { ok, summary } => self.server.show(if ok { 3 } else { 1 }, &format!("wishlang: {}", summary.lines().next().unwrap_or(""))),
            _ => {}
        }
    }
}

fn strip_ansi(s: &str) -> String {
    regex::Regex::new(r"\x1b\[[0-9;]*m").unwrap().replace_all(s, "").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uris_roundtrip() {
        let p = Path::new("/tmp/my dir/src/main.rs");
        let u = path_to_uri(p);
        assert_eq!(u, "file:///tmp/my%20dir/src/main.rs");
        assert_eq!(uri_to_path(&u).unwrap(), p);
    }

    #[test]
    fn tokens() {
        use Who::*;
        let text = "a\n\nbb\nmine\nccc\n";
        let whos = [Claude, Claude, Claude, Human, Claude];
        // line 0 len 1; blank line 1 skipped; line 2 len 2 (+2); line 3 yours; line 4 len 3 (+2, prototype)
        assert_eq!(encode_tokens(&whos, text, &[(5, 5)]), [0, 0, 1, 0, 0, 2, 0, 2, 0, 0, 2, 0, 3, 0, 1]);
    }

    #[test]
    fn cargo_diagnostics() {
        let m = parse_cargo("src/main.rs:7:42: error[E0599]: no variant named `Windowed`\nwarning: unused\nsrc/window.rs:3:1: warning: dead code");
        assert_eq!(m["src/main.rs"][0]["severity"], 1);
        assert_eq!(m["src/main.rs"][0]["range"]["start"]["line"], 6);
        assert_eq!(m["src/window.rs"][0]["severity"], 2);
    }
}
