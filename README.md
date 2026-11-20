# wishlang

[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#license)
[![Release](https://img.shields.io/github/v/release/Xunrise/wishlang)](https://github.com/Xunrise/wishlang/releases/latest)
[![JetBrains Marketplace](https://img.shields.io/badge/IntelliJ-Marketplace-orange)](https://plugins.jetbrains.com/search?search=wishlang)
[![Zed extension](https://img.shields.io/badge/Zed-extension-blue)](https://zed.dev/extensions?query=wishlang)

**Write the call you wish existed.** [xunrise.github.io/wishlang](https://xunrise.github.io/wishlang/)

Create-from-usage for Rust and Java (Maven), in Zed and IntelliJ. You write the call sites you wish existed; Claude writes the vocabulary underneath, first as a skimmable **prototype**, then, once you're happy, as the real **implementation**. The app restarts on every save so you see each change.

```rust
fn main() {
    App::new("Demo").window().open(Mode::Windowed(800, 600)).background(Gradient::Sunset).run()
}
```

The project stays normal, clean Rust (or Java). The only extra files are `wish.toml`, `DECISIONS.md`, `.wish/` and, for Zed, `.zed/settings.json`.

## Install

```sh
cargo install --path wish        # puts `wishlang` on PATH (Zed looks for it there, or in $WISH_BIN)
```

Then, in Zed, open the command palette, run **zed: install dev extension**, and pick the `zed/` folder of this repo.

The colors come from three semantic token rules in your Zed **user** settings (Zed only reads `global_lsp_settings` there):

```json
"global_lsp_settings": {
  "semantic_token_rules": [
    { "token_type": "generated", "token_modifiers": ["untraced"], "background_color": "#dc26262e" },
    { "token_type": "generated", "token_modifiers": ["prototype"], "background_color": "#3b82f633", "font_style": "italic" },
    { "token_type": "generated", "background_color": "#3b82f61f" }
  ]
}
```

Zed applies the first rule that matches, so the more specific ones come first (untraced beats prototype). They only match the token type `generated`, which only `wishlang lsp` emits. The project's `.zed/settings.json` turns on `"semantic_tokens": "combined"` for Rust. After changing settings, run `editor: restart language server` once.

Requires Claude Code (`claude`) on PATH and a git repository (`wishlang init` creates one). Set `WISH_MODEL=sonnet` for faster cycles.

## Java

A project with a `pom.xml` is a Java project; otherwise `Cargo.toml` makes it a Rust one. The same loop applies, with these differences:

- **Check:** `mvn -q compile` instead of `cargo check`.
- **Run:** the app is started as `java -cp target/classes <main class>`. The class is `[java] main_class` in `wish.toml`, else `<mainClass>` in `pom.xml`, else `Main`.
- **Prototypes:** a Javadoc `<h2>Mocked</h2>` section marks an item as mocked (the counterpart of `# Mocked`).
- **Protected files** (`[protected]` in `wish.toml`) may only gain `import` lines.
- **Multi-module builds:** a root `pom.xml` with `<modules>` (nested ones too) is one wish project. Sources come from every module's `src/main/java`, Claude may edit every module's `pom.xml`, and `mvn -q compile` runs at the root. `wishlang init` sets `[java] preview = false` there, because `java -cp target/classes` can't start a reactor module. Set `preview = false` yourself to stop any app from being run after cycles.
- **Existing codebases:** `wishlang init --protect-existing` protects every source file that exists, so Claude writes what you wish into new files and only adds imports to yours. Remove a file from `[protected]` to let Claude change it. Protection binds only wish's cycles; ordinary Claude Code sessions edit any file.
- **Large codebases:** above 150 kB of protected code, a cycle's prompt shows only the protected files you changed, and Claude reads the rest itself (Glob, Grep, Read).
- `examples/demo-java` is the Java demo.

Java editing is supported in IntelliJ only. In Zed, jdtls reports unresolved-symbol errors for code that doesn't exist yet and they can't be filtered.

## IntelliJ

Install **wishlang** from the JetBrains Marketplace (**Settings › Plugins › Marketplace**). If `wishlang` is not installed, the plugin says so on project open and offers to download the latest release for your platform into `~/.local/bin` (macOS and Linux).

To build it yourself:

```sh
cd intellij && ./gradlew buildPlugin     # add -PlocalIde="/Applications/IntelliJ IDEA.app" to build against an installed IDE
```

Install `intellij/build/distributions/*.zip` via **Settings › Plugins › ⚙ › Install Plugin from Disk**. To try it in a sandbox IDE instead: `./gradlew runIde -PdemoProject=/path/to/project`. `./gradlew verifyPlugin` runs the Plugin Verifier against the IntelliJ IDEA releases since `sinceBuild`.

Publishing: `./gradlew publishPlugin` signs and uploads the plugin. It reads the signing certificate chain, private key and its password from `CERTIFICATE_CHAIN`, `PRIVATE_KEY` and `PRIVATE_KEY_PASSWORD`, and the Marketplace token from `PUBLISH_TOKEN`. Keep them out of the repository.

The plugin runs `wishlang lsp` from `$WISH_BIN`, else the first of `~/.local/bin`, `/opt/homebrew/bin`, `/usr/local/bin`, `~/.cargo/bin` and (Windows) `%LOCALAPPDATA%\wishlang\bin` that has `wishlang`. It never runs a `wish` from PATH, because `/usr/bin/wish` on macOS is Tk.

**Windows IDE, project in WSL** (opened as `\\wsl.localhost\<distro>\…`): the plugin starts `wishlang lsp` inside that distro through `wsl.exe`, in an interactive login bash so `.bashrc`'s PATH (Java, Maven, `claude`) applies, and maps the IDE's UNC paths to Linux paths. Install wish in WSL (`cargo install --path wish`). `$WISH_BIN` is then read on the Linux side. From WSL, `intellij/deploy.sh` builds the plugin and publishes it to a custom plugin repository on the Windows side (`C:\Users\<you>\hexagonal-inspector-repo\updatePlugins.xml` by default, `WISH_DEPLOY_DIR` to override). After each deploy, update it under **Settings › Plugins › Updates**.

| You see | Meaning |
|---|---|
| bar in the gutter + faint tint | generated by Claude (stronger while still mocked) |
| **red** bar | generated in a ticket cycle but serves no problem of the ticket |
| **hover** | who wrote it and, for generated code, why and which edit triggered it |
| warning on a call site | `will be generated` on save |
| no red `Cannot resolve symbol` | IntelliJ's unresolved error is hidden for wished symbols |
| lens actions in the gutter | implement, accept/reject suggestion, open problems (`◆ P1 · P2`) |

The **Wish** tool window has two tabs:

- **Log:** the journal as a table (ticket key, author, date), with the selected entry's files and diffs and Claude's reasoning. For ticket entries, **Group by Problem** (on by default) regroups files as problem, file, item. Pending decisions get Accept and Reject buttons.
- **Ticket:** the branch's ticket as one tree: ticket → problems (plus an **Untraced** group) → files → items, with details for the selection. The picker at the left of its toolbar switches to any other ticket branch that has a journal (journals are shared by all worktrees of a repository): its files open from the worktree that has the branch checked out, or read-only from its journal when none does. Double-click opens the file (an item at its line); the toolbar has **Open File** and **Show Diff** (the ticket's diff of that file), toggles the changes browser over the ticket's whole diff, and adds, edits and removes problems.

The tool window's title bar has **Fulfil Wishes** (run a cycle for your edits now) and **Build on Save** (on by default, per project): turn it off to save freely and trigger cycles only with Fulfil Wishes.

## Ticket tracing

When the branch belongs to a ticket, wish traces generated code back to the ticket's problems. It works with protoflow notes:

- **Ticket key:** branch `feature|fix|chore/<KEY>-…` or `proto/<KEY>`.
- **Notes:** `<main checkout>/.work/<KEY>/` (`intent.md`, `spec.md`).
- **Problems:** the first cycle extracts them into `.work/<KEY>/problems.json` (P1, P2, …). Edit the file (or use the IntelliJ Ticket tab) to correct them.
- **Journal:** every item line is `- Item [P1,P3]: why`, and the commit has a `Ticket: <KEY>` trailer.
- **Untraced:** generated in a ticket cycle but serving no problem.

`wishlang ticket [--json]` prints problems with their files and items.

### Code Claude writes outside wish

An ordinary Claude Code session ("change X") is recorded too, once `wishlang session` is a hook in your **user** settings (`~/.claude/settings.json`, so it covers every checkout and worktree):

```json
"hooks": {
  "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "~/.cargo/bin/wishlang session"}]}],
  "Stop": [{"hooks": [{"type": "command", "command": "~/.cargo/bin/wishlang session"}]}]
},
"permissions": {"allow": ["Bash(~/.cargo/bin/wishlang explain:*)"]}
```

(Write the full path of `wish`.) When you send a prompt, what is on disk is recorded as yours. When Claude's turn ends, everything that changed during the turn is Claude's, however it was written (Edit, Write or a shell command). If the turn changed items, the hook first sends Claude back once to explain them, with the ticket's problems: Claude pipes `{"summary", "changes": [{"item", "why", "problems"}]}` into `wishlang explain`. The result is a `session` entry in the journal, like a cycle: blue code, hovers with the reason, `Triggered by` your prompt, and the ticket tab. The hook does nothing without a `wish.toml` in the session's directory, a parent, or a direct subdirectory, and nothing inside wish's own cycles. While one session's turn is running, a prompt in another session on the same project doesn't record the disk as yours: the running session's Stop records it as Claude's.

For code written before that, `wishlang import <since>` rebuilds the branch's journal: the code at `<since>` is yours, every commit since that touches the project is Claude's (one `imported` entry each, explained by its commit message), uncommitted changes are yours. `--report file.json` adds per-item reasons and problem tags: `{"<sha>": {"changes": [{"item", "why", "problems"}], "problems": [...]}}` (`problems` creates `problems.json` if it doesn't exist). In Zed, hovering an item says "Solves P#", the `◆ P1 · P2` lens opens `problems.json`, untraced code gets the red rule from the settings above, untraced warnings show in project diagnostics, and the task `wish: ticket` lists the tracing in the terminal.

## Working in Zed

Install **wishlang** from Zed's extensions (**zed: extensions**). The extension runs `$WISH_BIN`, else `wishlang` from PATH, else downloads the latest release for your platform into its own folder and keeps it updated. To work on the extension, run **zed: install dev extension** and pick this repo's `zed/` folder.

Open `examples/demo` (or any project after `wishlang init`) in Zed.

| You see | Meaning |
|---|---|
| green/yellow/red in the gutter | git changes (Zed native) |
| red squiggles | real syntax/type problems (rust-analyzer) |
| **blue** `✦ will be generated: Mode::Windowed` | you used something that doesn't exist yet; it's generated on save |
| **blue** `◇ prototype …` | this item's behaviour is still mocked (its doc has a `# Mocked` section) |
| **blue background** | Claude generated this code, persistently, every time you open the project |
| **stronger blue, italic** | generated *and* still mocked (prototype) |
| **red background** | generated in a ticket cycle but serves none of the ticket's problems (also a warning `serves no problem of KEY`) |
| `◆ P1 · P3` above an item | the ticket problems this item serves; click to open `problems.json` at the first one |
| no background, `✎` at line start | you wrote it (always visible in files that also contain generated code) |
| `◇ prototype · mocked: …` after a signature | that item is still mocked |
| `▶ Implement …` above an item | click to make just that item real (or all items in the file, from the top-of-file lens) |
| `✓ Accept suggestion …  ✗ Reject` above code | Claude proposes changing your code; one click applies it and regenerates |
| **hover** any line | git-blame style: who wrote it and when; for generated code, **why** (Claude's reason for that item) and **which of your edits triggered it** |

- **Save** to start a cycle. Progress shows in the status bar, the prototype updates, and the app restarts.
- **Code actions** (`ctrl-.`):
  - adopt or unadopt the item under the cursor
  - accept or reject a suggestion
  - *implement prototype in this file*, or *implement the whole prototype*
  - *regenerate now*, *undo last cycle*
  - *next generated block ↓* / *next block written by you ↓*
- **New decisions** (for example a new crate) pop up with Accept / Reject buttons.

The terminal alternative is `wishlang dev`, which offers the same loop, with keys.

### Panels (reusing Zed's built-in views)

- **What wish did:** the journal is mirrored to a local branch, `wish/<project>/log` (`wish/examples/demo/log` for the demo). Open **git graph** in Zed and select it: every save and cycle, Claude's "Why" per item, changed files and diffs. lazygit works too. In the terminal (or the Zed task `wish: log`), use `wishlang log` (`--patch` for diffs).
- **All suggestions:** pending suggestions are published as warnings for every file, so Zed's **project diagnostics** (`ctrl-shift-m`) lists them all. Accept or reject them with the lens or `ctrl-.`. The Zed task `wish: suggestions` lists them in the terminal.

### Your lines inside generated files

They are wishes. Claude keeps them and makes them true. Formatting may change, and an empty body you write is filled in: write `pub enum Color {}` and the declaration line stays **yours** (in blame and color) while the variants Claude adds are generated.

## Two phases

**Prototype** (every save). Claude writes header-like files you can skim in seconds:
- public types, signatures and doc comments carry the meaning;
- bodies are a few well-named steps;
- details are mocked with the simplest thing that makes your change visible, and each mocked item says so in a `# Mocked` doc section. That section is the single source of truth for "prototype".

You iterate by editing your lines. You can also edit **inside generated files**: a line you add there (e.g. `/// The title shows the current mode`) is a wish too. Claude keeps it verbatim and makes it true. Whatever you don't specify, Claude infers from context.

**Implement** (`wishlang implement [file]`, the code action, or `I` in `wishlang dev`). Claude turns the prototype into the real thing, in place:
- Public signatures and type definitions must stay identical (verified; any change reverts the cycle).
- Public bodies stay at intent level. Details go into private helpers or submodules (e.g. `src/window/paint.rs`), so each file still reads top-down at one flight level.
- Claude gets the full **history** of how the prototype came about: every save, every cycle, and what you undid or abandoned.
- The prototype is frozen at `refs/wish/<project>/prototype`.

## Provenance and protection

Every line is either yours or generated, and both can live in one file. Provenance comes from git blame on the journal (below), so it needs no state of its own, persists across sessions, and covers unsaved buffers: lines you're typing are yours. `wishlang lsp` turns it into semantic tokens (blue) and hovers.

| What | Protection |
|---|---|
| your protected files (`[protected]` in `wish.toml`, default `src/main.rs`) | Claude may only *add* single-line `mod`/`use` wiring |
| your lines inside generated files | must survive every cycle verbatim; Claude restructures around them |
| adopted items (`wishlang adopt Mode`) | hashed; any code or doc change is refused |
| `wish.toml`, `DECISIONS.md`, `.wish/` | tool-owned; Claude can't touch them |

Protection is enforced twice:
1. A Claude Code PreToolUse hook (`wish hook`) refuses tool calls that would break a rule, and tells Claude to file a suggestion instead.
2. Every cycle verifies afterwards, and reverts the whole cycle on a violation.

## Git is the source of truth

Snapshots are commits built with a temporary index; your branch, index and HEAD are never touched. wishlang never commits to your branch. Per project:

wish's history has the same shape as yours: **one journal per code branch**, merged when your branches merge.

- `refs/wish/<project>/journal/<branch>`: every save (authored **you**) and every Claude cycle (authored **claude**), each with a `Code-Head:` trailer naming the code commit it was made on.
  - **New branch:** its journal forks from the journal of the branch it came from, so provenance carries over.
  - **Merge** (`git merge feature`, or pulling a teammate's work): on the next save (or as soon as Zed notices the new HEAD) wish records a journal merge commit joining both histories. If the code compiles, that merge is the new green state; nothing is regenerated.
  - **Blame** follows lines through merges: a line Claude generated on `feature` stays Claude's, with that cycle's reasoning, after it lands on `master`.
- Cycle commits carry structured messages, and the hover is literally `git blame` plus `git show`:
  ```
  cycle ok: Added `Gradient::Sunset` and `OpenWindow::background(...)` …

  <summary>

  Why:
  - Gradient: main.rs now uses `Gradient::Sunset` as the argument to `.background(...)` …

  Triggered-by:
  src/main.rs: +    App::new("Demo")…background(Gradient::Sunset).run()

  Mode: prototype
  Code-Head: 3a4f907…
  ```
- `refs/wish/<project>/last/<branch>`: the last green cycle. Your edits since then (`git diff`) are the regeneration scope.
- `refs/wish/<project>/prototype/<branch>`: the prototype frozen by implement mode.
- `refs/wish/<project>/attic/…`: reverted attempts. Implement mode still sees them in the history, but they stay out of blame.
- `refs/heads/wish/<project>/log/<branch>`: a branch mirror for Zed's git graph and lazygit. `wishlang log` prints the same story.
- **Undo** reverse-applies exactly the changes Claude made in the last cycle. Your lines and later edits stay, and `last` moves back.
- **Revert on violation** restores the before-snapshot. The failed attempt goes to the attic.
- Journals from older wishlang versions (a single `refs/wish/<project>/journal`) migrate to the current branch automatically.

### Sharing

The journals are plain git refs, but `git push` and `git clone` skip custom refs, so:

```sh
wishlang share          # push this project's journals (all branches) to origin
wishlang sync           # fetch others' journals: new ones adopted, behind ones fast-forwarded
```

After `wishlang sync`, a teammate sees exactly what you see: blue generated blocks, yours versus Claude's per line, and hovers with the reasoning. If journals diverge (you both generated on the same branch), they merge on the next save once your code contains theirs.

Adopted items are pinned by token hashes in `wish.toml`, because adoption is per item and must survive `rustfmt`.

## Decisions

- **Local choices** your line didn't force go in the item's doc comment under `# Decisions`.
- **Dependencies and architecture** come back as structured decisions, recorded in `DECISIONS.md` as *proposed*:
  - accept or reject them in Zed's prompt, or with `wishlang decide N accept|reject [reason]`;
  - rejected options are forbidden in later cycles;
  - `wishlang check` fails for any crate without a decision;
  - `[constraints] rules = [...]` in `wish.toml` steers decisions up front.

## Commands

```
wishlang lsp                     language server (Zed launches it via the extension)
wishlang log [--patch]           the journal: saves and cycles with Claude's reasoning
wishlang ticket [--json]         this branch's ticket: problems, and the files and items serving each
wishlang share [REMOTE]          push wish journals (default: origin)
wishlang sync [REMOTE]           fetch others' wish journals
wishlang dev                     terminal loop: c compile · f force · w compile-on-save · r restart · u undo · I implement
                             d decisions · s suggestions · i items · :<command> · q quit
wishlang compile [--json] [--force] [--no-run] [--note TEXT]
wishlang implement [FILE] [--item PATH] [--json]
wishlang undo
wishlang adopt|unadopt <item>    e.g. Mode, Window::open, Mode::Maximized (see `wishlang items`)
wishlang decisions | wishlang decide <id> accept|reject [reason]
wishlang suggestions | wishlang accept <id> | wishlang reject <id>
wishlang check                   adopted intact + dependencies decided + cargo check
wishlang init                    wish.toml (+ .zed/settings.json) + git journal for a cargo or Maven project
```

## Layout

```
wish/src/
  lsp.rs         language server: markers, blue semantic tokens, blame hover, code actions, cycles on save
  journal.rs     structured journal commit messages (why / triggered-by / decisions)
  engine.rs      one cycle: save → claude → verify/revert → item diff → record → journal
  git.rs         snapshots, journal/last refs, blame, history, restore/undo patches
  provenance.rs  per-line authorship from blame; "your lines must survive"
  pending.rs     syntactic "will be generated" detection
  claude.rs      headless Claude: base/prototype/implement rules, hook wiring, report schema
  hook.rs        PreToolUse guard (wiring, your lines, adopted items)
  items.rs       syn items, token/signature hashes, doc sections (# Decisions, # Mocked)
  project.rs     wish.toml, ownership, decisions/suggestions state
  commands.rs  dev.rs  preview.rs  events.rs
zed/             Zed extension (launches `wishlang lsp`)
examples/demo    demo project (+ .zed/settings.json)
```
