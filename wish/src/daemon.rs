//! `wishlang daemon`: one process per user that serves every editor of every project over a
//! Unix socket, so projects keep their index, caches, cycle queue and preview across editor
//! restarts and are shared by several editors.
//!
//! - `wishlang lsp` is a proxy between the editor's stdio and the daemon. It starts the daemon
//!   when none runs, and if the daemon goes away (crash, update) it reconnects and replays the
//!   editor's `initialize` and open documents, so the editor never notices.
//! - Hooks and CLI commands run in their own process and send the daemon a `changed` note
//!   after writing the journal, so open editors refresh. They never start a daemon.
//! - The state dir (`~/.wishlang`, or `$WISHLANG_HOME`) holds `daemon.sock`, `daemon.pid`
//!   (locked by the running daemon) and `daemon.log` (rotated at 5 MB).
//!
//! Wire format: the client sends one JSON line ([`Hello`]), the daemon answers one JSON line
//! ([`Reply`]); an `lsp` connection then carries LSP frames both ways.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Bumped when the wire format changes incompatibly.
pub const PROTO: u32 = 1;
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct Hello {
    pub proto: u32,
    pub version: String,
    /// "lsp" or "ctl".
    pub kind: String,
    /// For "ctl": "status", "stop" or "changed".
    #[serde(default)]
    pub cmd: Option<String>,
    /// For "changed": the project root.
    #[serde(default)]
    pub root: Option<PathBuf>,
    /// The client's process, for the log.
    #[serde(default)]
    pub pid: u32,
    /// The client's environment variables that the project's tools need.
    #[serde(default)]
    pub env: Vec<(String, String)>,
}

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct Reply {
    pub ok: bool,
    pub version: String,
    /// The client is newer: the daemon is handing over; start a new one.
    #[serde(default)]
    pub restart: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub status: Option<Value>,
}

/// Variables passed from client to daemon: what decides which tools a cycle finds and runs.
const ENV_KEYS: [&str; 6] = ["PATH", "WISH_CLAUDE", "WISH_MODEL", "CARGO_TARGET_DIR", "JAVA_HOME", "HOME"];

fn client_env() -> Vec<(String, String)> {
    ENV_KEYS.iter().filter_map(|k| Some((k.to_string(), std::env::var(k).ok()?))).collect()
}

/// `~/.wishlang`, or `$WISHLANG_HOME`.
pub fn home() -> PathBuf {
    match std::env::var_os("WISHLANG_HOME") {
        Some(h) => PathBuf::from(h),
        None => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".wishlang"),
    }
}

/// `a.b.c` as numbers, for "is the client newer"; pre-release suffixes are ignored.
fn version_key(v: &str) -> (u64, u64, u64) {
    let mut n = v.split(|c: char| !c.is_ascii_digit()).filter(|s| !s.is_empty()).map(|s| s.parse().unwrap_or(0));
    (n.next().unwrap_or(0), n.next().unwrap_or(0), n.next().unwrap_or(0))
}

/// The answer to a hello, before any per-kind work.
fn greet(h: &Hello) -> Reply {
    let mut r = Reply { ok: true, version: VERSION.into(), ..Default::default() };
    if h.proto != PROTO {
        r.ok = false;
        r.error = Some(format!("wishlang {} speaks daemon protocol {PROTO}, the client {} speaks {}: restart the editor", VERSION, h.version, h.proto));
    } else if h.kind == "lsp" && version_key(&h.version) > version_key(VERSION) {
        r.ok = false;
        r.restart = true;
    }
    r
}

/// Should an idle daemon exit now?
fn idle_due(connections: usize, busy: bool, quiet_for: std::time::Duration, idle: std::time::Duration) -> bool {
    connections == 0 && !busy && quiet_for >= idle
}

#[cfg(unix)]
pub use unix::{lsp, notify_changed, run, status, stop};

#[cfg(not(unix))]
mod fallback {
    use anyhow::{Result, bail};
    use std::path::Path;
    pub fn run(_idle_exit: bool) -> Result<()> {
        bail!("the daemon is not supported on Windows yet; editors and hooks run wishlang in-process")
    }
    pub fn lsp() -> Result<()> {
        crate::lsp::run()
    }
    pub fn status() -> Option<serde_json::Value> {
        None
    }
    pub fn stop() -> bool {
        false
    }
    pub fn notify_changed(_root: &Path) {}
}
#[cfg(not(unix))]
pub use fallback::{lsp, notify_changed, run, status, stop};

#[cfg(unix)]
mod unix {
    use super::*;
    use anyhow::{Context, Result, bail};
    use lsp_server::{ErrorCode, Message, Notification, Request, RequestId, Response};
    use std::collections::BTreeMap;
    use std::fs::{File, OpenOptions};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    const IDLE: Duration = Duration::from_secs(30 * 60);
    const LOG_MAX: u64 = 5 * 1024 * 1024;

    fn sock(home: &Path) -> PathBuf {
        home.join("daemon.sock")
    }

    // ---- daemon --------------------------------------------------------------

    /// A running daemon: what its connections and its timer share.
    struct Daemon {
        home: PathBuf,
        started: Instant,
        /// Live `lsp` connections.
        connections: AtomicUsize,
        /// Last connect or disconnect.
        touched: Mutex<Instant>,
        stopping: AtomicBool,
        /// The pid file and its lock; dropped on handover so a successor can start.
        lock: Mutex<Option<File>>,
        log: Mutex<Option<File>>,
    }

    impl Daemon {
        fn log(&self, msg: &str) {
            let mut g = self.log.lock().unwrap();
            let path = self.home.join("daemon.log");
            if g.as_ref().is_none_or(|f| f.metadata().map(|m| m.len() > LOG_MAX).unwrap_or(false)) {
                *g = open_log(&path);
            }
            if let Some(f) = g.as_mut() {
                let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
                let _ = writeln!(f, "{t} [{}] {msg}", std::process::id());
            }
        }

        fn touch(&self) {
            *self.touched.lock().unwrap() = Instant::now();
        }

        fn status(&self) -> Value {
            let projects: Vec<Value> = crate::lsp::projects().into_iter().map(|(root, clients, busy)| serde_json::json!({"root": root, "clients": clients, "busy": busy})).collect();
            serde_json::json!({"pid": std::process::id(), "version": VERSION, "uptime_s": self.started.elapsed().as_secs(), "connections": self.connections.load(Ordering::Relaxed), "projects": projects, "home": self.home})
        }

        /// Stop accepting and give up the socket and lock, so the next client starts a successor.
        /// Connections already open are served until [`run`] exits.
        fn hand_over(&self) {
            if self.stopping.swap(true, Ordering::SeqCst) {
                return;
            }
            self.log("handing over");
            // Wake the accept loop so it sees `stopping`, then free the socket and the lock.
            let _ = UnixStream::connect(sock(&self.home));
            let _ = std::fs::remove_file(sock(&self.home));
            self.lock.lock().unwrap().take();
        }
    }

    /// Opens the log, rotating it first when it is over the limit.
    fn open_log(path: &Path) -> Option<File> {
        if std::fs::metadata(path).is_ok_and(|m| m.len() > LOG_MAX) {
            let _ = std::fs::rename(path, path.with_extension("log.1"));
        }
        OpenOptions::new().create(true).append(true).mode(0o600).open(path).ok()
    }

    /// Bind the socket as the one daemon of `home`. None if another daemon holds the lock.
    fn start(home: &Path) -> Result<Option<(Arc<Daemon>, UnixListener)>> {
        std::fs::create_dir_all(home).with_context(|| format!("creating {}", home.display()))?;
        let _ = std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o700));
        let mut pid = OpenOptions::new().create(true).truncate(false).write(true).open(home.join("daemon.pid"))?;
        match pid.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
            Err(std::fs::TryLockError::Error(e)) => return Err(e).context("locking daemon.pid"),
        }
        pid.set_len(0)?;
        writeln!(pid, "{} {VERSION}", std::process::id())?;
        // We hold the lock, so a socket file left here belongs to a daemon that died.
        let path = sock(home);
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;
        let d = Arc::new(Daemon {
            home: home.to_path_buf(),
            started: Instant::now(),
            connections: AtomicUsize::new(0),
            touched: Mutex::new(Instant::now()),
            stopping: AtomicBool::new(false),
            lock: Mutex::new(Some(pid)),
            log: Mutex::new(open_log(&home.join("daemon.log"))),
        });
        Ok(Some((d, listener)))
    }

    /// Accept connections until handover.
    fn serve(d: &Arc<Daemon>, listener: UnixListener) {
        for s in listener.incoming() {
            if d.stopping.load(Ordering::SeqCst) {
                break;
            }
            let Ok(s) = s else { continue };
            let d = d.clone();
            std::thread::spawn(move || {
                if let Err(e) = connection(&d, s) {
                    d.log(&format!("connection: {e:#}"));
                }
            });
        }
    }

    fn connection(d: &Arc<Daemon>, s: UnixStream) -> Result<()> {
        let mut reader = BufReader::new(s.try_clone()?);
        let mut w = s;
        let mut line = String::new();
        reader.read_line(&mut line)?;
        if line.is_empty() {
            return Ok(()); // a wake-up connect
        }
        let hello: Hello = serde_json::from_str(&line).context("bad hello")?;
        let mut reply = greet(&hello);
        if reply.restart {
            d.log(&format!("client {} is newer than {VERSION}", hello.version));
            send(&mut w, &reply)?;
            d.hand_over();
            return Ok(());
        }
        if !reply.ok {
            return send(&mut w, &reply);
        }
        match (hello.kind.as_str(), hello.cmd.as_deref()) {
            ("lsp", _) => {
                send(&mut w, &reply)?;
                d.connections.fetch_add(1, Ordering::SeqCst);
                d.touch();
                d.log(&format!("editor connected (wishlang lsp pid {})", hello.pid));
                let r = serve_lsp(reader, w, hello.env);
                d.connections.fetch_sub(1, Ordering::SeqCst);
                d.touch();
                d.log(&format!("editor disconnected (pid {}){}", hello.pid, r.as_ref().err().map(|e| format!(": {e:#}")).unwrap_or_default()));
                r
            }
            ("ctl", Some("status")) => {
                reply.status = Some(d.status());
                send(&mut w, &reply)
            }
            ("ctl", Some("stop")) => {
                send(&mut w, &reply)?;
                d.hand_over();
                Ok(())
            }
            ("ctl", Some("changed")) => {
                if let Some(root) = &hello.root {
                    crate::lsp::project_changed(root);
                }
                send(&mut w, &reply)
            }
            (k, c) => {
                reply.ok = false;
                reply.error = Some(format!("unknown request {k} {c:?}"));
                send(&mut w, &reply)
            }
        }
    }

    fn send(w: &mut impl Write, v: &impl Serialize) -> Result<()> {
        let mut line = serde_json::to_string(v)?;
        line.push('\n');
        w.write_all(line.as_bytes())?;
        w.flush()?;
        Ok(())
    }

    /// One editor over the socket, served by the same code as an in-process `wishlang lsp`.
    fn serve_lsp(mut reader: BufReader<UnixStream>, mut w: UnixStream, env: Vec<(String, String)>) -> Result<()> {
        let (out_tx, out_rx) = crossbeam_channel::unbounded::<Message>();
        let (in_tx, in_rx) = crossbeam_channel::unbounded::<Message>();
        let shutdown = w.try_clone()?;
        std::thread::spawn(move || {
            for m in out_rx {
                if m.write(&mut w).is_err() {
                    break;
                }
            }
        });
        std::thread::spawn(move || {
            while let Ok(Some(m)) = Message::read(&mut reader) {
                if in_tx.send(m).is_err() {
                    break;
                }
            }
        });
        let r = crate::lsp::run_with(lsp_server::Connection { sender: out_tx, receiver: in_rx }, env);
        let _ = shutdown.shutdown(std::net::Shutdown::Both);
        r
    }

    /// `wishlang daemon`: serve until handover, or until idle for 30 minutes (unless `idle_exit` is off).
    pub fn run(idle_exit: bool) -> Result<()> {
        let home = home();
        let Some((d, listener)) = start(&home)? else {
            eprintln!("wishlang: a daemon is already running ({})", home.display());
            return Ok(());
        };
        let default_hook = std::panic::take_hook();
        let dl = d.clone();
        std::panic::set_hook(Box::new(move |info| {
            dl.log(&format!("panic: {info}"));
            default_hook(info);
        }));
        d.log(&format!("wishlang {VERSION} daemon started"));
        let dt = d.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(60));
                crate::lsp::evict_idle(IDLE);
                let busy = crate::lsp::projects().iter().any(|p| p.2);
                let quiet = dt.touched.lock().unwrap().elapsed();
                if idle_exit && idle_due(dt.connections.load(Ordering::SeqCst), busy, quiet, IDLE) {
                    dt.log("idle, exiting");
                    dt.hand_over();
                    std::process::exit(0);
                }
            }
        });
        serve(&d, listener);
        // Handed over: let running cycles finish (they are Claude runs), then go. The editors'
        // proxies reconnect to the successor.
        while crate::lsp::projects().iter().any(|p| p.2) {
            std::thread::sleep(Duration::from_millis(500));
        }
        d.log("exiting");
        std::process::exit(0)
    }

    // ---- clients ---------------------------------------------------------------

    /// Connect and say hello. Err when no daemon listens.
    fn hello(home: &Path, h: &Hello, timeout: Option<Duration>) -> Result<(BufReader<UnixStream>, UnixStream, Reply)> {
        let mut s = UnixStream::connect(sock(home))?;
        s.set_read_timeout(timeout)?;
        s.set_write_timeout(timeout)?;
        send(&mut s, h)?;
        let mut r = BufReader::new(s.try_clone()?);
        let mut line = String::new();
        r.read_line(&mut line)?;
        let reply: Reply = serde_json::from_str(&line).context("no answer from the daemon")?;
        // macOS refuses socket options (EINVAL) once the peer has closed, as after a ctl reply.
        let _ = s.set_read_timeout(None);
        let _ = s.set_write_timeout(None);
        Ok((r, s, reply))
    }

    fn ctl(cmd: &str, root: Option<&Path>, timeout: Duration) -> Option<Reply> {
        let h = Hello { proto: PROTO, version: VERSION.into(), kind: "ctl".into(), cmd: Some(cmd.into()), root: root.map(Path::to_path_buf), ..Default::default() };
        hello(&home(), &h, Some(timeout)).ok().map(|(_, _, r)| r)
    }

    /// The running daemon's status, or None.
    pub fn status() -> Option<Value> {
        ctl("status", None, Duration::from_secs(2))?.status
    }

    /// Ask the running daemon to hand over; true if one was running.
    pub fn stop() -> bool {
        ctl("stop", None, Duration::from_secs(2)).is_some()
    }

    /// Tell a running daemon that `root` changed on disk. Never starts one, never waits long.
    pub fn notify_changed(root: &Path) {
        if std::env::var_os("WISHLANG_NO_DAEMON").is_none() {
            let _ = ctl("changed", Some(root), Duration::from_millis(200));
        }
    }

    /// The binary to start a daemon from: this one, as it is on disk now (after an update,
    /// the new version).
    fn exe() -> Result<PathBuf> {
        let p = std::env::current_exe()?;
        // Linux names a replaced binary "<path> (deleted)"; the path itself has the new one.
        let s = p.to_string_lossy();
        Ok(match s.strip_suffix(" (deleted)") {
            Some(live) => PathBuf::from(live),
            None => p,
        })
    }

    fn spawn_daemon() -> Result<()> {
        use std::os::unix::process::CommandExt;
        std::process::Command::new(exe()?)
            .arg("daemon")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            // Its own process group: closing the editor (SIGHUP/SIGINT to its group) leaves it running.
            .process_group(0)
            .spawn()
            .context("starting the daemon")?;
        Ok(())
    }

    /// An `lsp` connection to the daemon, starting one if needed.
    fn connect_lsp(home: &Path) -> Result<(BufReader<UnixStream>, UnixStream)> {
        let h = Hello { proto: PROTO, version: VERSION.into(), kind: "lsp".into(), env: client_env(), pid: std::process::id(), ..Default::default() };
        let mut spawned: Option<Instant> = None;
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match hello(home, &h, Some(Duration::from_secs(5))) {
                Ok((r, w, reply)) if reply.ok => return Ok((r, w)),
                Ok((_, _, reply)) if !reply.restart => bail!("{}", reply.error.unwrap_or_default()),
                // An older daemon handing over: it removes its socket, then we start ours.
                Ok(_) => {}
                // None listening (or a dead one's socket). A daemon started while the old one
                // still held the lock exits at once, so try again after a moment.
                Err(_) => {
                    if spawned.is_none_or(|t| t.elapsed() > Duration::from_millis(500)) {
                        spawn_daemon()?;
                        spawned = Some(Instant::now());
                    }
                }
            }
            if Instant::now() > deadline {
                bail!("the daemon did not come up ({})", home.display());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    // ---- proxy -----------------------------------------------------------------

    /// What the proxy needs to bring a fresh daemon to where the editor thinks it is.
    #[derive(Default)]
    pub(super) struct Replay {
        init: Option<Request>,
        initialized: bool,
        /// uri -> didOpen params with the latest text and version.
        docs: BTreeMap<String, Value>,
        /// Editor requests the daemon hasn't answered.
        inflight: Vec<RequestId>,
        /// The replayed initialize, whose answer the editor must not see.
        swallow: Option<RequestId>,
    }

    impl Replay {
        /// Note a message from the editor.
        pub(super) fn note_editor(&mut self, m: &Message) {
            match m {
                Message::Request(r) => {
                    if r.method == "initialize" {
                        self.init = Some(r.clone());
                    }
                    self.inflight.push(r.id.clone());
                }
                Message::Notification(n) => {
                    let uri = n.params["textDocument"]["uri"].as_str().unwrap_or("").to_string();
                    match n.method.as_str() {
                        "initialized" => self.initialized = true,
                        "textDocument/didOpen" => {
                            self.docs.insert(uri, n.params.clone());
                        }
                        "textDocument/didChange" => {
                            let text = n.params["contentChanges"].as_array().and_then(|c| c.last()).and_then(|c| c["text"].as_str());
                            if let (Some(doc), Some(text)) = (self.docs.get_mut(&uri), text) {
                                doc["textDocument"]["text"] = text.into();
                                doc["textDocument"]["version"] = n.params["textDocument"]["version"].clone();
                            }
                        }
                        "textDocument/didClose" => {
                            self.docs.remove(&uri);
                        }
                        _ => {}
                    }
                }
                Message::Response(_) => {}
            }
        }

        /// Note a message from the daemon; false if the editor must not see it.
        pub(super) fn note_daemon(&mut self, m: &Message) -> bool {
            if let Message::Response(r) = m {
                if self.swallow.as_ref() == Some(&r.id) {
                    self.swallow = None;
                    return false;
                }
                self.inflight.retain(|id| *id != r.id);
            }
            true
        }

        /// The daemon went away: errors for the editor's open requests, and the messages that
        /// bring a new daemon up to date.
        pub(super) fn reconnect(&mut self) -> (Vec<Message>, Vec<Message>) {
            let errors = self
                .inflight
                .drain(..)
                .map(|id| Message::Response(Response::new_err(id, ErrorCode::ContentModified as i32, "wishlang restarted; try again".into())))
                .collect();
            let mut replay = Vec::new();
            if let Some(init) = &self.init {
                let id = RequestId::from("wishlang-replay".to_string());
                self.swallow = Some(id.clone());
                replay.push(Message::Request(Request::new(id, init.method.clone(), init.params.clone())));
            }
            if self.initialized {
                replay.push(Message::Notification(Notification::new("initialized".into(), serde_json::json!({}))));
            }
            for params in self.docs.values() {
                replay.push(Message::Notification(Notification::new("textDocument/didOpen".into(), params.clone())));
            }
            (errors, replay)
        }
    }

    /// `wishlang lsp`: the editor's stdio, bridged to the daemon. Falls back to serving in-process
    /// when no daemon can be had.
    pub fn lsp() -> Result<()> {
        if std::env::var_os("WISHLANG_NO_DAEMON").is_some() {
            return crate::lsp::run();
        }
        let home = home();
        let (reader, writer) = match connect_lsp(&home) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("wishlang: serving in-process: {e:#}");
                return crate::lsp::run();
            }
        };
        let replay = Arc::new(Mutex::new(Replay::default()));
        let daemon = Arc::new(Mutex::new(writer));

        // Editor -> daemon.
        {
            let replay = replay.clone();
            let daemon = daemon.clone();
            std::thread::spawn(move || {
                let mut stdin = std::io::stdin().lock();
                while let Ok(Some(m)) = Message::read(&mut stdin) {
                    replay.lock().unwrap().note_editor(&m);
                    let exit = matches!(&m, Message::Notification(n) if n.method == "exit");
                    // A failed write is fine: the reader reconnects and replays.
                    let _ = m.write(&mut *daemon.lock().unwrap());
                    if exit {
                        break;
                    }
                }
                let _ = daemon.lock().unwrap().shutdown(std::net::Shutdown::Both);
                std::process::exit(0);
            });
        }

        // Daemon -> editor, reconnecting when the daemon goes away.
        let mut reader = reader;
        let mut drops: Vec<Instant> = Vec::new();
        loop {
            while let Ok(Some(m)) = Message::read(&mut reader) {
                if replay.lock().unwrap().note_daemon(&m) {
                    let mut out = std::io::stdout().lock();
                    m.write(&mut out)?;
                }
            }
            drops.retain(|t| t.elapsed() < Duration::from_secs(60));
            drops.push(Instant::now());
            if drops.len() > 5 {
                bail!("the daemon keeps going away; see {}", home.join("daemon.log").display());
            }
            eprintln!("wishlang: the daemon went away; reconnecting");
            // Hold the writer while replaying, so the editor's next messages follow the replay.
            let mut w = daemon.lock().unwrap();
            let (r, new_w) = connect_lsp(&home)?;
            *w = new_w;
            let (errors, msgs) = replay.lock().unwrap().reconnect();
            for m in msgs {
                m.write(&mut *w)?;
            }
            drop(w);
            let mut out = std::io::stdout().lock();
            for e in errors {
                e.write(&mut out)?;
            }
            reader = r;
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn temp_home(name: &str) -> PathBuf {
            // Short: a socket path has to fit in ~100 bytes, and macOS's temp dir is long.
            let d = PathBuf::from(format!("/tmp/wishd-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            d
        }

        fn lsp_hello(version: &str) -> Hello {
            Hello { proto: PROTO, version: version.into(), kind: "lsp".into(), ..Default::default() }
        }

        #[test]
        fn one_daemon_per_home_and_stale_sockets_are_replaced() {
            let home = temp_home("single");
            std::fs::create_dir_all(&home).unwrap();
            std::fs::write(sock(&home), "left over by a crash").unwrap();
            let (d, l) = start(&home).unwrap().expect("first daemon starts");
            assert!(start(&home).unwrap().is_none(), "second daemon must not start");
            let dd = d.clone();
            let t = std::thread::spawn(move || serve(&dd, l));
            let (_, _, r) = hello(&home, &Hello { proto: PROTO, version: VERSION.into(), kind: "ctl".into(), cmd: Some("status".into()), ..Default::default() }, None).unwrap();
            assert_eq!(r.status.unwrap()["version"], VERSION);
            // Handover frees the lock and the socket for a successor.
            d.hand_over();
            t.join().unwrap();
            assert!(!sock(&home).exists());
            // A child forked elsewhere in this process may share the lock until it execs; retry briefly
            // (clients do the same when they spawn a successor).
            let started = (0..50).any(|_| start(&home).unwrap().is_some() || {
                std::thread::sleep(Duration::from_millis(20));
                false
            });
            assert!(started, "successor starts after handover");
        }

        #[test]
        fn a_newer_client_makes_the_daemon_hand_over_an_older_one_is_served() {
            let home = temp_home("versions");
            let (d, l) = start(&home).unwrap().unwrap();
            let dd = d.clone();
            let t = std::thread::spawn(move || serve(&dd, l));
            let (_, _, older) = hello(&home, &lsp_hello("0.0.1"), None).unwrap();
            assert!(older.ok && !older.restart);
            let (_, _, newer) = hello(&home, &lsp_hello("999.0.0"), None).unwrap();
            assert!(newer.restart);
            t.join().unwrap();
            assert!(d.stopping.load(Ordering::SeqCst));
        }

        #[test]
        fn editors_of_one_project_share_it_and_hear_about_changes() {
            let home = temp_home("share");
            let root = crate::lsp::tests::temp_project_named("daemon-share");
            let (d, l) = start(&home).unwrap().unwrap();
            std::thread::spawn(move || serve(&d, l));
            let open = |n: i32| {
                let (mut r, mut w) = connect_lsp(&home).unwrap();
                let init = Message::Request(Request::new(RequestId::from(n), "initialize".into(), serde_json::json!({"capabilities": {}, "rootUri": crate::lsp::path_to_uri(&root)})));
                init.write(&mut w).unwrap();
                loop {
                    if let Some(Message::Response(_)) = Message::read(&mut r).unwrap() {
                        break;
                    }
                }
                Message::Notification(Notification::new("initialized".into(), serde_json::json!({}))).write(&mut w).unwrap();
                (r, w)
            };
            let (mut ra, _wa) = open(1);
            let (_rb, _wb) = open(2);
            let clients = || crate::lsp::projects().into_iter().find(|p| p.0.ends_with(root.file_name().unwrap())).map_or(0, |p| p.1);
            let deadline = Instant::now() + Duration::from_secs(10);
            while clients() < 2 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            assert_eq!(clients(), 2, "both editors share one project");
            // A hook's note reaches the editors.
            ctl_at(&home, "changed", &root);
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                assert!(Instant::now() < deadline, "no wish/changed");
                if let Some(Message::Notification(n)) = Message::read(&mut ra).unwrap()
                    && n.method == "wish/changed"
                {
                    break;
                }
            }
        }

        fn ctl_at(home: &Path, cmd: &str, root: &Path) {
            let h = Hello { proto: PROTO, version: VERSION.into(), kind: "ctl".into(), cmd: Some(cmd.into()), root: Some(root.to_path_buf()), ..Default::default() };
            assert!(hello(home, &h, None).unwrap().2.ok);
        }

        #[test]
        fn the_proxy_replays_the_editor_state_to_a_new_daemon() {
            let mut r = Replay::default();
            let req = |id: i32, m: &str, p: Value| Message::Request(Request::new(RequestId::from(id), m.into(), p));
            let note = |m: &str, p: Value| Message::Notification(Notification::new(m.into(), p));
            r.note_editor(&req(1, "initialize", serde_json::json!({"rootUri": "file:///p"})));
            assert!(r.note_daemon(&Message::Response(Response::new_ok(RequestId::from(1), Value::Null))));
            r.note_editor(&note("initialized", serde_json::json!({})));
            r.note_editor(&note("textDocument/didOpen", serde_json::json!({"textDocument": {"uri": "file:///p/A.java", "languageId": "java", "version": 1, "text": "a"}})));
            r.note_editor(&note("textDocument/didOpen", serde_json::json!({"textDocument": {"uri": "file:///p/B.java", "languageId": "java", "version": 1, "text": "b"}})));
            r.note_editor(&note("textDocument/didChange", serde_json::json!({"textDocument": {"uri": "file:///p/A.java", "version": 7}, "contentChanges": [{"text": "a2"}]})));
            r.note_editor(&note("textDocument/didClose", serde_json::json!({"textDocument": {"uri": "file:///p/B.java"}})));
            r.note_editor(&req(2, "wish/fileState", serde_json::json!({})));

            let (errors, replay) = r.reconnect();
            assert!(matches!(&errors[..], [Message::Response(e)] if e.id == RequestId::from(2) && e.response_result.is_err()));
            let methods: Vec<String> = replay
                .iter()
                .map(|m| match m {
                    Message::Request(r) => r.method.clone(),
                    Message::Notification(n) => format!("{} {}", n.method, n.params["textDocument"]["text"].as_str().unwrap_or("")),
                    _ => unreachable!(),
                })
                .collect();
            assert_eq!(methods, ["initialize", "initialized ", "textDocument/didOpen a2"]);
            // The replayed initialize's answer is not the editor's business.
            let Message::Request(init) = &replay[0] else { unreachable!() };
            assert!(!r.note_daemon(&Message::Response(Response::new_ok(init.id.clone(), Value::Null))));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn only_newer_editors_restart_the_daemon() {
        let h = |v: &str, kind: &str| Hello { proto: PROTO, version: v.into(), kind: kind.into(), ..Default::default() };
        assert!(greet(&h("999.0.0", "lsp")).restart);
        assert!(!greet(&h("0.0.1", "lsp")).restart);
        assert!(!greet(&h(VERSION, "lsp")).restart);
        assert!(!greet(&h("999.0.0", "ctl")).restart, "hooks and the CLI never restart it");
        assert!(!greet(&Hello { proto: PROTO + 1, ..h(VERSION, "lsp") }).ok);
        assert!(version_key("0.10.0") > version_key("0.9.3"));
        assert_eq!(version_key("1.2.3-rc.1"), (1, 2, 3));
    }

    #[test]
    fn idle_exit_waits_for_editors_cycles_and_quiet() {
        let idle = Duration::from_secs(1800);
        assert!(idle_due(0, false, idle, idle));
        assert!(!idle_due(1, false, idle, idle));
        assert!(!idle_due(0, true, idle, idle));
        assert!(!idle_due(0, false, idle / 2, idle));
    }
}
