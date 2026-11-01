# wishlang

Create-from-usage for Rust. You write the call sites you wish existed; Claude writes the vocabulary underneath, first as a skimmable **prototype**, then, once you're happy, as the real **implementation**. The app restarts on every save so you see each change.

```rust
fn main() {
    App::new("Demo").window().open(Mode::Windowed(800, 600)).background(Gradient::Sunset).run()
}
```

The project stays normal, clean Rust. The only extra files are `wish.toml`, `DECISIONS.md`, `.wish/` and `.zed/settings.json`.

## Install

```sh
cargo install --path wish        # puts `wish` on PATH (Zed looks for it there, or in $WISH_BIN)
```

Then, in Zed, open the command palette, run **zed: install dev extension**, and pick the `zed/` folder of this repo.

The blue comes from two semantic token rules in your Zed **user** settings (Zed only reads `global_lsp_settings` there):

```json
"global_lsp_settings": {
  "semantic_token_rules": [
    { "token_type": "generated", "token_modifiers": ["prototype"], "background_color": "#3b82f633", "font_style": "italic" },
    { "token_type": "generated", "background_color": "#3b82f61f" }
  ]
}
```

They only match the token type `generated`, which only `wish lsp` emits. The project's `.zed/settings.json` turns on `"semantic_tokens": "combined"` for Rust. After changing settings, run `editor: restart language server` once.

Requires Claude Code (`claude`) on PATH and a git repository (`wish init` creates one). Set `WISH_MODEL=sonnet` for faster cycles.

## Working in Zed

Open `examples/demo` (or any project after `wish init`) in Zed.

| You see | Meaning |
|---|---|
| green/yellow/red in the gutter | git changes (Zed native) |
| red squiggles | real syntax/type problems (rust-analyzer) |
| **blue** `✦ will be generated: Mode::Windowed` | you used something that doesn't exist yet; it's generated on save |
| **blue** `◇ prototype …` | this item's behaviour is still mocked (its doc has a `# Mocked` section) |
| **blue background** | Claude generated this code, persistently, every time you open the project |
| **stronger blue, italic** | generated *and* still mocked (prototype) |
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

The terminal alternative is `wish dev`, which offers the same loop, with keys.

### Panels (reusing Zed's built-in views)

- **What wish did:** the journal is mirrored to a local branch, `wish/<project>/log` (`wish/examples/demo/log` for the demo). Open **git graph** in Zed and select it: every save and cycle, Claude's "Why" per item, changed files and diffs. lazygit works too. In the terminal (or the Zed task `wish: log`), use `wish log` (`--patch` for diffs).
- **All suggestions:** pending suggestions are published as warnings for every file, so Zed's **project diagnostics** (`ctrl-shift-m`) lists them all. Accept or reject them with the lens or `ctrl-.`. The Zed task `wish: suggestions` lists them in the terminal.

### Your lines inside generated files

They are wishes. Claude keeps them and makes them true. Formatting may change, and an empty body you write is filled in: write `pub enum Color {}` and the declaration line stays **yours** (in blame and color) while the variants Claude adds are generated.

## Two phases

**Prototype** (every save). Claude writes header-like files you can skim in seconds:
- public types, signatures and doc comments carry the meaning;
- bodies are a few well-named steps;
- details are mocked with the simplest thing that makes your change visible, and each mocked item says so in a `# Mocked` doc section. That section is the single source of truth for "prototype".

You iterate by editing your lines. You can also edit **inside generated files**: a line you add there (e.g. `/// The title shows the current mode`) is a wish too. Claude keeps it verbatim and makes it true. Whatever you don't specify, Claude infers from context.

**Implement** (`wish implement [file]`, the code action, or `I` in `wish dev`). Claude turns the prototype into the real thing, in place:
- Public signatures and type definitions must stay identical (verified; any change reverts the cycle).
- Public bodies stay at intent level. Details go into private helpers or submodules (e.g. `src/window/paint.rs`), so each file still reads top-down at one flight level.
- Claude gets the full **history** of how the prototype came about: every save, every cycle, and what you undid or abandoned.
- The prototype is frozen at `refs/wish/<project>/prototype`.

## Provenance and protection

Every line is either yours or generated, and both can live in one file. Provenance comes from git blame on the journal (below), so it needs no state of its own, persists across sessions, and covers unsaved buffers: lines you're typing are yours. `wish lsp` turns it into semantic tokens (blue) and hovers.

| What | Protection |
|---|---|
| your protected files (`[protected]` in `wish.toml`, default `src/main.rs`) | Claude may only *add* single-line `mod`/`use` wiring |
| your lines inside generated files | must survive every cycle verbatim; Claude restructures around them |
| adopted items (`wish adopt Mode`) | hashed; any code or doc change is refused |
| `wish.toml`, `DECISIONS.md`, `.wish/` | tool-owned; Claude can't touch them |

Protection is enforced twice:
1. A Claude Code PreToolUse hook (`wish hook`) refuses tool calls that would break a rule, and tells Claude to file a suggestion instead.
2. Every cycle verifies afterwards, and reverts the whole cycle on a violation.

## Git is the source of truth

Snapshots are commits built with a temporary index; your branch, index and HEAD are never touched. wishlang never commits to your branch. Per project:

- `refs/wish/<project>/journal`: every save (authored **you**) and every Claude cycle (authored **claude**). Blame on it gives line provenance. `git log refs/wish/examples/demo/journal` is the full story, and implement mode reads it as history. Cycle commits carry structured messages, and the hover is literally `git blame` followed by `git show`:
  ```
  cycle ok: Added `Gradient::Sunset` and `OpenWindow::background(...)` …

  <summary>

  Why:
  - Gradient: main.rs now uses `Gradient::Sunset` as the argument to `.background(...)` …
  - Handler::resumed: Prints the gradient's colours when the window opens, because nothing is painted yet …

  Triggered-by:
  src/main.rs: +    App::new("Demo")…background(Gradient::Sunset).run()

  Mode: prototype
  ```
  The same works in a terminal: `git blame refs/wish/examples/demo/journal -- src/window.rs`, then `git show <sha>`.
- These refs are local: they survive restarts but aren't pushed by default. To share provenance, push `refs/wish/*` explicitly.
- `refs/wish/<project>/last`: the last green cycle. Your edits since then (`git diff`) are the regeneration scope.
- **Undo** reverse-applies exactly the changes Claude made in the last cycle. Your lines and later edits stay, and `last` moves back.
- **Revert on violation** restores the before-snapshot; the failed attempt stays in the journal as a side parent.

Adopted items are pinned by token hashes in `wish.toml`, because adoption is per item and must survive `rustfmt`.

## Decisions

- **Local choices** your line didn't force go in the item's doc comment under `# Decisions`.
- **Dependencies and architecture** come back as structured decisions, recorded in `DECISIONS.md` as *proposed*:
  - accept or reject them in Zed's prompt, or with `wish decide N accept|reject [reason]`;
  - rejected options are forbidden in later cycles;
  - `wish check` fails for any crate without a decision;
  - `[constraints] rules = [...]` in `wish.toml` steers decisions up front.

## Commands

```
wish lsp                     language server (Zed launches it via the extension)
wish log [--patch]           the journal: saves and cycles with Claude's reasoning
wish dev                     terminal loop: c compile · f force · w compile-on-save · r restart · u undo · I implement
                             d decisions · s suggestions · i items · :<command> · q quit
wish compile [--json] [--force] [--no-run] [--note TEXT]
wish implement [FILE] [--item PATH] [--json]
wish undo
wish adopt|unadopt <item>    e.g. Mode, Window::open, Mode::Maximized (see `wish items`)
wish decisions | wish decide <id> accept|reject [reason]
wish suggestions | wish accept <id> | wish reject <id>
wish check                   adopted intact + dependencies decided + cargo check
wish init                    wish.toml + .zed/settings.json + git journal for a cargo project
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
zed/             Zed extension (launches `wish lsp`)
examples/demo    demo project (+ .zed/settings.json)
```
