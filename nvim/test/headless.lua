-- Smoke test: run from the repo root with
--   nvim --headless -u NONE -l nvim/test/headless.lua
-- Copies examples/demo-java to a temp git repo, opens it, and checks that wishlang attaches,
-- answers wish/fileState and wish/log, and that :WishLog opens a float. Needs `wishlang` installed.
local repo = vim.fn.getcwd()
vim.opt.rtp:append(repo .. "/nvim")
local wl = require("wishlang")
if not wl.binary() then
  print("SKIP: wishlang binary not found")
  return vim.cmd("qa!")
end

local tmp = vim.fn.tempname()
vim.fn.mkdir(tmp, "p")
vim.system({ "cp", "-R", repo .. "/examples/demo-java/.", tmp }):wait()
for _, c in ipairs({ { "init", "-q" }, { "add", "-A" }, { "-c", "user.email=t@t.t", "-c", "user.name=t", "commit", "-qm", "init" } }) do
  vim.system(vim.list_extend({ "git", "-C", tmp }, c)):wait()
end

local failed = false
local function check(ok, what)
  print((ok and "ok   " or "FAIL ") .. what)
  failed = failed or not ok
end

wl.setup()
vim.cmd("edit " .. tmp .. "/src/main/java/demo/Main.java")
vim.bo.filetype = "java"
check(vim.wait(20000, function() return #vim.lsp.get_clients({ name = "wishlang" }) > 0 end), "language server attaches")
local c = vim.lsp.get_clients({ name = "wishlang" })[1]
if c then
  check(vim.uv.fs_realpath(c.config.root_dir) == vim.uv.fs_realpath(tmp), "root is the wish.toml directory")
  local st = c:request_sync("wish/fileState", { textDocument = vim.lsp.util.make_text_document_params(0) }, 15000)
  check(st and st.result and st.result.pending ~= nil, "wish/fileState returns data")
  local log = c:request_sync("wish/log", vim.NIL, 15000)
  check(log and log.result and log.result.entries ~= nil, "wish/log returns entries")
  vim.cmd("WishLog")
  check(vim.wait(5000, function() return vim.bo.filetype == "markdown" end), ":WishLog opens a float")
end
vim.fn.delete(tmp, "rf")
vim.cmd(failed and "cq" or "qa!")
