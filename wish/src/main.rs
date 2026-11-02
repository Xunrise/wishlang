//! wishlang: write Rust the way you wish the API looked; Claude builds the rest.

mod claude;
mod commands;
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
mod project;

use clap::{Parser, Subcommand};
use project::Project;

#[derive(Parser)]
#[command(name = "wish", about = "Create-from-usage for Rust: you write the call sites, Claude writes the vocabulary.")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Add wish.toml to the current cargo project (protects src/main.rs).
    Init,
    /// Dev loop: compile on save, live preview, decisions and suggestions.
    Dev,
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
    /// Language server for editors (Zed): blue markers, provenance hints, compile on save.
    Lsp,
    /// Claude Code PreToolUse hook (internal).
    #[command(hide = true)]
    Hook,
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
        Cmd::Init => print(commands::init()),
        Cmd::Dev => dev::run(Project::discover()?),
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
        Cmd::Lsp => lsp::run(),
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
