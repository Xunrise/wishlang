# wishlang for Neovim

Lua plugin that runs `wishlang lsp` for Rust and Java buffers. Needs Neovim 0.12+ and the `wishlang` binary ([install](https://xunrise.github.io/wishlang/)).

The plugin is glue: all behaviour lives in the language server.

- **Tint:** generated code is a semantic token of type `generated` (modifiers `prototype`, `untraced`), linked to `WishGenerated`, `WishPrototype`, `WishUntraced`.
- **Hover** (`vim.lsp.buf.hover()`): who wrote the code, why, and which problems of the ticket it serves.
- **Code lenses:** `▶ Implement`, `✓ Accept` / `✗ Reject`, `◆ P1 · P2`. Run the one under the cursor with `vim.lsp.codelens.run()`, or use code actions (`vim.lsp.buf.code_action()`).
- **Commands:** `:WishLog`, `:WishTicket [branch]` (float windows, `q` closes), `:WishImplement [file]`, `:WishAccept N`, `:WishReject N`, `:WishUndo`, `:WishRegenerate`.
- `:checkhealth wishlang` reports the binary and its version.

The root is the nearest `wish.toml`, else the git root. The binary is `$WISH_BIN`, else the first of `~/.local/bin/wishlang`, `/opt/homebrew/bin/wishlang`, `/usr/local/bin/wishlang`, `~/.cargo/bin/wishlang`, `~/.cargo/bin/wish`, else `wishlang` on PATH.

## Install

The plugin lives in the `nvim/` subfolder of the repo, so the plugin manager has to add that folder to the runtimepath.

**lazy.nvim**

```lua
{
  "Xunrise/wishlang",
  ft = { "rust", "java" },
  config = function(plugin)
    vim.opt.rtp:append(plugin.dir .. "/nvim")
    require("wishlang").setup()
  end,
}
```

**packer.nvim**

```lua
use { "Xunrise/wishlang", rtp = "nvim", config = function() require("wishlang").setup() end }
```

**Native packages**

```sh
git clone https://github.com/Xunrise/wishlang ~/.local/share/nvim/wishlang
mkdir -p ~/.local/share/nvim/site/pack/wishlang/start
ln -s ~/.local/share/nvim/wishlang/nvim ~/.local/share/nvim/site/pack/wishlang/start/wishlang
```

then `require("wishlang").setup()` in your config.

## Options

```lua
require("wishlang").setup({
  filetypes = { "rust", "java" },
  cmd = nil,          -- e.g. { "/path/to/wishlang", "lsp" }
  codelens = true,
  inlay_hints = true,
  highlights = true,  -- set the default highlight links; false to define your own
})
```

Override the tint in your colorscheme, for example `vim.api.nvim_set_hl(0, "WishGenerated", { bg = "#1f2a1f" })`.

Java: wishlang does not hide jdtls errors for symbols that don't exist yet. If they bother you, don't enable jdtls in wish projects.

## Test

From the repo root (needs `wishlang` installed):

```sh
nvim --headless -u NONE -l nvim/test/headless.lua
```
