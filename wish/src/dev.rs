//! `wish dev`: the terminal dev loop. It runs next to any editor.
//!
//! Save your file (auto mode) or press `c`. Claude regenerates, the project
//! is verified and built, and the app relaunches as a live preview.

use crate::commands;
use crate::engine::{self, Options};
use crate::events::{Channel, Event, render};
use crate::project::Project;
use anyhow::Result;
use crossterm::event::{self, Event as TermEvent, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use notify::Watcher;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::process::Child;
use std::sync::mpsc::{self, Sender};
use std::time::{Duration, Instant};

enum Msg {
    Ev(Event),
    Built(bool),
    FileChanged,
}

fn out(line: &str) {
    let mut o = std::io::stdout();
    for l in line.lines() {
        let _ = write!(o, "\r\x1b[K{l}\r\n");
    }
    let _ = o.flush();
}

const HELP: &str = "\x1b[1mkeys\x1b[0m  c compile · f force (re-ask Claude) · w toggle compile-on-save · r restart preview\n      d decisions · s suggestions · i items · : command (adopt X, decide 1 accept, accept 2, note <text>…) · q quit";

pub fn run(project: Project) -> Result<()> {
    let root = project.root.clone();
    let (tx, rx) = mpsc::channel::<Msg>();

    let protected: Vec<_> = project.cfg.protected.files.iter().map(|f| root.join(f)).collect();
    let wtx = tx.clone();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(e) = res {
            if (e.kind.is_modify() || e.kind.is_create()) && e.paths.iter().any(|p| protected.iter().any(|q| p.ends_with(q.strip_prefix(&root).unwrap_or(q)))) {
                let _ = wtx.send(Msg::FileChanged);
            }
        }
    })?;
    watcher.watch(&project.root.join("src"), notify::RecursiveMode::Recursive)?;

    enable_raw_mode()?;
    let result = event_loop(project, tx, rx);
    let _ = disable_raw_mode();
    result
}

struct Dev {
    project: Project,
    tx: Sender<Msg>,
    busy: bool,
    auto: bool,
    preview: Option<Child>,
    last_seen: BTreeMap<String, String>,
    pending_change: Option<Instant>,
    cmdline: Option<String>,
}

fn event_loop(project: Project, tx: Sender<Msg>, rx: mpsc::Receiver<Msg>) -> Result<()> {
    let last_seen = project.protected_files();
    let mut d = Dev { project, tx, busy: false, auto: true, preview: None, last_seen, pending_change: None, cmdline: None };
    out(&format!("\x1b[1mwishlang dev\x1b[0m: {} · watching {}", d.project.root.display(), d.project.cfg.protected.files.join(", ")));
    out(HELP);
    d.compile(false, None);

    loop {
        while let Ok(m) = rx.try_recv() {
            match m {
                Msg::Ev(e) => render(&e).iter().for_each(|l| out(l)),
                Msg::FileChanged => {
                    let now = d.project.protected_files();
                    if now != d.last_seen && !d.busy {
                        d.pending_change = Some(Instant::now());
                    }
                }
                Msg::Built(ok) => {
                    d.busy = false;
                    d.reload();
                    d.last_seen = d.project.protected_files();
                    if ok {
                        d.restart_preview();
                    }
                    d.prompt();
                }
            }
        }
        if let Some(t) = d.pending_change {
            if t.elapsed() > Duration::from_millis(350) {
                d.pending_change = None;
                if d.auto {
                    out("\x1b[1m▸ saved\x1b[0m");
                    d.compile(false, None);
                } else {
                    out("\x1b[2m(file changed; press c to compile)\x1b[0m");
                }
            }
        }
        if let Some(c) = d.preview.as_mut() {
            if let Ok(Some(status)) = c.try_wait() {
                out(&format!("\x1b[2m[app exited: {status}]\x1b[0m"));
                d.preview = None;
            }
        }
        if event::poll(Duration::from_millis(80))? {
            if let TermEvent::Key(k) = event::read()? {
                if !d.key(k) {
                    break;
                }
            }
        }
    }
    if let Some(mut c) = d.preview.take() {
        let _ = c.kill();
    }
    Ok(())
}

impl Dev {
    fn reload(&mut self) {
        if let Ok(p) = Project::load(&self.project.root) {
            self.project = p;
        }
    }

    fn prompt(&self) {
        let mode = if self.auto { "compile-on-save" } else { "manual" };
        let mut o = std::io::stdout();
        let _ = write!(o, "\r\x1b[K\x1b[2m[{mode}] c/f/w/r/d/s/i/:/q ›\x1b[0m ");
        let _ = o.flush();
    }

    fn compile(&mut self, force: bool, note: Option<String>) {
        if self.busy {
            out("\x1b[2m(already compiling)\x1b[0m");
            return;
        }
        self.busy = true;
        let root = self.project.root.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let mut sink = Channel(mpsc::channel().0);
            let (etx, erx) = mpsc::channel::<Event>();
            sink.0 = etx;
            let fwd = tx.clone();
            let forward = std::thread::spawn(move || {
                for e in erx {
                    let _ = fwd.send(Msg::Ev(e));
                }
            });
            let ok = match Project::load(&root) {
                Ok(mut p) => {
                    let ok = engine::compile(&mut p, &mut sink, &Options { force, note }).unwrap_or_else(|e| {
                        let _ = sink.0.send(Event::Violation { msg: format!("{e:#}") });
                        false
                    });
                    if ok {
                        let _ = sink.0.send(Event::Status { msg: "building preview".into() });
                        let (built, errors) = engine::build(&p);
                        if !built {
                            let _ = sink.0.send(Event::Check { ok: false, errors });
                        }
                        built
                    } else {
                        false
                    }
                }
                Err(e) => {
                    let _ = sink.0.send(Event::Violation { msg: format!("{e:#}") });
                    false
                }
            };
            drop(sink);
            let _ = forward.join();
            let _ = tx.send(Msg::Built(ok));
        });
    }

    fn restart_preview(&mut self) {
        if let Some(mut c) = self.preview.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        match engine::spawn_preview(&self.project, true) {
            Ok(mut c) => {
                for stream in [c.stdout.take().map(|s| Box::new(s) as Box<dyn std::io::Read + Send>), c.stderr.take().map(|s| Box::new(s) as Box<dyn std::io::Read + Send>)].into_iter().flatten() {
                    let tx = self.tx.clone();
                    std::thread::spawn(move || {
                        for line in BufReader::new(stream).lines().map_while(Result::ok) {
                            let _ = tx.send(Msg::Ev(Event::App { line }));
                        }
                    });
                }
                out("\x1b[1m▸ preview running\x1b[0m");
                self.preview = Some(c);
            }
            Err(e) => out(&format!("could not start preview: {e}")),
        }
    }

    /// Returns false to quit.
    fn key(&mut self, k: KeyEvent) -> bool {
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            return false;
        }
        if let Some(line) = self.cmdline.as_mut() {
            match k.code {
                KeyCode::Enter => {
                    let line = self.cmdline.take().unwrap();
                    out("");
                    self.run_command(line.trim());
                    self.prompt();
                }
                KeyCode::Esc => {
                    self.cmdline = None;
                    out("");
                    self.prompt();
                }
                KeyCode::Backspace => {
                    line.pop();
                }
                KeyCode::Char(ch) => line.push(ch),
                _ => {}
            }
            if let Some(line) = &self.cmdline {
                let mut o = std::io::stdout();
                let _ = write!(o, "\r\x1b[K:{line}");
                let _ = o.flush();
            }
            return true;
        }
        match k.code {
            KeyCode::Char('q') => return false,
            KeyCode::Char('c') => self.compile(false, None),
            KeyCode::Char('f') => self.compile(true, None),
            KeyCode::Char('w') => {
                self.auto = !self.auto;
                out(if self.auto { "compile-on-save on" } else { "compile-on-save off" });
                self.prompt();
            }
            KeyCode::Char('r') => self.restart_preview(),
            KeyCode::Char('d') => self.show(commands::decisions(&self.project)),
            KeyCode::Char('s') => self.show(commands::suggestions(&self.project)),
            KeyCode::Char('i') => self.show(commands::items(&self.project)),
            KeyCode::Char('h') | KeyCode::Char('?') => {
                out(HELP);
                self.prompt();
            }
            KeyCode::Char(':') => {
                self.cmdline = Some(String::new());
                let mut o = std::io::stdout();
                let _ = write!(o, "\r\x1b[K:");
                let _ = o.flush();
            }
            _ => {}
        }
        true
    }

    fn show(&self, r: Result<String>) {
        match r {
            Ok(s) => out(&s),
            Err(e) => out(&format!("\x1b[31m{e:#}\x1b[0m")),
        }
        self.prompt();
    }

    fn run_command(&mut self, line: &str) {
        self.reload();
        let (cmd, rest) = line.split_once(' ').unwrap_or((line, ""));
        let rest = rest.trim();
        let num = || rest.split_whitespace().next().and_then(|n| n.parse::<u32>().ok());
        let r: Result<String> = match cmd {
            "adopt" => commands::adopt(&mut self.project, rest),
            "unadopt" => commands::unadopt(&mut self.project, rest),
            "accept" => num().map(|n| commands::accept(&mut self.project, n)).unwrap_or_else(|| Err(anyhow::anyhow!("usage: accept <id>"))),
            "reject" => num().map(|n| commands::reject(&self.project, n)).unwrap_or_else(|| Err(anyhow::anyhow!("usage: reject <id>"))),
            "decide" => {
                let mut it = rest.splitn(3, ' ');
                match (it.next().and_then(|n| n.parse().ok()), it.next(), it.next()) {
                    (Some(id), Some("accept"), _) => commands::decide(&self.project, id, true, None),
                    (Some(id), Some("reject"), reason) => commands::decide(&self.project, id, false, reason.map(String::from)),
                    _ => Err(anyhow::anyhow!("usage: decide <id> accept | decide <id> reject <reason>")),
                }
            }
            "note" => {
                self.compile(true, Some(rest.to_string()));
                return;
            }
            "check" => Ok(commands::check(&self.project).1),
            "" => return,
            other => Err(anyhow::anyhow!("unknown command `{other}` (adopt, unadopt, accept, reject, decide, note, check)")),
        };
        self.show(r);
    }
}
