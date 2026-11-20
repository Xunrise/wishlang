//! wishlang: write Rust the way you wish the API looked; Claude builds the rest.

mod claude;
mod commands;
mod daemon;
mod service;
mod dev;
mod engine;
mod events;
mod git;
mod hook;
mod items;
mod java;
mod journal;
mod lsp;
mod pending;
mod preview;
mod provenance;
mod session;
mod project;
mod ticket;

use clap::{Parser, Subcommand};
use project::Project;

#[derive(Parser)]
#[command(name = "wishlang", version, about = "Create-from-usage for Rust and Java: you write the call sites, Claude writes the code underneath.")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Add wish.toml to the current cargo or Maven project (protects src/main.rs, or the Java entry point).
    Init {
        /// Protect every source file that exists now: Claude may only add imports to them and
        /// writes what you wish into new files. For adopting wish in an existing codebase.
        #[arg(long)]
        protect_existing: bool,
    },
    /// Dev loop: compile on save, live preview, decisions and suggestions.
    Dev,
    /// Claude Code hook (UserPromptSubmit and Stop) that records ordinary Claude sessions in the journal.
    Session,
    /// Explain a Claude Code turn to the session hook (JSON on stdin; asked for by the hook).
    Explain {
        #[arg(long)]
        session: String,
    },
    /// Rebuild this branch's journal from the commits since <since>, recording them as Claude's.
    Import {
        since: String,
        /// Per-commit reasons and ticket problems: {"<sha>": {"summary", "changes": [{"item", "why", "problems"}], "problems"}}.
        #[arg(long)]
        report: Option<std::path::PathBuf>,
    },
    /// Run one compile cycle, then launch the app.
    Compile {
        /// Emit NDJSON events (for editor plugins); implies --no-run.
        #[arg(long)]
        json: bool,
        /// Ask Claude even if the code compiles and nothing changed.
        #[arg(long)]
        force: bool,
        /// Don't launch the app afterwards.
        #[arg(long)]
        no_run: bool,
        /// One-off instruction for Claude this cycle.
        #[arg(long)]
        note: Option<String>,
    },
    /// Adopt an item (e.g. `Mode`, `Window::open`): protected from now on.
    Adopt { item: Vec<String> },
    /// Give an adopted item back to Claude.
    Unadopt { item: Vec<String> },
    /// List items per file and their owner.
    Items,
    /// List architectural/dependency decisions and doc-comment decisions.
    Decisions,
    /// Accept or reject an architectural/dependency decision.
    Decide {
        id: u32,
        #[arg(value_parser = ["accept", "reject"])]
        verdict: String,
        /// Why (for rejections; Claude reads this).
        reason: Vec<String>,
    },
    /// Show Claude's pending suggestions for your code.
    Suggestions,
    /// Apply a suggestion.
    Accept { id: u32 },
    /// Discard a suggestion.
    Reject { id: u32 },
    /// Roll Claude's changes back to the previous cycle (your code is kept).
    Undo,
    /// Verify adopted items, dependency decisions and the build.
    Check,
    /// Turn the approved prototype into the real implementation (whole project, or one file).
    Implement {
        file: Option<String>,
        /// Only this item, e.g. `Mode::Windowed` or `OpenWindow::background`.
        #[arg(long)]
        item: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// The journal: every save and every Claude cycle with its reasoning (also on branch wish/<project>/log).
    Log {
        /// Show full diffs, not just file stats.
        #[arg(short, long)]
        patch: bool,
    },
    /// Push this project's wish journals (all branches) to a remote.
    Share {
        #[arg(default_value = "origin")]
        remote: String,
    },
    /// Fetch others' wish journals from a remote.
    Sync {
        #[arg(default_value = "origin")]
        remote: String,
    },
    /// Show which generated code solves each problem of this branch's ticket.
    Ticket {
        /// Print the full trace as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Language server for editors (Zed): blue markers, provenance hints, compile on save.
    Lsp,
    /// Keep the daemon running as a login service.
    Service {
        #[command(subcommand)]
        cmd: ServiceCmd,
    },
    /// The per-user daemon that serves editors (started automatically by `wishlang lsp`).
    Daemon {
        #[command(subcommand)]
        cmd: Option<DaemonCmd>,
        /// Keep running when idle (for `wishlang service`).
        #[arg(long)]
        no_idle_exit: bool,
    },
    /// Claude Code PreToolUse hook (internal).
    #[command(hide = true)]
    Hook,
}

#[derive(Subcommand)]
enum ServiceCmd {
    /// Run the daemon from login on (launchd on macOS, systemd --user on Linux).
    Install,
    /// Remove the service; editors start the daemon on demand again.
    Uninstall,
    /// Whether the service is installed and which binary it runs.
    Status {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum DaemonCmd {
    /// Show whether the daemon runs, its version and the projects it serves.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Stop the daemon after its running cycles finish; editors reconnect to a new one.
    Stop,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("wish: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let print = |r: anyhow::Result<String>| -> anyhow::Result<()> {
        println!("{}", r?);
        Ok(())
    };
    match cli.cmd {
        Cmd::Hook => hook::run(),
        Cmd::Init { protect_existing } => print(commands::init(protect_existing)),
        Cmd::Dev => dev::run(Project::discover()?),
        Cmd::Session => session::hook(),
        Cmd::Explain { session } => print(session::explain(&session)),
        Cmd::Import { since, report } => print(session::import(&Project::discover()?, &since, report.as_deref())),
        Cmd::Compile { json, force, no_run, note } => {
            let mut p = Project::discover()?;
            let mut sink: Box<dyn events::Sink> = if json { Box::new(events::Json) } else { Box::new(events::Human) };
            let ok = engine::compile(&mut p, sink.as_mut(), &engine::Options { force, note, mode: engine::Mode::Prototype })?;
            if !ok {
                std::process::exit(1);
            }
            if !(json || no_run) {
                println!("\x1b[1m▸ launching\x1b[0m");
                engine::spawn_preview(&p, false)?.wait()?;
            }
            Ok(())
        }
        Cmd::Adopt { item } => print(commands::adopt(&mut Project::discover()?, &item.join(" "))),
        Cmd::Unadopt { item } => print(commands::unadopt(&mut Project::discover()?, &item.join(" "))),
        Cmd::Items => print(commands::items(&Project::discover()?)),
        Cmd::Decisions => print(commands::decisions(&Project::discover()?)),
        Cmd::Decide { id, verdict, reason } => {
            let reason = (!reason.is_empty()).then(|| reason.join(" "));
            print(commands::decide(&Project::discover()?, id, verdict == "accept", reason))
        }
        Cmd::Suggestions => print(commands::suggestions(&Project::discover()?)),
        Cmd::Accept { id } => print(commands::accept(&mut Project::discover()?, id)),
        Cmd::Reject { id } => print(commands::reject(&Project::discover()?, id)),
        Cmd::Ticket { json } => print(commands::ticket(&Project::discover()?, json)),
        Cmd::Lsp => daemon::lsp(),
        Cmd::Daemon { cmd: None, no_idle_exit } => daemon::run(!no_idle_exit),
        Cmd::Daemon { cmd: Some(DaemonCmd::Status { json }), .. } => {
            let st = daemon::status();
            if json {
                println!("{}", st.clone().unwrap_or(serde_json::json!({"running": false})));
            } else {
                match &st {
                    Some(s) => {
                        println!("wishlang daemon {} (pid {}, up {}s, {} editor connection(s))", s["version"].as_str().unwrap_or("?"), s["pid"], s["uptime_s"], s["connections"]);
                        for p in s["projects"].as_array().into_iter().flatten() {
                            println!("  {}  {} editor(s){}", p["root"].as_str().unwrap_or("?"), p["clients"], if p["busy"] == true { ", cycle running" } else { "" });
                        }
                    }
                    None => println!("no daemon running ({})", daemon::home().display()),
                }
            }
            if st.is_none() {
                std::process::exit(1);
            }
            Ok(())
        }
        Cmd::Service { cmd: ServiceCmd::Install } => print(service::install()),
        Cmd::Service { cmd: ServiceCmd::Uninstall } => print(service::uninstall()),
        Cmd::Service { cmd: ServiceCmd::Status { json } } => {
            let s = service::status()?;
            if json {
                println!("{s}");
            } else {
                println!("service: {}", if s["installed"] == true { format!("installed ({})", s["unit"].as_str().unwrap_or("")) } else { "not installed".into() });
                if s["installed"] == true && s["runs_this_binary"] != true {
                    println!("  note: the unit runs another binary; run `wishlang service install` again");
                }
                println!("daemon: {}", if s["daemon"].is_null() { "not running" } else { "running" });
            }
            Ok(())
        }
        Cmd::Daemon { cmd: Some(DaemonCmd::Stop), .. } => {
            println!("{}", if daemon::stop() { "daemon stopping (after running cycles finish)" } else { "no daemon running" });
            Ok(())
        }
        Cmd::Share { remote } => print(commands::share(&Project::discover()?, &remote)),
        Cmd::Sync { remote } => print(commands::sync(&Project::discover()?, &remote)),
        Cmd::Log { patch } => {
            let p = Project::discover()?;
            let g = git::Git::open(&p.root)?;
            let Some(j) = g.journal() else { anyhow::bail!("no journal yet: save once in Zed or run `wish compile`") };
            let mut c = std::process::Command::new("git");
            c.current_dir(&p.root).args(["log", "--first-parent", "--color=auto", "--format=%C(yellow)%h%Creset %C(bold)%an%Creset %C(dim)%ar%Creset%n%B", "--relative"]);
            c.arg(if patch { "-p" } else { "--stat" }).arg(&j).arg("--").arg(".");
            c.status()?;
            Ok(())
        }
        Cmd::Implement { file, item, json } => {
            let mut p = Project::discover()?;
            let mut sink: Box<dyn events::Sink> = if json { Box::new(events::Json) } else { Box::new(events::Human) };
            if !engine::compile(&mut p, sink.as_mut(), &commands::implement_options(file, item))? {
                std::process::exit(1);
            }
            Ok(())
        }
        Cmd::Undo => print(commands::undo(&Project::discover()?)),
        Cmd::Check => {
            let (ok, report) = commands::check(&Project::discover()?);
            println!("{report}");
            if !ok {
                std::process::exit(1);
            }
            Ok(())
        }
    }
}
