//! wishlang: write Rust the way you wish the API looked; Claude builds the rest.

mod claude;
mod commands;
mod daemon;
mod service;
mod dev;
mod doctor;
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
mod pseudo;
mod project;
mod session;
mod setup;
mod ticket;
mod update;

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use project::Project;

#[derive(Parser)]
#[command(name = "wishlang", version, about = "Create-from-usage for Rust and Java: you write the call sites, Claude writes the code underneath.", help_template = HELP)]
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
    /// Show a Java file as pseudocode, zoomable: each class as one line, then one line per method, then its steps.
    Pseudo {
        /// The Java file, or - for stdin.
        file: String,
        /// The fold ranges and texts as JSON (UTF-16 offsets), for editors.
        #[arg(long)]
        json: bool,
        /// How far to zoom in: 0 each class as one line, 1 one line per method, 2 each method's steps.
        #[arg(long, default_value_t = 2)]
        zoom: u8,
        /// One step per statement, without asking Claude.
        #[arg(long)]
        baseline: bool,
        /// Score every X.java with an X.golden.json in this directory (FILE) instead.
        #[arg(long, hide = true)]
        eval: bool,
        /// Fresh model runs per fixture for --eval.
        #[arg(long, hide = true, default_value_t = 1)]
        runs: usize,
    },
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
    /// Wire the session hooks and the explain permission into ~/.claude/settings.json.
    Setup {
        /// Remove wishlang's entries again.
        #[arg(long)]
        uninstall: bool,
        /// Apply without asking (required when stdin is not a terminal).
        #[arg(long, short)]
        yes: bool,
    },
    /// Check the tools, install and editor plugins wishlang depends on.
    Doctor {
        #[arg(long)]
        json: bool,
    },
    /// Update wishlang to the latest release.
    Update {
        /// Only report whether an update is available (exit 1 if so).
        #[arg(long)]
        check: bool,
        /// Print the result as JSON; implies --check.
        #[arg(long)]
        json: bool,
        /// Install this version instead of the latest.
        #[arg(long)]
        version: Option<String>,
    },
    /// Print a shell completion script.
    Completions { shell: clap_complete::Shell },
    /// Print the man page (roff).
    Man,
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

const HELP: &str = "\
{about}

{usage-heading} {usage}

Project:
  init         Add wish.toml to the current cargo or Maven project
  items        List items per file and their owner
  adopt        Adopt an item: protected from now on
  unadopt      Give an adopted item back to Claude
  check        Verify adopted items, dependency decisions and the build

Cycles:
  compile      Run one compile cycle, then launch the app
  implement    Turn the approved prototype into the real implementation
  undo         Roll Claude's changes back to the previous cycle
  dev          Dev loop: compile on save, live preview, decisions

Claude's proposals:
  suggestions  Show Claude's pending suggestions for your code
  accept       Apply a suggestion
  reject       Discard a suggestion
  decisions    List architectural and dependency decisions
  decide       Accept or reject a decision

Reading code:
  pseudo       Show a Java file as zoomable pseudocode: class, methods, steps

History and tickets:
  log          The journal: every save and every Claude cycle with its reasoning
  ticket       Show which generated code solves each problem of the ticket
  import       Rebuild this branch's journal from the commits since a revision
  share        Push this project's wish journals to a remote
  sync         Fetch others' wish journals from a remote

Editors and Claude Code:
  lsp          Language server for editors (Zed, IntelliJ)
  session      Claude Code hook that records ordinary Claude sessions
  explain      Explain a Claude Code turn to the session hook
  setup        Install the Claude Code hooks into ~/.claude/settings.json

Runtime:
  daemon       Manage the shared background daemon
  service      Start the daemon at login
  doctor       Check the tools, install and editor plugins
  update       Update wishlang to the latest release
  completions  Print a shell completion script
  man          Print the man page

Options:
{options}

Exit codes: 0 ok, 1 failure, 2 usage error. Run `wishlang help <command>` for details.
";

/// Long description and examples per command: `--help` shows them, a unit test requires them.
const DOCS: &[(&str, &str, &str)] = &[
    ("init", "Creates wish.toml in the current cargo or Maven project, a git journal and (for Rust) .zed/settings.json. Without --protect-existing, only src/main.rs (or the Java entry point) is protected.", "  wishlang init\n  wishlang init --protect-existing"),
    ("dev", "Terminal loop: compiles when you save, shows a live preview and lists Claude's decisions and suggestions.", "  wishlang dev"),
    ("session", "Hook for Claude Code (UserPromptSubmit and Stop). It records what an ordinary Claude session changed in the journal. Installed by `wishlang setup`; not meant to be run by hand.", "  wishlang setup"),
    ("pseudo", "Folds every method body of a Java file into pseudocode: one line per business step, outcomes indented below, --zoom 0 shows each class as one line, --zoom 1 one line per method. Claude only picks runs of whole statements; anything it gets wrong falls back to one step per statement. Answers are cached in ~/.wishlang/pseudo per file content. Works on any Java file, no wish.toml needed. WISH_PSEUDO_MODEL picks the model (default haiku).", "  wishlang pseudo src/main/java/demo/App.java\n  wishlang pseudo --zoom 1 App.java\n  git show main:App.java | wishlang pseudo -"),
    ("explain", "Reads {\"summary\", \"changes\"} JSON on stdin and keeps it for the Stop hook, which commits it as the turn's explanation. The hook tells Claude when to call it.", "  wishlang explain --session <id> < explanation.json"),
    ("import", "Rebuilds the branch's journal: the code at <since> is yours, every later commit that touches the project is Claude's, uncommitted changes are yours.", "  wishlang import main\n  wishlang import HEAD~5 --report report.json"),
    ("compile", "Runs one cycle: asks Claude to fill in what you wished for, verifies the result, records it in the journal, then launches the app. Exits 1 if the cycle fails.", "  wishlang compile\n  wishlang compile --force --note \"use tokio\"\n  wishlang compile --json"),
    ("adopt", "Marks items (functions, types, methods, variants) as yours: Claude may no longer change them. Names as shown by `wishlang items`.", "  wishlang adopt Mode\n  wishlang adopt Window::open"),
    ("unadopt", "Gives adopted items back to Claude.", "  wishlang unadopt Window::open"),
    ("items", "Lists every item per file with its owner (you, Claude or adopted).", "  wishlang items"),
    ("decisions", "Lists the architectural and dependency decisions Claude made, and those documented in doc comments.", "  wishlang decisions"),
    ("decide", "Accepts or rejects a decision from `wishlang decisions`. A rejection reason is passed to Claude on the next cycle.", "  wishlang decide 3 accept\n  wishlang decide 4 reject use the std HashMap instead"),
    ("suggestions", "Lists the changes to your own code that Claude proposes but never makes itself.", "  wishlang suggestions"),
    ("accept", "Applies a suggestion from `wishlang suggestions`.", "  wishlang accept 2"),
    ("reject", "Discards a suggestion.", "  wishlang reject 2"),
    ("undo", "Restores Claude's files to the previous cycle. Your own code is kept.", "  wishlang undo"),
    ("check", "Verifies that adopted items are intact, every dependency has a decision and the project builds. Exits 1 if anything fails.", "  wishlang check"),
    ("implement", "Turns the approved prototype into the real implementation, for the whole project or one file or item. Exits 1 if the cycle fails.", "  wishlang implement\n  wishlang implement src/engine.rs\n  wishlang implement --item Mode::Windowed --json"),
    ("log", "Shows the journal: every save and Claude cycle with its reasoning. It is also the git branch wish/<project>/log.", "  wishlang log\n  wishlang log --patch"),
    ("share", "Pushes all of the project's wish journal branches to a remote.", "  wishlang share\n  wishlang share upstream"),
    ("sync", "Fetches the wish journals that others shared.", "  wishlang sync"),
    ("ticket", "When the branch belongs to a ticket (feature/<KEY>-..., proto/<KEY>), shows each problem of the ticket with the files and items that serve it.", "  wishlang ticket\n  wishlang ticket --json"),
    ("lsp", "Language server that editors launch: blue markers for generated code, provenance hovers and compile on save. Speaks LSP on stdin and stdout.", "  wishlang lsp"),
    ("setup", "Merges wishlang's hooks (UserPromptSubmit and Stop run `wishlang session`) and the permission to run `wishlang explain` into ~/.claude/settings.json, replacing older wish entries and keeping everything else. Shows a diff first and keeps settings.json.bak.", "  wishlang setup\n  wishlang setup --yes\n  wishlang setup --uninstall"),
    ("doctor", "Checks claude, git and the build tool, which wishlang is first on PATH, the daemon, the Claude Code hooks, editor plugins and the current directory. Each check reports ok, warn or fail with a fix. Exits 1 if any check fails.", "  wishlang doctor\n  wishlang doctor --json"),
    ("update", "Downloads the latest release, verifies its SHA-256 checksum and replaces this binary, then restarts the daemon. Homebrew and cargo installs print their own upgrade command instead. WISHLANG_VERSION and WISHLANG_BASE_URL override the version and the download location.", "  wishlang update\n  wishlang update --check\n  wishlang update --version 0.2.0"),
    ("completions", "Prints a completion script for your shell to stdout.", "  wishlang completions zsh > ~/.zfunc/_wishlang\n  wishlang completions bash > /etc/bash_completion.d/wishlang"),
    ("daemon", "The per-user background process that serves every editor of every project: `wishlang lsp` starts it on demand and it exits after 30 idle minutes. State, socket and log live in ~/.wishlang (or $WISHLANG_HOME). Exits 1 from `status` when no daemon runs.", "  wishlang daemon status\n  wishlang daemon status --json\n  wishlang daemon stop"),
    ("service", "Installs the daemon as a login service (launchd on macOS, systemd --user on Linux) so it is always running and never exits when idle.", "  wishlang service install\n  wishlang service status\n  wishlang service uninstall"),
    ("man", "Prints the man page in roff format to stdout.", "  wishlang man > ~/.local/share/man/man1/wishlang.1"),
];

fn command() -> clap::Command {
    DOCS.iter().fold(Cli::command(), |c, (name, long, examples)| {
        c.mut_subcommand(*name, |s| s.long_about(*long).after_long_help(format!("Examples:\n{examples}")))
    })
}

fn main() {
    if let Err(e) = run() {
        eprintln!("wishlang: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let cli = Cli::from_arg_matches(&command().get_matches())?;
    let print = |r: anyhow::Result<String>| -> anyhow::Result<()> {
        println!("{}", r?);
        Ok(())
    };
    // Commands that write the journal or the project tell a running daemon, so open editors refresh.
    let writes = matches!(
        cli.cmd,
        Cmd::Import { .. } | Cmd::Compile { .. } | Cmd::Implement { .. } | Cmd::Adopt { .. } | Cmd::Unadopt { .. } | Cmd::Decide { .. } | Cmd::Accept { .. } | Cmd::Reject { .. } | Cmd::Undo | Cmd::Sync { .. }
    );
    let root = if writes { Project::discover().ok().map(|p| p.root) } else { None };
    let changed = || {
        if let Some(r) = &root {
            daemon::notify_changed(r);
        }
    };
    let result = match cli.cmd {
        Cmd::Hook => hook::run(),
        Cmd::Init { protect_existing } => print(commands::init(protect_existing)),
        Cmd::Dev => dev::run(Project::discover()?),
        Cmd::Session => session::hook(),
        Cmd::Pseudo { file, json, zoom, baseline, eval, runs } => {
            if eval { print(pseudo::eval(std::path::Path::new(&file), runs)) } else { print(pseudo::run(&file, json, zoom, baseline)) }
        }
        Cmd::Explain { session } => print(session::explain(&session)),
        Cmd::Import { since, report } => print(session::import(&Project::discover()?, &since, report.as_deref())),
        Cmd::Compile { json, force, no_run, note } => {
            let mut p = Project::discover()?;
            let mut sink: Box<dyn events::Sink> = if json { Box::new(events::Json) } else { Box::new(events::Human) };
            let ok = engine::compile(&mut p, sink.as_mut(), &engine::Options { force, note, mode: engine::Mode::Prototype })?;
            changed();
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
        Cmd::Setup { uninstall, yes } => print(setup::run(uninstall, yes)),
        Cmd::Doctor { json } => doctor::run(json),
        Cmd::Update { check, json, version } => update::run(check, json, version),
        Cmd::Completions { shell } => {
            clap_complete::generate(shell, &mut command(), "wishlang", &mut std::io::stdout());
            Ok(())
        }
        Cmd::Man => {
            clap_mangen::Man::new(command()).render(&mut std::io::stdout())?;
            Ok(())
        }
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
            let ok = engine::compile(&mut p, sink.as_mut(), &commands::implement_options(file, item))?;
            changed();
            if !ok {
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
    };
    changed();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_visible_command_is_in_the_root_help_and_has_examples() {
        let listed: Vec<&str> = HELP.lines().filter_map(|l| l.strip_prefix("  ")).filter_map(|l| l.split_whitespace().next()).collect();
        for sub in command().get_subcommands().filter(|s| !s.is_hide_set()) {
            let name = sub.get_name();
            assert!(listed.contains(&name), "`{name}` is missing from the root help template");
            assert!(sub.get_long_about().is_some(), "`{name}` has no long description");
            assert!(sub.get_after_long_help().is_some_and(|h| h.to_string().contains("wishlang")), "`{name}` has no examples");
        }
    }
}
