local M = {}

function M.check()
  vim.health.start("wishlang")
  local bin = require("wishlang").binary()
  if not bin then
    return vim.health.error("wishlang binary not found", {
      "Install it: https://xunrise.github.io/wishlang/",
      "or set $WISH_BIN; searched ~/.local/bin, /opt/homebrew/bin, /usr/local/bin, ~/.cargo/bin, PATH",
    })
  end
  local out = vim.system({ bin, "--version" }, { text = true }):wait()
  if out.code == 0 then
    vim.health.ok(("%s: %s"):format(bin, vim.trim(out.stdout)))
  else
    vim.health.error(("`%s --version` failed: %s"):format(bin, vim.trim(out.stderr or "")))
  end
  if vim.fn.has("nvim-0.12") == 0 then
    vim.health.error("Neovim 0.12+ required")
  end
  local c = vim.lsp.get_clients({ name = "wishlang" })
  vim.health.info(#c > 0 and "language server running" or "language server not running (open a Rust or Java file in a wish project)")
end

return M
