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
//!   prototype (file or project), undo, regenerate, jump between blocks;
//! - `wish/*` requests for editors that render the same data natively
//!   (`wish/fileState`, `wish/show`) and a `wish/changed` notification.
//!
//! Every per-file answer is a projection of one [`FileState`].

use crate::commands;
use crate::engine::{self, Mode, Options};
use crate::events::{Event, Sink};
use crate::git::{FileStat, Git, Who};
use crate::items::{self, Item};
use crate::journal::CycleMessage;
use crate::pending::{self, Pending};
use crate::preview::Preview;
use crate::project::{Lang, Project, Status, locate};
use crate::provenance;
use crate::ticket::{self, Problem, Trace};
use anyhow::{Context, Result, bail};
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, RequestId, Response};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

type Callback = Box<dyn FnOnce(Value) + Send>;

/// One editor connection.
struct State {
    root: Option<PathBuf>,
    /// The project's shared state; a private one until the root is known.
    hub: Arc<Hub>,
    /// The client's environment (PATH, ...), applied to the project's tools.
    env: Vec<(String, String)>,
    /// Open buffers: relative path -> text.
    docs: BTreeMap<String, String>,
    /// Files we published diagnostics for (to clear them later).
    published: std::collections::BTreeSet<String>,
    next_id: i32,
    pending: HashMap<RequestId, Callback>,
}

/// A project as every editor connected to it sees it. In the daemon several clients share one;
/// it outlives them, so an editor that reconnects finds the index warm.
struct Hub {
    shared: Mutex<Shared>,
    clients: Mutex<Vec<Server>>,
}

#[derive(Default)]
struct Shared {
    busy: bool,
    /// A cycle requested while one was running; runs next.
    queued: Option<Options>,
    /// "branch@HEAD" last checked for journals to merge.
    checked_head: Option<String>,
    preview: Preview,
    /// Diagnostics from the last check, by relative path.
    check: BTreeMap<String, Vec<Value>>,
    /// Items of each file's last successful parse, so markers survive a buffer with a syntax error.
    last_items: BTreeMap<String, Vec<Item>>,
    /// Parsed items and implicit members of files on disk, keyed by path and checked against
    /// size and mtime, so "will be generated" doesn't reparse the whole project per keystroke.
    index: HashMap<String, Indexed>,
    /// The last file state and its key (file, text, journal, HEAD): an editor asks for several
    /// projections of the same state per keystroke. Cleared whenever wish changes anything.
    last_state: Option<(String, Arc<FileState>)>,
    /// The ticket trace of the branch's journal tip, keyed by "branch@journal".
    trace: Option<(String, Option<Arc<Trace>>)>,
    /// Environment of the latest client that connected.
    env: Vec<(String, String)>,
    /// When the last client left (None while one is connected).
    idle_since: Option<std::time::Instant>,
}

impl Hub {
    fn new() -> Arc<Hub> {
        Arc::new(Hub { shared: Mutex::new(Shared::default()), clients: Mutex::new(Vec::new()) })
    }
}

/// Projects by root, shared by every client in this process.
static HUBS: Mutex<BTreeMap<PathBuf, Arc<Hub>>> = Mutex::new(BTreeMap::new());

/// A file on disk as last parsed for the project index.
struct Indexed {
    stamp: (u64, Option<std::time::SystemTime>),
    items: Arc<Vec<Item>>,
    implicit: Arc<Vec<(String, Option<String>)>>,
}

#[derive(Clone)]
struct Server {
    id: u64,
    state: Arc<Mutex<State>>,
    out: crossbeam_sender::Sender,
}

/// A code lens in the editor-neutral shape of `wish/fileState`.
#[derive(Serialize, Clone)]
struct Lens {
    line: usize,
    title: String,
    command: String,
    arguments: Value,
}

/// A run of generated lines: an item with its doc comment and body (0-based, inclusive).
struct FileBlock {
    start: usize,
    end: usize,
    /// The item's name line (or the first line of a block without one): hover is anchored here.
    anchor: usize,
    item: Option<String>,
    problems: Vec<String>,
    untraced: bool,
}

/// Everything known about one file: the single place that blames and parses it.
/// Every per-file answer (tokens, hover, hints, lenses, diagnostics,
/// `wish/fileState`) is a projection of this.
struct FileState {
    git: Option<Git>,
    ticket: Option<Arc<Trace>>,
    text: String,
    items: Vec<Item>,
    whos: Vec<Who>,
    /// The journal commit behind each line (None for lines not saved yet).
    shas: Vec<Option<String>>,
    /// Generated and prototype lines (0-based); a line is never in both.
    generated: Vec<usize>,
    prototype: Vec<usize>,
    pending: Vec<Pending>,
    /// Whether `pending` was computed (it's the expensive part).
    with_pending: bool,
    lenses: Vec<Lens>,
    blocks: Vec<FileBlock>,
}

#[derive(Serialize)]
struct WirePending {
    range: Value,
    name: String,
}

#[derive(Serialize)]
struct WireBlock {
    start: usize,
    end: usize,
    item: Option<String>,
    hover: String,
    problems: Vec<String>,
    untraced: bool,
}

/// `wish/fileState`: ranges are 0-based LSP ranges (UTF-16 columns), blocks and lenses use 0-based lines.
#[derive(Serialize)]
struct WireFileState {
    generated: Vec<Value>,
    prototype: Vec<Value>,
    pending: Vec<WirePending>,
    lenses: Vec<Lens>,
    blocks: Vec<WireBlock>,
}

#[derive(Serialize)]
struct WireWhy {
    item: String,
    why: String,
    problems: Vec<String>,
    file: Option<String>,
    line: Option<usize>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WireEntry {
    sha: String,
    author: &'static str,
    when: String,
    label: String,
    summary: String,
    ticket: Option<String>,
    why: Vec<WireWhy>,
    triggered_by: Vec<String>,
    decisions: Vec<String>,
    mode: String,
    files: Vec<FileStat>,
}

#[derive(Serialize)]
struct WireDecision {
    id: u32,
    title: String,
    choice: String,
    why: String,
    status: Status,
}

#[derive(Serialize)]
struct WireProblem {
    id: String,
    text: String,
}

/// `wish/log`: the journal (newest first), every decision with its live status, and the ticket's problems.
#[derive(Serialize)]
struct WireLog {
    entries: Vec<WireEntry>,
    decisions: Vec<WireDecision>,
    problems: Vec<WireProblem>,
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
    let result = run_with(conn, Vec::new());
    // Worker threads may still hold sender clones, so don't wait for the IO threads.
    drop(io);
    result
}

/// Serve one client over `conn` until it shuts down. `env` is the client's environment
/// (empty in-process: the tools then run in this process's own).
pub fn run_with(conn: Connection, env: Vec<(String, String)>) -> Result<()> {
    let (id, params) = conn.initialize_start()?;
    let root = root_from_init(&params).and_then(|r| Project::discover_from(&r)).map(|p| p.root);
    let caps = json!({
        "capabilities": {
            "textDocumentSync": {"openClose": true, "change": 1, "save": {"includeText": false}},
            "codeActionProvider": true,
            "hoverProvider": true,
            "inlayHintProvider": true,
            "codeLensProvider": {"resolveProvider": false},
            "semanticTokensProvider": {"legend": {"tokenTypes": ["generated"], "tokenModifiers": ["prototype", "untraced"]}, "full": true},
            "executeCommandProvider": {"commands": ["wish.adopt", "wish.unadopt", "wish.accept", "wish.reject", "wish.decide", "wish.undo", "wish.implement", "wish.cycle", "wish.regenerate", "wish.nextBlock", "wish.ticket.edit", "wish.ticket.open"]}
        },
        "serverInfo": {"name": "wishlang", "version": env!("CARGO_PKG_VERSION")}
    });
    conn.initialize_finish(id, caps)?;
    static NEXT_CLIENT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let server = Server {
        id: NEXT_CLIENT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        state: Arc::new(Mutex::new(State { root: None, hub: Hub::new(), env, docs: BTreeMap::new(), published: Default::default(), next_id: 1, pending: HashMap::new() })),
        out: crossbeam_sender::Sender(conn.sender.clone()),
    };
    if let Some(r) = root {
        server.attach(r);
    }
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
    server.detach();
    Ok(())
}

/// The project at `root` changed outside this process (a hook or a CLI command wrote the
/// journal): drop its caches and let every editor of it republish.
pub fn project_changed(root: &Path) {
    let Some(hub) = HUBS.lock().unwrap().get(root).cloned() else { return };
    {
        let mut sh = hub.shared.lock().unwrap();
        sh.last_state = None;
        sh.trace = None;
        sh.checked_head = None;
    }
    let clients = hub.clients.lock().unwrap().clone();
    std::thread::spawn(move || clients.iter().for_each(Server::publish_all));
}

/// One line per served project: root, connected clients, whether a cycle runs.
pub fn projects() -> Vec<(PathBuf, usize, bool)> {
    HUBS.lock().unwrap().iter().map(|(r, h)| (r.clone(), h.clients.lock().unwrap().len(), h.shared.lock().unwrap().busy)).collect()
}

/// Forget projects nobody has used for `after` (their caches and preview go with them).
pub fn evict_idle(after: std::time::Duration) {
    HUBS.lock().unwrap().retain(|_, h| {
        let sh = h.shared.lock().unwrap();
        sh.busy || !sh.idle_since.is_some_and(|t| t.elapsed() >= after)
    });
}

fn root_from_init(params: &Value) -> Option<PathBuf> {
    let uri = params["workspaceFolders"][0]["uri"].as_str().or(params["rootUri"].as_str())?;
    uri_to_path(uri)
}

/// Adds `root`, the project's URI: the files in a response are relative to it, which need not be the
/// editor's workspace (a project in `backend/` of a repository opened at its root).
fn with_root(mut v: Value, p: &Project) -> Value {
    v["root"] = json!(path_to_uri(&p.root));
    v
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
    text.lines().nth(line).map(|l| l.encode_utf16().count() as u32).unwrap_or(0)
}

// ---- server ----------------------------------------------------------------

impl Server {
    fn root(&self) -> Option<PathBuf> {
        self.state.lock().unwrap().root.clone()
    }

    fn hub(&self) -> Arc<Hub> {
        self.state.lock().unwrap().hub.clone()
    }

    /// Serve `root`: join its hub (created on first use) and bring this client's environment.
    fn attach(&self, root: PathBuf) {
        let hub = HUBS.lock().unwrap().entry(root.clone()).or_insert_with(Hub::new).clone();
        hub.clients.lock().unwrap().push(self.clone());
        let env = {
            let mut st = self.state.lock().unwrap();
            st.root = Some(root);
            st.hub = hub.clone();
            st.env.clone()
        };
        let mut sh = hub.shared.lock().unwrap();
        sh.idle_since = None;
        if !env.is_empty() {
            sh.env = env;
        }
    }

    /// Leave the hub; the last client to leave stops the preview.
    fn detach(&self) {
        let hub = self.hub();
        let mut clients = hub.clients.lock().unwrap();
        clients.retain(|c| c.id != self.id);
        if clients.is_empty() {
            let mut sh = hub.shared.lock().unwrap();
            sh.preview.stop();
            sh.idle_since = Some(std::time::Instant::now());
        }
    }

    /// Every client of this project, this one included.
    fn peers(&self) -> Vec<Server> {
        let hub = self.hub();
        let v = hub.clients.lock().unwrap().clone();
        if v.is_empty() { vec![self.clone()] } else { v }
    }

    /// Something changed for the whole project (a cycle, a command): every editor of it republishes.
    fn publish_everywhere(&self) {
        for c in self.peers() {
            c.publish_all();
        }
    }

    /// The project's language (Rust until a root is known).
    fn lang(&self) -> Lang {
        self.root().map_or(Lang::Rust, |r| Lang::detect(&r))
    }

    fn project(&self) -> Option<Project> {
        let mut p = Project::load(&self.root()?).ok()?;
        p.env = self.hub().shared.lock().unwrap().env.clone();
        Some(p)
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
        self.attach(p.root.clone());
        self.log(&format!("wishlang: serving project {}", p.root.display()));
        if let Ok(g) = Git::open(&p.root) {
            let _ = engine::ensure_journal(&p, &g);
        }
        self.publish_all();
    }

    /// Git for the project, on the current branch. After a branch switch the
    /// branch's journal may not exist yet: fork it now, so colors and hovers
    /// work before the first save on the new branch.
    fn git(&self) -> Option<Git> {
        let p = self.project()?;
        let g = Git::open(&p.root).ok()?;
        let tip = match g.journal() {
            Some(t) => t,
            None => engine::ensure_journal(&p, &g).ok()?,
        };
        // When your code moved (checkout, merge, pull), join in the wish
        // history of branches you merged, so their lines keep their provenance.
        let key = format!("{}@{}", g.branch(), g.code_head().unwrap_or_default());
        let changed = self.hub().shared.lock().unwrap().checked_head.as_deref() != Some(key.as_str());
        if changed {
            self.hub().shared.lock().unwrap().checked_head = Some(key);
            if !engine::journals_to_merge(&g, &tip).is_empty() {
                if let Ok(m) = engine::record_save(&g, &tip, "save") {
                    self.log(&format!("wishlang: {}", g.subject(&m)));
                }
            }
        }
        Some(g)
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
                    self.notify("wish/changed", json!({}));
                }
            }
            "textDocument/didClose" => {
                self.state.lock().unwrap().docs.remove(&rel);
            }
            "textDocument/didSave" if self.project().is_some_and(|p| p.lang.is_src(&rel)) => self.cycle(Options::default()),
            _ => {}
        }
    }

    // ---- file state ----

    /// The items of a buffer; while it has a syntax error, those of its last good parse.
    fn items_of(&self, lang: Lang, rel: &str, text: &str) -> Vec<Item> {
        match items::parse(lang, text) {
            Ok(items) => {
                self.hub().shared.lock().unwrap().last_items.insert(rel.to_string(), items.clone());
                items
            }
            Err(_) => self.hub().shared.lock().unwrap().last_items.get(rel).cloned().unwrap_or_default(),
        }
    }

    /// A file on disk, parsed once per change: its items and (Java) implicit members.
    fn indexed(&self, p: &Project, f: &str) -> Option<(Arc<Vec<Item>>, Arc<Vec<(String, Option<String>)>>)> {
        let meta = std::fs::metadata(p.root.join(f)).ok()?;
        let stamp = (meta.len(), meta.modified().ok());
        let cached = self.hub().shared.lock().unwrap().index.get(f).filter(|i| i.stamp == stamp).map(|i| (i.items.clone(), i.implicit.clone()));
        if cached.is_some() {
            return cached;
        }
        let src = p.read(f)?;
        let items = Arc::new(items::parse(p.lang, &src).unwrap_or_default());
        let members = Arc::new(if p.lang == Lang::Java { crate::java::implicit_members(&src) } else { vec![] });
        self.hub().shared.lock().unwrap().index.insert(f.to_string(), Indexed { stamp, items: items.clone(), implicit: members.clone() });
        Some((items, members))
    }

    /// Where each item is (path -> file, 1-based line), like [`Project::item_locations`] but from the index.
    fn item_locations(&self, p: &Project) -> HashMap<String, (String, usize)> {
        let mut locations = HashMap::new();
        for f in p.src_files() {
            let open = self.state.lock().unwrap().docs.get(&f).cloned();
            let items = match open {
                Some(src) => Arc::new(self.items_of(p.lang, &f, &src)),
                None => match self.indexed(p, &f) {
                    Some((items, _)) => items,
                    None => continue,
                },
            };
            for it in items.iter() {
                locations.entry(it.path.clone()).or_insert((f.clone(), it.line));
            }
        }
        locations
    }

    /// Uses of things the rest of the project doesn't define yet (open buffers win over disk).
    fn pending_in(&self, p: &Project, rel: &str, text: &str) -> Vec<Pending> {
        let mut all: Vec<Arc<Vec<Item>>> = Vec::new();
        let mut implicit: Vec<Arc<Vec<(String, Option<String>)>>> = Vec::new();
        for f in p.src_files().iter().filter(|f| f.as_str() != rel) {
            let open = self.state.lock().unwrap().docs.get(f).cloned();
            if let Some(src) = open {
                all.push(Arc::new(self.items_of(p.lang, f, &src)));
                if p.lang == Lang::Java {
                    implicit.push(Arc::new(crate::java::implicit_members(&src)));
                }
                continue;
            }
            let Some((items, members)) = self.indexed(p, f) else { continue };
            all.push(items);
            implicit.push(members);
        }
        let mut known = pending::Known::from_items(all.iter().flat_map(|v| v.iter()));
        for (path, ty) in implicit.iter().flat_map(|v| v.iter()) {
            known.add_member(path, ty.as_deref());
        }
        pending::find(p.lang, text, &known).unwrap_or_default()
    }

    /// Everything an editor shows for one file; `with_pending` also finds what will be generated.
    fn file_state(&self, rel: &str, with_pending: bool) -> Option<Arc<FileState>> {
        let p = self.project()?;
        let text = self.text(rel)?;
        let git = self.git();
        let key = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            text.hash(&mut h);
            let refs = git.as_ref().map(|g| (g.branch().to_string(), g.journal(), g.code_head()));
            format!("{rel}\0{:x}\0{refs:?}", h.finish())
        };
        let cached = self.hub().shared.lock().unwrap().last_state.clone().filter(|(k, fs)| *k == key && (fs.with_pending || !with_pending));
        if let Some((_, fs)) = cached {
            return Some(fs);
        }
        let (whos, shas): (Vec<Who>, Vec<Option<String>>) = git.as_ref().and_then(|g| g.blame_lines(rel, &text)).unwrap_or_default().into_iter().unzip();
        let src = p.lang.is_src(rel) && !p.is_build_output(rel);
        let items = if src { self.items_of(p.lang, rel, &text) } else { vec![] };
        let mocked: Vec<(usize, usize)> = items.iter().filter(|i| i.mocked().is_some()).map(|i| (i.line, i.end_line)).collect();
        let (generated, prototype) = paint_lines(&whos, &text, &mocked);
        let pending = if src && with_pending { self.pending_in(&p, rel, &text) } else { vec![] };
        let lenses = lenses(&p, rel, &text, &items);
        let mut blocks = item_blocks(&whos, &text, &items);
        let ticket = git.as_ref().and_then(|g| self.ticket_trace(&p, g));
        if let Some(t) = &ticket {
            for b in &mut blocks {
                if let Some(served) = b.item.as_deref().and_then(|i| ticket::problems_of(&t.union, i)) {
                    b.problems = served.iter().cloned().collect();
                    b.untraced = served.is_empty();
                }
            }
        }
        let mut lenses = lenses;
        if ticket.as_ref().is_some_and(|t| t.notes_dir.is_some()) {
            for b in blocks.iter().filter(|b| !b.problems.is_empty()) {
                lenses.push(Lens { line: b.start, title: format!("◆ {}", b.problems.join(" · ")), command: "wish.ticket.open".into(), arguments: json!([b.problems[0]]) });
            }
        }
        let fs = Arc::new(FileState { git, ticket, text, items, whos, shas, generated, prototype, pending, with_pending, lenses, blocks });
        self.hub().shared.lock().unwrap().last_state = Some((key, fs.clone()));
        Some(fs)
    }

    fn file_state_of_uri(&self, params: &Value, with_pending: bool) -> Option<Arc<FileState>> {
        self.file_state(&params["textDocument"]["uri"].as_str().and_then(|u| self.rel_of_uri(u))?, with_pending)
    }

    /// `wish/fileState`: everything an editor needs to decorate one file.
    fn wire_file_state(&self, params: &Value) -> Value {
        let line_range = |l: &usize, text: &str| range(*l as u32, 0, *l as u32, line_len(text, *l));
        let Some(fs) = self.file_state_of_uri(params, true) else {
            return serde_json::to_value(WireFileState { generated: vec![], prototype: vec![], pending: vec![], lenses: vec![], blocks: vec![] }).unwrap_or_default();
        };
        let lang = self.lang();
        let wire = WireFileState {
            generated: fs.generated.iter().map(|l| line_range(l, &fs.text)).collect(),
            prototype: fs.prototype.iter().map(|l| line_range(l, &fs.text)).collect(),
            pending: fs.pending.iter().map(|x| WirePending { range: range(x.start.0, x.start.1, x.end.0, x.end.1), name: x.name.clone() }).collect(),
            lenses: fs.lenses.clone(),
            blocks: fs.blocks.iter().map(|b| WireBlock { start: b.start, end: b.end, item: b.item.clone(), hover: hover_markdown(&fs, lang, b.anchor), problems: b.problems.clone(), untraced: b.untraced }).collect(),
        };
        serde_json::to_value(wire).unwrap_or_default()
    }

    /// `wish/show {sha, path, base?}`: a file before and after a journal commit
    /// (before = `base`, else the commit's parent), for a diff view.
    fn show_file(&self, params: &Value) -> Result<Value> {
        let (sha, path, base) = (params["sha"].as_str().unwrap_or(""), params["path"].as_str().unwrap_or(""), params["base"].as_str());
        let rev = regex::Regex::new(r"^[0-9a-f]{7,40}\^?$").unwrap();
        if !rev.is_match(sha) || base.is_some_and(|b| !rev.is_match(b)) {
            bail!("invalid revision (expected a hex sha, optionally followed by ^)");
        }
        if path.is_empty() || path.starts_with('/') || path.split('/').any(|s| s == "..") {
            bail!("invalid path {path:?} (expected a path relative to the project)");
        }
        let git = self.git().context("no wishlang project")?;
        let after = git.resolve(sha).with_context(|| format!("unknown revision {sha}"))?;
        let before = base.map(String::from).unwrap_or_else(|| format!("{sha}^"));
        // A file the commit added has no "before"; a root commit has no parent.
        Ok(json!({
            "before": git.resolve(&before).and_then(|b| git.show(&b, path)).unwrap_or_default(),
            "after": git.show(&after, path).unwrap_or_default(),
        }))
    }

    /// The ticket trace of the current branch, cached per journal tip (an
    /// edit of the problems clears it; `wish/ticket` always recomputes).
    fn ticket_trace(&self, p: &Project, git: &Git) -> Option<Arc<Trace>> {
        ticket::key(git.branch())?;
        let key = format!("{}@{}", git.branch(), git.journal()?);
        let cached = self.hub().shared.lock().unwrap().trace.as_ref().filter(|(k, _)| *k == key).map(|(_, t)| t.clone());
        if let Some(t) = cached {
            return t;
        }
        let t = match ticket::trace(p, git) {
            Ok(t) => Some(Arc::new(t)),
            Err(e) => {
                self.log(&format!("wishlang: ticket: {e:#}"));
                None
            }
        };
        self.hub().shared.lock().unwrap().trace = Some((key, t.clone()));
        t
    }

    /// `wish/ticket`: the ticket of the current branch with the items that serve each problem.
    /// `wish/ticket {branch?}`: the trace of this branch's ticket, or of another ticket branch.
    fn ticket_request(&self, branch: Option<&str>) -> Result<Value> {
        let p = self.project().context("no wishlang project")?;
        let git = self.git().context("no wishlang project")?;
        if let Some(b) = branch.filter(|b| *b != git.branch()) {
            let t = ticket::trace_branch(&p, &git, b)?;
            // Files are relative to the checkout that has the branch; without one there is no root to open them from.
            let root = t.checkout.as_deref().map(path_to_uri);
            let mut v = serde_json::to_value(t)?;
            v["root"] = json!(root);
            return Ok(v);
        }
        let t = ticket::trace(&p, &git)?;
        self.hub().shared.lock().unwrap().trace = Some((format!("{}@{}", git.branch(), git.journal().unwrap_or_default()), Some(Arc::new(t.clone()))));
        Ok(with_root(serde_json::to_value(t)?, &p))
    }

    /// `wish/tickets`: every ticket branch with a journal, the current one first.
    fn tickets_request(&self) -> Result<Value> {
        let p = self.project().context("no wishlang project")?;
        let git = self.git().context("no wishlang project")?;
        Ok(serde_json::to_value(ticket::tickets(&p, &git))?)
    }

    /// `wish.ticket.edit [problems, branch?]`: replace the `problems.json` of this branch's ticket (or `branch`'s).
    fn edit_problems(&self, p: &Project, arg: Option<&Value>, branch: Option<&str>) -> Result<String> {
        let ps: Vec<Problem> = serde_json::from_value(arg.cloned().unwrap_or_default()).context("expected a list of problems [{id, text, scenarios}]")?;
        let git = Git::open(&p.root)?;
        let branch = branch.filter(|b| !b.is_empty()).unwrap_or(git.branch());
        let key = ticket::key(branch).with_context(|| format!("branch {branch} has no ticket"))?;
        let dir = ticket::notes_dir(&p.root, &key).with_context(|| format!("no .work/{key} in the main checkout"))?;
        ticket::save_problems(&dir, &ps)?;
        self.hub().shared.lock().unwrap().trace = None;
        Ok(format!("{key}: {} problem(s) saved", ps.len()))
    }

    /// Open the ticket's problems.json in the editor, selecting the line of problem `id`.
    fn open_problem(&self, p: &Project, id: &str) {
        let Some(dir) = self.git().and_then(|g| self.ticket_trace(p, &g)).and_then(|t| t.notes_dir.clone()) else { return };
        let file = Path::new(&dir).join("problems.json");
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        let line = ["\"id\": \"", "\"id\":\""].iter().find_map(|k| text.find(&format!("{k}{id}\""))).map_or(0, |at| text[..at].matches('\n').count() as u32);
        self.ask("window/showDocument", json!({"uri": path_to_uri(&file), "takeFocus": true, "selection": range(line, 0, line, 0)}), Box::new(|_| {}));
    }

    /// `wish/log`: what wish did, for a history view.
    fn journal_log(&self) -> Result<Value> {
        let p = self.project().context("no wishlang project")?;
        let git = self.git().context("no wishlang project")?;
        let journal = git.journal().context("no journal yet")?;
        let locs = self.item_locations(&p);
        let entries = git
            .history(&journal)
            .into_iter()
            .map(|e| {
                let m = CycleMessage::parse(&format!("{}\n\n{}", e.subject, e.body));
                let why = m
                    .why
                    .into_iter()
                    .map(|w| {
                        let at = locate(&locs, &w.item);
                        WireWhy { file: at.map(|a| a.0.clone()), line: at.map(|a| a.1), item: w.item, why: w.why, problems: w.problems }
                    })
                    .collect();
                WireEntry { sha: e.sha, author: e.who.name(), when: e.age, label: m.label, summary: m.summary, ticket: m.ticket, why, triggered_by: m.triggered_by, decisions: m.decisions, mode: m.mode, files: e.files }
            })
            .collect();
        let decisions = p.decisions().into_iter().map(|d| WireDecision { id: d.id, title: d.title, choice: d.choice, why: d.why, status: d.status }).collect();
        let problems = self.ticket_trace(&p, &git).map(|t| t.problems.iter().map(|x| WireProblem { id: x.id.clone(), text: x.text.clone() }).collect()).unwrap_or_default();
        Ok(with_root(serde_json::to_value(WireLog { entries, decisions, problems })?, &p))
    }

    // ---- diagnostics ----

    fn diagnostics(&self, rel: &str) -> Vec<Value> {
        let Some(p) = self.project() else { return vec![] };
        let mut out = Vec::new();
        if let Some(fs) = self.file_state(rel, true).filter(|_| p.lang.is_src(rel)) {
            for pnd in &fs.pending {
                out.push(json!({
                    "range": range(pnd.start.0, pnd.start.1, pnd.end.0, pnd.end.1),
                    "severity": 3, "source": "wishlang",
                    "message": format!("✦ will be generated: {}", pnd.name),
                }));
            }
            if let Some(t) = &fs.ticket {
                for b in fs.blocks.iter().filter(|b| b.untraced) {
                    out.push(json!({"range": range(b.anchor as u32, 0, b.anchor as u32, line_len(&fs.text, b.anchor)), "severity": 2, "source": "wishlang",
                        "code": "untraced", "message": format!("serves no problem of {}", t.key)}));
                }
            }
            let text = &fs.text;
            for s in p.suggestions().into_iter().filter(|s| s.status == Status::Proposed && s.file == rel) {
                if let Some(at) = text.find(&s.replace) {
                    let l0 = text[..at].matches('\n').count() as u32;
                    let l1 = l0 + s.replace.matches('\n').count() as u32;
                    // Warning, so every suggestion is listed in Zed's project diagnostics.
                    out.push(json!({"range": range(l0, 0, l1, line_len(text, l1 as usize)), "severity": 2, "source": "wishlang",
                        "code": format!("suggestion-{}", s.id),
                        "message": format!("✎ suggestion {}: {}\nproposed:\n{}", s.id, s.reason, s.with)}));
                }
            }
        }
        out.extend(self.hub().shared.lock().unwrap().check.get(rel).cloned().unwrap_or_default());
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
        self.hub().shared.lock().unwrap().last_state = None;
        let rels: std::collections::BTreeSet<String> = {
            let checked: Vec<String> = self.hub().shared.lock().unwrap().check.keys().cloned().collect();
            let st = self.state.lock().unwrap();
            st.docs.keys().chain(st.published.iter()).chain(checked.iter()).cloned().collect::<std::collections::BTreeSet<_>>()
        };
        // Files with pending suggestions or untraced items too, so they all appear in Zed's project diagnostics.
        let mut rels = rels;
        if let Some(p) = self.project() {
            rels.extend(p.suggestions().into_iter().filter(|s| s.status == Status::Proposed).map(|s| s.file));
            if let Some(t) = self.git().and_then(|g| self.ticket_trace(&p, &g)) {
                rels.extend(t.untraced.iter().map(|i| i.file.clone()));
            }
        }
        for r in rels {
            self.publish(&r);
        }
        for refresh in ["workspace/semanticTokens/refresh", "workspace/inlayHint/refresh", "workspace/codeLens/refresh"] {
            self.ask(refresh, Value::Null, Box::new(|_| {}));
        }
        self.notify("wish/changed", json!({}));
    }

    // ---- requests ----

    fn request(&self, req: Request) {
        if let Some(uri) = req.params["textDocument"]["uri"].as_str() {
            self.discover_from_uri(uri);
        }
        let result: Result<Value> = match req.method.as_str() {
            "textDocument/semanticTokens/full" => Ok(self.semantic_tokens(&req.params)),
            "textDocument/hover" => Ok(self.hover(&req.params)),
            "textDocument/inlayHint" => Ok(self.inlay_hints(&req.params)),
            "textDocument/codeLens" => Ok(self.code_lenses(&req.params)),
            "textDocument/codeAction" => Ok(self.code_actions(&req.params)),
            "wish/fileState" => Ok(self.wire_file_state(&req.params)),
            "wish/log" => self.journal_log(),
            "wish/ticket" => self.ticket_request(req.params["branch"].as_str()),
            "wish/tickets" => self.tickets_request(),
            "wish/show" => self.show_file(&req.params),
            "workspace/executeCommand" => {
                self.execute(&req.params);
                Ok(Value::Null)
            }
            _ => Ok(Value::Null),
        };
        match result {
            Ok(v) => self.reply(req.id, v),
            Err(e) => self.out.send(Message::Response(Response::new_err(req.id, ErrorCode::RequestFailed as i32, format!("{e:#}")))),
        }
    }

    /// Every generated line as a `generated` semantic token; Zed's token rule
    /// paints it blue. Lines inside a still-mocked item also get `prototype`.
    fn semantic_tokens(&self, params: &Value) -> Value {
        let Some(fs) = self.file_state_of_uri(params, false) else { return json!({"data": []}) };
        let untraced: Vec<usize> = fs.blocks.iter().filter(|b| b.untraced).flat_map(|b| b.start..=b.end).collect();
        json!({"data": encode_tokens(&fs.text, &fs.generated, &fs.prototype, &untraced)})
    }

    /// Blame the hovered line and show the reasoning recorded in its journal commit.
    fn hover(&self, params: &Value) -> Value {
        let Some(fs) = self.file_state_of_uri(params, false) else { return Value::Null };
        let line = params["position"]["line"].as_u64().unwrap_or(0) as usize;
        if fs.whos.get(line).is_none() || fs.text.lines().nth(line).is_none_or(|l| l.trim().is_empty()) {
            return Value::Null;
        }
        let md = hover_markdown(&fs, self.lang(), line);
        json!({"contents": {"kind": "markdown", "value": md}, "range": range(line as u32, 0, line as u32, line_len(&fs.text, line))})
    }

    /// Always-visible annotations: `✎` on your lines inside generated files,
    /// and a `◇ prototype` tag after the signature of every still-mocked item.
    fn inlay_hints(&self, params: &Value) -> Value {
        let Some(fs) = self.file_state_of_uri(params, false) else { return json!([]) };
        let text = &fs.text;
        let mut hints = Vec::new();
        // One marker per block of your lines, only where they mix with generated code.
        if fs.whos.contains(&Who::Claude) {
            let lines: Vec<&str> = text.lines().collect();
            for b in provenance::blocks(&fs.whos, text).into_iter().filter(|b| b.who == Who::Human) {
                let Some(first) = (b.start..=b.end).find(|i| lines.get(*i).is_some_and(|l| !l.trim().is_empty())) else { continue };
                let n = (b.start..=b.end).filter(|i| lines.get(*i).is_some_and(|l| !l.trim().is_empty())).count();
                let label = if n == 1 { "✎ yours".to_string() } else { format!("✎ yours · {n} lines") };
                hints.push(json!({"position": {"line": first, "character": line_len(text, first)}, "label": label, "paddingLeft": true, "tooltip": "Written by you. Protected: Claude keeps these lines and makes them true."}));
            }
        }
        for it in &fs.items {
            if let Some(m) = it.mocked() {
                let l = it.line.max(1) - 1;
                let what: String = m.join("; ").chars().take(80).collect();
                let label = if what.is_empty() { "◇ prototype".to_string() } else { format!("◇ prototype · mocked: {what}") };
                hints.push(json!({"position": {"line": l, "character": line_len(text, l)}, "label": label, "paddingLeft": true, "tooltip": "Still mocked. Use the ▶ Implement lens above to make it real."}));
            }
        }
        json!(hints)
    }

    /// Clickable actions above code: implement a prototype item, and accept or
    /// reject Claude's suggestions in place.
    fn code_lenses(&self, params: &Value) -> Value {
        let Some(fs) = self.file_state_of_uri(params, false) else { return json!([]) };
        let lens = |l: &Lens| json!({"range": range(l.line as u32, 0, l.line as u32, 0), "command": {"title": l.title, "command": l.command, "arguments": l.arguments}});
        json!(fs.lenses.iter().map(lens).collect::<Vec<_>>())
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
        let own = self.items_of(p.lang, &rel, &text);
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
        self.hub().shared.lock().unwrap().last_state = None;
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
                    self.publish_everywhere();
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
            // What a save does: a cycle for your edits since the last one (nothing to do: no Claude call).
            "wish.cycle" => {
                self.cycle(Options::default());
                return;
            }
            "wish.regenerate" => {
                self.cycle(Options { force: true, ..Options::default() });
                return;
            }
            "wish.ticket.edit" => self.edit_problems(&p, args.first(), args.get(1).and_then(|v| v.as_str())),
            "wish.ticket.open" => {
                self.open_problem(&p, &s(0));
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
        self.publish_everywhere();
    }

    fn next_block(&self, uri: &str, line: usize, human: bool) {
        let Some(fs) = self.rel_of_uri(uri).and_then(|rel| self.file_state(&rel, false)) else { return };
        let blocks = provenance::blocks(&fs.whos, &fs.text);
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
            if self.root().is_none() {
                return;
            }
            let hub = self.hub();
            let mut st = hub.shared.lock().unwrap();
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
                let diagnostics = match me.lang() {
                    Lang::Rust => parse_cargo(&errors),
                    Lang::Java => parse_mvn(&errors),
                };
                me.hub().shared.lock().unwrap().check = diagnostics;
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
            me.publish_everywhere();
            let again = {
                let hub = me.hub();
                let mut st = hub.shared.lock().unwrap();
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
        let r = self.hub().shared.lock().unwrap().preview.restart(&p, move |line| me.log(&format!("[app] {line}")));
        if let Err(e) = r {
            self.show(1, &format!("wishlang: could not start the app: {e}"));
        }
    }

    fn rebuild_preview(&self) {
        let me = self.clone();
        std::thread::spawn(move || me.build_and_restart());
    }
}

/// Claude's non-blank lines (0-based), split into those inside a still-mocked
/// item (`mocked` holds 1-based inclusive line ranges) and the rest. A line
/// is never in both.
fn paint_lines(whos: &[Who], text: &str, mocked: &[(usize, usize)]) -> (Vec<usize>, Vec<usize>) {
    let (mut generated, mut prototype) = (Vec::new(), Vec::new());
    for (i, (l, who)) in text.lines().zip(whos).enumerate() {
        if *who != Who::Claude || l.trim().is_empty() {
            continue;
        }
        if mocked.iter().any(|(a, b)| (*a..=*b).contains(&(i + 1))) {
            prototype.push(i);
        } else {
            generated.push(i);
        }
    }
    (generated, prototype)
}

/// LSP semantic token data: one `generated` token per generated line (whole
/// line), with the `prototype` modifier on prototype lines and `untraced` on
/// lines in `untraced`.
pub fn encode_tokens(text: &str, generated: &[usize], prototype: &[usize], untraced: &[usize]) -> Vec<u32> {
    let mut painted: Vec<(usize, bool)> = generated.iter().map(|l| (*l, false)).chain(prototype.iter().map(|l| (*l, true))).collect();
    painted.sort_unstable();
    let mut data = Vec::new();
    let mut prev = 0usize;
    for (line, proto) in painted {
        data.extend([(line - prev) as u32, 0, line_len(text, line), 0, proto as u32 | (untraced.contains(&line) as u32) << 1]);
        prev = line;
    }
    data
}

/// First line (0-based) of the doc comment and attributes above the item
/// whose name is on `line`: Rust `///` and `#[..]`, Java `/** .. */` and `@..`.
fn doc_start(lines: &[&str], line: usize) -> usize {
    let mut start = line.min(lines.len());
    while start > 0 {
        let t = lines[start - 1].trim();
        if t.starts_with("///") || t.starts_with("#[") || t.starts_with('@') {
            start -= 1;
        } else if t.ends_with("*/") && (t.starts_with("/*") || t.starts_with('*')) {
            match (0..start).rev().find(|&i| lines[i].trim_start().starts_with("/*")) {
                Some(open) => start = open,
                None => break,
            }
        } else {
            break;
        }
    }
    start
}

/// Split Claude's lines into blocks: runs of generated lines that start at
/// each item (at its doc comment, if it has one) or after one of your lines.
/// Blank lines belong to whichever block they sit in.
fn item_blocks(whos: &[Who], text: &str, items: &[Item]) -> Vec<FileBlock> {
    let lines: Vec<&str> = text.lines().collect();
    let starts: Vec<usize> = items.iter().filter(|i| i.line > 0).map(|i| doc_start(&lines, i.line - 1)).collect();
    let mut out = Vec::new();
    let mut open: Option<(usize, usize)> = None;
    let mut close = |run: Option<(usize, usize)>| {
        let Some((start, end)) = run else { return };
        let named = |i: usize| items.iter().find(|it| it.line == i + 1);
        let anchor = (start..=end).find(|&i| named(i).is_some()).or_else(|| (start..=end).find(|&i| !lines[i].trim().is_empty())).unwrap_or(start);
        let item = named(anchor).or_else(|| items.iter().filter(|it| it.line <= anchor + 1 && anchor < it.end_line).min_by_key(|it| it.end_line - it.line)).map(|it| it.path.clone());
        out.push(FileBlock { start, end, anchor, item, problems: vec![], untraced: false });
    };
    for (i, (l, who)) in lines.iter().zip(whos).enumerate() {
        if *who != Who::Claude || (starts.contains(&i) && open.is_some()) {
            close(open.take());
        }
        if *who == Who::Claude && !l.trim().is_empty() {
            open = Some((open.map_or(i, |o| o.0), i));
        }
    }
    close(open.take());
    out
}

/// Lenses for one file: implement a prototype item (above its doc comment),
/// and accept or reject Claude's suggestions in place.
fn lenses(p: &Project, rel: &str, text: &str, items: &[Item]) -> Vec<Lens> {
    let lines: Vec<&str> = text.lines().collect();
    let lens = |line: usize, title: String, command: &str, arguments: Value| Lens { line, title, command: command.into(), arguments };
    let mut out = Vec::new();
    let mocked: Vec<&Item> = items.iter().filter(|i| i.mocked().is_some()).collect();
    if mocked.len() > 1 {
        out.push(lens(0, format!("▶ Implement all {} prototype items in this file", mocked.len()), "wish.implement", json!([rel, null])));
    }
    for it in mocked {
        out.push(lens(doc_start(&lines, it.line.max(1) - 1), format!("▶ Implement `{}`", it.path), "wish.implement", json!([rel, it.path])));
    }
    for s in p.suggestions().into_iter().filter(|s| s.status == Status::Proposed && s.file == rel) {
        if let Some(at) = text.find(&s.replace) {
            let l = text[..at].matches('\n').count();
            let reason: String = s.reason.chars().take(90).collect();
            out.push(lens(l, format!("✓ Accept suggestion {}: {reason}", s.id), "wish.accept", json!([s.id])));
            out.push(lens(l, "✗ Reject".into(), "wish.reject", json!([s.id])));
        }
    }
    out
}

/// Hover text for one line: who wrote it, when, and why (from the journal commit).
fn hover_markdown(fs: &FileState, lang: Lang, line: usize) -> String {
    let (Some(git), Some(&who), Some(sha)) = (&fs.git, fs.whos.get(line), fs.shas.get(line)) else { return String::new() };
    let Some(sha) = sha.as_deref() else { return "✎ **yours**, not saved yet".into() };
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
    let item = fs.items.iter().filter(|i| i.line <= line + 1 && line + 1 <= i.end_line).min_by_key(|i| i.end_line - i.line);
    let why = item.and_then(|i| m.why_for(&i.path).map(|w| (i.path.clone(), w.to_string()))).or_else(|| {
        // Members that aren't separate items (methods in trait impls): match
        // the nearest enclosing `fn name` / `name(` against reasons given for `Type::name` / `Type.name`.
        let re = regex::Regex::new(match lang {
            Lang::Rust => r"\bfn\s+(\w+)",
            Lang::Java => r"\b(\w+)\s*\(",
        })
        .unwrap();
        let lines: Vec<&str> = fs.text.lines().collect();
        let start = item.map(|i| i.line.saturating_sub(1)).unwrap_or(0);
        let name = (start..=line.min(lines.len().saturating_sub(1))).rev().find_map(|l| re.captures(lines[l]).map(|c| c[1].to_string()))?;
        m.why.iter().find(|w| w.item.split('(').next().and_then(|p| p.rsplit([':', '.']).next()) == Some(name.as_str())).map(|w| (w.item.clone(), w.why.clone()))
    });
    if let Some(t) = &fs.ticket {
        let path = why.as_ref().map(|(path, _)| path).or(item.map(|i| &i.path));
        for id in path.and_then(|path| ticket::problems_of(&t.union, path)).into_iter().flatten() {
            let text = t.problems.iter().find(|p| p.id == id).map(|p| p.text.as_str()).unwrap_or("");
            s.push_str(&format!("\n\n**Solves {id}**: {text}"));
        }
    }
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

/// Turn `mvn -q compile` output (`[ERROR] File.java:[line,col] message`, paths
/// relative to the project) into diagnostics.
fn parse_mvn(text: &str) -> BTreeMap<String, Vec<Value>> {
    let re = regex::Regex::new(r"^\[(ERROR|WARNING)\]\s+(\S+\.java):\[(\d+),(\d+)\]\s+(.*)$").unwrap();
    let mut m: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for l in text.lines() {
        if let Some(c) = re.captures(l) {
            let line = c[3].parse::<u32>().unwrap_or(1).saturating_sub(1);
            let col = c[4].parse::<u32>().unwrap_or(1).saturating_sub(1);
            let sev = if &c[1] == "ERROR" { 1 } else { 2 };
            m.entry(c[2].to_string()).or_default().push(json!({"range": range(line, col, line, col + 1), "severity": sev, "source": "wishlang (mvn)", "message": c[5].to_string()}));
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
pub(crate) mod tests {
    use super::*;
    use crate::journal::Why;

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
        let (generated, prototype) = paint_lines(&whos, text, &[(5, 5)]);
        assert_eq!((&generated[..], &prototype[..]), (&[0, 2][..], &[4][..]));
        assert_eq!(encode_tokens(text, &generated, &prototype, &[]), [0, 0, 1, 0, 0, 2, 0, 2, 0, 0, 2, 0, 3, 0, 1]);
        // `untraced` is bit 1 (prototype is bit 0), and can combine with it.
        assert_eq!(encode_tokens(text, &generated, &prototype, &[2, 4]), [0, 0, 1, 0, 0, 2, 0, 2, 0, 2, 2, 0, 3, 0, 3]);
    }

    #[test]
    fn mvn_diagnostics() {
        let m = parse_mvn("[ERROR] src/main/java/demo/Main.java:[7,42] cannot find symbol\n[ERROR]   symbol:   method open(int,int)\n[WARNING] src/main/java/demo/App.java:[3,1] unchecked call\n[ERROR] -> [Help 1]");
        assert_eq!(m["src/main/java/demo/Main.java"].len(), 1);
        assert_eq!(m["src/main/java/demo/Main.java"][0]["severity"], 1);
        assert_eq!(m["src/main/java/demo/Main.java"][0]["range"]["start"]["character"], 41);
        assert_eq!(m["src/main/java/demo/App.java"][0]["severity"], 2);
    }

    #[test]
    fn cargo_diagnostics() {
        let m = parse_cargo("src/main.rs:7:42: error[E0599]: no variant named `Windowed`\nwarning: unused\nsrc/window.rs:3:1: warning: dead code");
        assert_eq!(m["src/main.rs"][0]["severity"], 1);
        assert_eq!(m["src/main.rs"][0]["range"]["start"]["line"], 6);
        assert_eq!(m["src/window.rs"][0]["severity"], 2);
    }

    const APP: &str = "package demo;\n\n/** The app. */\npublic class App {\n    /**\n     * Makes one.\n     * <h2>Mocked</h2>\n     * returns a stub\n     */\n    @Deprecated\n    public static App create(String name) { return new App(); }\n\n    /** Runs it. */\n    public void run() {}\n}\n";

    #[test]
    fn doc_start_handles_rust_and_java() {
        let java: Vec<&str> = APP.lines().collect();
        assert_eq!(doc_start(&java, 10), 4); // create: Javadoc + @Deprecated
        assert_eq!(doc_start(&java, 3), 2); // class: one-line Javadoc
        let rust: Vec<&str> = "/// a\n#[derive(Debug)]\nstruct S;".lines().collect();
        assert_eq!(doc_start(&rust, 2), 0);
        let none: Vec<&str> = "int x; /* c */\nint y;".lines().collect();
        assert_eq!(doc_start(&none, 1), 1);
    }

    #[test]
    fn paint_lines_are_disjoint() {
        let items = items::parse(Lang::Java, APP).unwrap();
        let mocked: Vec<(usize, usize)> = items.iter().filter(|i| i.mocked().is_some()).map(|i| (i.line, i.end_line)).collect();
        let whos = vec![Who::Claude; APP.lines().count()];
        let (generated, prototype) = paint_lines(&whos, APP, &mocked);
        assert_eq!(prototype, [10]);
        assert!(generated.iter().all(|l| !prototype.contains(l)));
        assert!(!generated.contains(&1)); // blank lines are never painted
    }

    #[test]
    fn blocks_start_at_items_and_their_docs() {
        let items = items::parse(Lang::Java, APP).unwrap();
        let whos = vec![Who::Claude; APP.lines().count()];
        let blocks = item_blocks(&whos, APP, &items);
        let got: Vec<(usize, usize, usize, Option<&str>)> = blocks.iter().map(|b| (b.start, b.end, b.anchor, b.item.as_deref())).collect();
        // The package line, the class header, `create` with Javadoc and annotation, and `run` with the closing brace.
        assert_eq!(got, [(0, 0, 0, None), (2, 3, 3, Some("App")), (4, 10, 10, Some("App.create")), (12, 14, 13, Some("App.run"))]);
        // Your lines split blocks too.
        let mut mixed = whos.clone();
        mixed[10] = Who::Human;
        assert_eq!(item_blocks(&mixed, APP, &items).iter().map(|b| (b.start, b.end)).collect::<Vec<_>>(), [(0, 0), (2, 3), (4, 9), (12, 14)]);
    }

    // ---- in-process server on a temp git repo ----

    use std::process::Command as Cmd;

    fn temp_project() -> PathBuf {
        temp_project_named("lsp")
    }

    /// A Java wish project in a fresh git repository, with a ticket branch.
    pub(crate) fn temp_project_named(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wish-{name}-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let java = dir.join("src/main/java/demo");
        std::fs::create_dir_all(&java).unwrap();
        std::fs::write(dir.join("pom.xml"), "<project/>").unwrap();
        std::fs::write(dir.join("wish.toml"), "[protected]\nfiles = [\"src/main/java/demo/Main.java\"]\n").unwrap();
        std::fs::write(java.join("Main.java"), "package demo;\n\npublic class Main {\n    public static void main(String[] args) {\n        App.create(\"Demo\").run();\n        new Window(800, 600).show();\n    }\n}\n").unwrap();
        std::fs::write(dir.join(".gitignore"), ".work/\n").unwrap();
        // The ticket's notes live in the main checkout, which is this directory.
        let notes = dir.join(".work/DEMO-1");
        std::fs::create_dir_all(&notes).unwrap();
        std::fs::write(notes.join("intent.md"), "# DEMO-1 — Open the demo window\n\nThe demo should open a window.\n").unwrap();
        std::fs::write(notes.join("problems.json"), r#"[{"id": "P1", "text": "The demo window does not open", "scenarios": ["Open the window"]}, {"id": "P2", "text": "Closing the window leaves the app running"}]"#).unwrap();
        for args in [&["init", "-q"][..], &["add", "-A"], &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "init"], &["checkout", "-qb", "feature/DEMO-1-window"]] {
            assert!(Cmd::new("git").args(args).current_dir(&dir).status().unwrap().success());
        }
        dir
    }

    /// A client for an in-process server: sends requests and waits for their responses.
    struct Client {
        conn: Connection,
        next: i32,
        changed: usize,
        /// `window/showMessage` texts received so far.
        shown: Vec<String>,
        /// The latest diagnostics published per document uri.
        diagnostics: HashMap<String, Vec<Value>>,
        /// `window/showDocument` requests received so far.
        documents: Vec<Value>,
    }

    impl Client {
        fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
            self.next += 1;
            let id = RequestId::from(self.next);
            self.conn.sender.send(Message::Request(Request::new(id.clone(), method.into(), params))).unwrap();
            loop {
                match self.conn.receiver.recv_timeout(std::time::Duration::from_secs(20)).expect("server went quiet") {
                    Message::Response(r) if r.id == id => return r.response_result.map_err(|e| e.message),
                    Message::Request(r) => {
                        if r.method == "window/showDocument" {
                            self.documents.push(r.params.clone());
                        }
                        self.conn.sender.send(Message::Response(Response::new_ok(r.id, Value::Null))).unwrap();
                    }
                    Message::Notification(n) if n.method == "textDocument/publishDiagnostics" => {
                        self.diagnostics.insert(n.params["uri"].as_str().unwrap_or("").to_string(), n.params["diagnostics"].as_array().cloned().unwrap_or_default());
                    }
                    Message::Notification(n) if n.method == "wish/changed" => self.changed += 1,
                    Message::Notification(n) if n.method == "window/showMessage" => self.shown.push(n.params["message"].as_str().unwrap_or("").to_string()),
                    _ => {}
                }
            }
        }

        fn ok(&mut self, method: &str, params: Value) -> Value {
            self.call(method, params).unwrap_or_else(|e| panic!("{method}: {e}"))
        }

        fn notify(&self, method: &str, params: Value) {
            self.conn.sender.send(Message::Notification(Notification::new(method.into(), params))).unwrap();
        }
    }

    fn start(dir: &Path) -> (Client, std::thread::JoinHandle<Result<()>>) {
        let (server_side, client_side) = Connection::memory();
        let handle = std::thread::spawn(move || run_with(server_side, Vec::new()));
        let mut c = Client { conn: client_side, next: 0, changed: 0, shown: vec![], diagnostics: HashMap::new(), documents: vec![] };
        c.ok("initialize", json!({"capabilities": {}, "workspaceFolders": [{"uri": path_to_uri(dir)}]}));
        c.notify("initialized", json!({}));
        (c, handle)
    }

    fn stop(mut c: Client, handle: std::thread::JoinHandle<Result<()>>) {
        c.ok("shutdown", Value::Null);
        c.notify("exit", Value::Null);
        handle.join().unwrap().unwrap();
    }

    /// With WISH_BLESS=1, record a response (shas, ages and paths normalised) as test data for the IntelliJ plugin.
    fn bless(name: &str, value: &Value, root: &Path) {
        if std::env::var("WISH_BLESS").is_err() {
            return;
        }
        let mut text = serde_json::to_string_pretty(value).unwrap();
        for dir in [root.canonicalize().unwrap(), root.to_path_buf()] {
            text = text.replace(&dir.display().to_string(), "<root>");
        }
        for (re, to) in [(r"\b[0-9a-f]{40}\b", "<sha>"), (r"git show [0-9a-f]{8}", "git show <sha>"), (r"\d+ (second|minute|hour|day|week|month|year)s? ago", "<age>")] {
            text = regex::Regex::new(re).unwrap().replace_all(&text, to).to_string();
        }
        let out = Path::new(env!("CARGO_MANIFEST_DIR")).join("../intellij/src/test/testData");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join(format!("{name}.json")), text + "\n").unwrap();
    }

    #[test]
    fn serves_file_state_over_lsp() {
        let dir = temp_project();
        // Two ticket cycles on top of the baseline, as the engine would record them: the first adds
        // two files and tags `create` with P1 and `run` with nothing, the second adds P2 to `create`.
        let (project, git) = (Project::load(&dir).unwrap(), Git::open(&dir).unwrap());
        let base = engine::ensure_journal(&project, &git).unwrap();
        let ticket_cycle = |why: Vec<Why>| CycleMessage { label: "cycle ok".into(), summary: "Added App.".into(), why, mode: "prototype".into(), ticket: Some("DEMO-1".into()), ..Default::default() };
        let tagged = |item: &str, problems: &[&str]| Why { item: item.into(), why: format!("Main calls {item}."), problems: problems.iter().map(|p| p.to_string()).collect() };
        std::fs::write(dir.join("src/main/java/demo/App.java"), APP).unwrap();
        std::fs::write(dir.join("src/main/java/demo/Quit.java"), "package demo;\n\n/** Quits. */\nclass Quit {}\n").unwrap();
        let first = git.snapshot(&[&base], Who::Claude, &ticket_cycle(vec![tagged("App.create", &["P1"]), tagged("App.run", &[])]).render()).unwrap();
        let second = git.snapshot(&[&first], Who::Claude, &ticket_cycle(vec![tagged("App.create", &["P2"])]).render()).unwrap();
        git.set_ref(&git.ref_name("journal"), &second).unwrap();
        let (mut c, handle) = start(&dir);
        let uri = |f: &str| path_to_uri(&dir.join("src/main/java/demo").join(f));
        let app_uri = uri("App.java");
        for (f, text) in [("Main.java", std::fs::read_to_string(dir.join("src/main/java/demo/Main.java")).unwrap()), ("App.java", APP.to_string())] {
            c.notify("textDocument/didOpen", json!({"textDocument": {"uri": uri(f), "languageId": "java", "version": 1, "text": text}}));
        }

        // Generated code, the prototype `create`, and the blocks with their anchored hover.
        let st = c.ok("wish/fileState", json!({"textDocument": {"uri": app_uri}}));
        bless("fileState", &st, &dir);
        assert_eq!(st["prototype"].as_array().unwrap().len(), 1);
        assert_eq!(st["prototype"][0]["start"]["line"], 10);
        assert!(st["generated"].as_array().unwrap().iter().all(|r| r["start"]["line"] != 10));
        let block = |item: &str| st["blocks"].as_array().unwrap().iter().find(|b| b["item"] == item).unwrap().clone();
        let create = block("App.create");
        assert_eq!((create["start"].as_u64(), create["end"].as_u64()), (Some(4), Some(10)));
        assert!(create["hover"].as_str().unwrap().contains("generated"));
        assert!(st["lenses"].as_array().unwrap().iter().any(|l| l["line"] == 4 && l["command"] == "wish.implement"));

        // Ticket tracing: `create` serves P1 and P2 (united over both cycles); `run` was tagged with nothing.
        assert_eq!((create["problems"].clone(), create["untraced"].clone()), (json!(["P1", "P2"]), json!(false)));
        let (run, class) = (block("App.run"), block("App"));
        assert_eq!((run["problems"].clone(), run["untraced"].clone()), (json!([]), json!(true)));
        assert_eq!(class["untraced"], false); // `App` itself was never in a ticket cycle
        assert!(create["hover"].as_str().unwrap().contains("**Solves P1**: The demo window does not open"));
        assert!(create["hover"].as_str().unwrap().contains("**Solves P2**: Closing the window leaves the app running"));
        assert!(!run["hover"].as_str().unwrap().contains("Solves"));
        let ticket_lens = st["lenses"].as_array().unwrap().iter().filter(|l| l["command"] == "wish.ticket.open").cloned().collect::<Vec<_>>();
        assert_eq!(ticket_lens.len(), 1);
        assert_eq!((ticket_lens[0]["line"].clone(), ticket_lens[0]["title"].clone(), ticket_lens[0]["arguments"].clone()), (json!(4), json!("◆ P1 · P2"), json!(["P1"])));
        let marks = c.diagnostics.get(&app_uri).cloned().unwrap_or_default();
        let untraced = marks.iter().find(|d| d["code"] == "untraced").expect("untraced diagnostic");
        assert_eq!((untraced["severity"].clone(), untraced["source"].clone(), untraced["message"].clone()), (json!(2), json!("wishlang"), json!("serves no problem of DEMO-1")));
        assert_eq!(untraced["range"]["start"]["line"], APP.lines().position(|l| l.contains("run() {}")).unwrap());
        // The lens opens problems.json at P2's line.
        c.ok("workspace/executeCommand", json!({"command": "wish.ticket.open", "arguments": ["P2"]}));
        c.ok("wish/log", Value::Null);
        assert!(c.documents[0]["uri"].as_str().unwrap().ends_with(".work/DEMO-1/problems.json"));

        // Your protected file has no generated lines; the wished `Window` is pending.
        let main = c.ok("wish/fileState", json!({"textDocument": {"uri": uri("Main.java")}}));
        assert!(main["generated"].as_array().unwrap().is_empty());
        assert!(main["pending"].as_array().unwrap().iter().any(|p| p["name"] == "Window"));

        // The standard handlers answer from the same state.
        let tokens = c.ok("textDocument/semanticTokens/full", json!({"textDocument": {"uri": app_uri}}));
        assert!(!tokens["data"].as_array().unwrap().is_empty());
        let hover = c.ok("textDocument/hover", json!({"textDocument": {"uri": app_uri}, "position": {"line": 13, "character": 4}}));
        assert!(hover["contents"]["value"].as_str().unwrap().contains("generated"));

        // wish/log: newest first, with each reason located in the code, its problems, files with line counts.
        let log = c.ok("wish/log", Value::Null);
        bless("log", &log, &dir);
        let entries = log["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 4);
        assert_eq!((entries[0]["sha"].as_str(), entries[0]["label"].as_str(), entries[0]["author"].as_str(), entries[0]["ticket"].as_str()), (Some(second.as_str()), Some("cycle ok"), Some("claude"), Some("DEMO-1")));
        assert_eq!(entries[1]["why"][0], json!({"item": "App.create", "why": "Main calls App.create.", "problems": ["P1"], "file": "src/main/java/demo/App.java", "line": 11}));
        assert_eq!(entries[1]["why"][1]["problems"], json!([]));
        assert_eq!(entries[1]["summary"], "Added App.");
        assert_eq!(entries[3]["author"], "you");
        let two_files = json!([{"path": "src/main/java/demo/App.java", "added": APP.lines().count(), "removed": 0}, {"path": "src/main/java/demo/Quit.java", "added": 4, "removed": 0}]);
        assert_eq!(entries[1]["files"], two_files);
        assert_eq!(log["problems"], json!([{"id": "P1", "text": "The demo window does not open"}, {"id": "P2", "text": "Closing the window leaves the app running"}]));
        assert_eq!(log["decisions"], json!([]));

        // wish/ticket: problems with their items and reasons per cycle, untraced items, and the ticket's whole diff.
        let t = c.ok("wish/ticket", Value::Null);
        bless("ticket", &t, &dir);
        assert_eq!((t["key"].as_str(), t["title"].as_str(), t["cycles"].as_u64()), (Some("DEMO-1"), Some("Open the demo window"), Some(2)));
        assert_eq!((t["head"].as_str(), t["base"].as_str()), (Some(second.as_str()), Some(format!("{first}^").as_str())));
        let items = |problem: usize| t["problems"][problem]["items"].as_array().unwrap().clone();
        assert_eq!(items(0).len(), 1);
        assert_eq!((items(0)[0]["item"].clone(), items(0)[0]["line"].clone(), items(0)[0]["reasons"].as_array().unwrap().len()), (json!("App.create"), json!(11), 1));
        assert_eq!(items(1)[0]["reasons"][0]["sha"], second.as_str());
        assert_eq!(t["problems"][0]["scenarios"], json!(["Open the window"]));
        assert_eq!(t["untraced"].as_array().unwrap().iter().map(|i| i["item"].clone()).collect::<Vec<_>>(), [json!("App.run")]);
        assert_eq!(t["files"], two_files);
        assert!(t["notesDir"].as_str().unwrap().ends_with(".work/DEMO-1"));
        assert!(t["root"].as_str().unwrap().starts_with("file:///"));

        // Editing the problems validates, saves and refreshes; a bad edit shows a message and changes nothing.
        let edit = |c: &mut Client, ps: Value| c.ok("workspace/executeCommand", json!({"command": "wish.ticket.edit", "arguments": [ps]}));
        let before = c.changed;
        edit(&mut c, json!([{"id": "P1", "text": "Only one now", "scenarios": []}]));
        assert!(c.changed > before);
        assert_eq!(c.ok("wish/ticket", Value::Null)["problems"].as_array().unwrap().len(), 1);
        edit(&mut c, json!([]));
        assert!(c.ok("wish/ticket", Value::Null)["problems"].as_array().unwrap().len() == 1);
        c.ok("wish/log", Value::Null);
        assert!(c.shown.iter().any(|m| m.contains("1 to 8 problems")), "{:?}", c.shown);

        // wish/show: the file before and after a journal commit, with validated arguments.
        let tip = entries[1]["sha"].as_str().unwrap().to_string();
        let shown = c.ok("wish/show", json!({"sha": tip, "path": "src/main/java/demo/App.java"}));
        assert_eq!((shown["before"].as_str(), shown["after"].as_str()), (Some(""), Some(APP)));
        for bad in [json!({"sha": "--output=x", "path": "pom.xml"}), json!({"sha": tip, "path": "../x"}), json!({"sha": tip, "path": "/etc/passwd"}), json!({"sha": tip, "path": "pom.xml", "base": "HEAD"}), json!({"sha": "deadbeefdead", "path": "pom.xml"})] {
            assert!(c.call("wish/show", bad).is_err());
        }

        // A syntax error keeps the last good markers.
        c.notify("textDocument/didChange", json!({"textDocument": {"uri": app_uri, "version": 2}, "contentChanges": [{"text": APP.replace("run() {}", "run() {")}]}));
        let broken = c.ok("wish/fileState", json!({"textDocument": {"uri": app_uri}}));
        assert!(broken["blocks"].as_array().unwrap().iter().any(|b| b["item"] == "App.create"));
        assert!(c.changed > 0);

        stop(c, handle);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

