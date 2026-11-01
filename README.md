# wishlang

Create-from-usage for Rust. You write the call sites you wish existed in a plain `main.rs`; Claude writes the vocabulary underneath in ordinary module files; the app relaunches as a live preview.

```rust
fn main() {
    App::new("Demo").window().open(Mode::Maximized).run()
}
```

The project stays normal, clean Rust. The only extra files are `wish.toml`, `DECISIONS.md`, and `.wish/`.

## Install & run

```sh
cargo install --path wish          # or: cargo build --release, use target/release/wish
cd examples/demo
wish dev                           # edit src/main.rs in your editor and save
```

Requires Claude Code (`claude`) on PATH and a git repository (`wish init` creates one if needed). Set `WISH_MODEL=sonnet` for faster cycles.

## Ownership

| What | Who | Protection |
|---|---|---|
| `src/main.rs` (any file in `[protected]`) | you | Claude may only *add* single-line `mod`/`use` wiring |
| other `src/**/*.rs`, `Cargo.toml` | Claude | none, until you adopt an item |
| adopted items (`wish adopt Mode`) | you | hashed; any code/doc change is refused |
| `wish.toml`, `DECISIONS.md`, `.wish/` | the tool | Claude can't touch them |

Protection is enforced twice:
1. A Claude Code PreToolUse hook (`wish hook`) refuses tool calls whose resulting file content would break the rules. The refusal tells Claude to file a suggestion instead.
2. Every compile cycle compares git snapshots taken before and after Claude ran. On a violation the whole cycle is reverted from the snapshot.

## The loop

1. **Write or change your lines** and save (or press `c` in `wish dev`).
2. **Claude regenerates** (headless `claude -p`). Its regeneration scope is `git diff` of your files since the last green cycle.
3. **You see what changed, item by item:** `+ Mode::Fullscreen  ~ impl ApplicationHandler for Handler  = 9 untouched`. That tells you whether "only what my change affects" held.
4. **The preview relaunches.**
5. **Steer by rewriting your lines**, not Claude's code. Use `:note <text>` for a one-off nudge.
6. **Adopt what you trust:** `wish adopt Window::open`. From then on it's yours.

When Claude thinks your code or an adopted item should change, it files a **suggestion** (`wish suggestions`, `wish accept N`, `wish reject N`). Accepting re-locks the adopted items it touches.

## Git is the source of truth

wishlang keeps no copies of your files. Each cycle snapshots the project directory into a git commit, built with a temporary index, so your branch, index and HEAD are never touched:

- `refs/wish/<project-path>/last` is the last green cycle. Its parent chain is the cycle history: `git log refs/wish/last` (or `refs/wish/examples/demo/last` for the demo).
- **Your edits** = `git diff <last> <now> -- src/main.rs`. That diff is what Claude is told to regenerate for.
- **Item-level change report**: `git diff --name-status` before and after the cycle, then each changed file's old and new versions are parsed with `git show`.
- **Revert on violation**: `git restore --source=<before> --worktree`, and files Claude added are deleted.
- A project that already compiles but has no history yet is recorded as the baseline without asking Claude.
- **Undo** (`wish undo`, or `u` in `wish dev`) restores everything except your protected files and `wish.toml` to the previous cycle, including that cycle's decisions and suggestions, and moves `last` back. If your lines changed in the undone cycle, the next compile regenerates for them. Undo refuses when it would change an item you adopted afterwards.

Committing stays your call: wishlang never commits to your branch. Adopted items are still pinned by token hashes in `wish.toml`, because git tracks whole files while adoption is per item and must survive `rustfmt`.

## Decisions

- **Local choices**, the ones your line didn't force, live in the item's doc comment:
  ```rust
  /// # Decisions
  /// - `Maximized` asks the window manager to maximize; not exclusive fullscreen.
  ```
- **Dependencies and architecture** (backend crate, error strategy, threading…) come back as structured decisions and are recorded in `DECISIONS.md` as *proposed*. Claude proceeds provisionally, so you see a running preview right away.
  - `wish decide 1 accept` makes a decision binding.
  - `wish decide 1 reject "too heavy, use minifb"` forbids it, and the next cycle (`f`) reworks the code.
- **Dependency gate:** `wish check` fails for any crate in `Cargo.toml` without a decision.
- **Constraints:** `[constraints] rules = ["no async", "prefer std"]` in `wish.toml` steer decisions up front.

## Commands

```
wish dev                     dev loop: c compile · f force · w compile-on-save · r restart preview · u undo
                             d decisions · s suggestions · i items · :<command> · q quit
wish compile [--json] [--force] [--no-run] [--note TEXT]
wish adopt|unadopt <item>    items: Mode, Window::open, impl Display for Mode  (see `wish items`)
wish decisions | wish decide <id> accept|reject [reason]
wish suggestions | wish accept <id> | wish reject <id>
wish undo                    roll Claude's changes back one cycle (keeps your code and wish.toml)
wish check                   adopted intact + dependencies decided + cargo check
wish init                    add wish.toml to an existing cargo project
```

## Editor integration (next)

`wish compile --json` streams NDJSON events: `tool`, `blocked`, `wiring`, `changes`, `decision`, `suggestion`, `check`, `done`. A Zed or nvim plugin is a thin client over that stream plus the user commands above.

## Layout

```
wish/src/
  engine.rs    one compile cycle: snapshot → claude → verify/revert → item diff → record → check
  git.rs       snapshots via temporary index, refs/wish/*, diff/show/restore
  claude.rs    headless Claude: rules prompt, tool allowlist, hook wiring, JSON report schema
  hook.rs      PreToolUse guard (computes post-edit content, checks wiring/adoption)
  items.rs     syn item identity + token hashes (rustfmt-proof), `# Decisions` extraction
  project.rs   wish.toml, ownership, decisions/suggestions state, wiring rule
  commands.rs  user commands;  dev.rs  terminal loop;  events.rs  event stream
examples/demo  the demo after its first cycle (your line + Claude's app.rs/window.rs)
```
