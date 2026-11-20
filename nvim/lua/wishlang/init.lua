-- wishlang for Neovim: starts `wishlang lsp` for Rust and Java buffers and adds the wishlang commands.
-- All behaviour lives in the language server; this file is glue.
local M = {}

M.defaults = {
  filetypes = { "rust", "java" },
  cmd = nil, -- full command override, e.g. { "/path/to/wishlang", "lsp" }
  codelens = true,
  inlay_hints = true,
  highlights = true,
}
M.opts = vim.deepcopy(M.defaults)

--- The wishlang binary: $WISH_BIN, else the first usual install location, else PATH.
--- `wish` is only found in ~/.cargo/bin (it clashes with Tk's /usr/bin/wish elsewhere).
function M.binary()
  local env = vim.env.WISH_BIN
  if env and env ~= "" then
    return vim.fn.executable(env) == 1 and env or nil
  end
  local home = vim.uv.os_homedir()
  for _, p in ipairs({
    home .. "/.local/bin/wishlang",
    "/opt/homebrew/bin/wishlang",
    "/usr/local/bin/wishlang",
    home .. "/.cargo/bin/wishlang",
    home .. "/.cargo/bin/wish",
  }) do
    if vim.fn.executable(p) == 1 then
      return p
    end
  end
  local onpath = vim.fn.exepath("wishlang")
  return onpath ~= "" and onpath or nil
end

local function root_dir(buf)
  return vim.fs.root(buf, { "wish.toml" }) or vim.fs.root(buf, { ".git" })
end

local function client(buf)
  return vim.lsp.get_clients({ bufnr = buf or 0, name = "wishlang" })[1]
end

local warned
local function start(buf)
  local cmd = M.opts.cmd
  if not cmd then
    local bin = M.binary()
    if not bin then
      if not warned then
        warned = true
        vim.notify("wishlang: binary not found (install it, or set $WISH_BIN). See :checkhealth wishlang", vim.log.levels.WARN)
      end
      return
    end
    cmd = { bin, "lsp" }
  end
  local root = root_dir(buf)
  if not root then
    return
  end
  vim.lsp.start({
    name = "wishlang",
    cmd = cmd,
    root_dir = root,
    handlers = {
      -- The project changed (a cycle, a hook, a CLI command): redraw tint and lenses.
      ["wish/changed"] = function()
        vim.schedule(function()
          for _, c in ipairs(vim.lsp.get_clients({ name = "wishlang" })) do
            for b in pairs(c.attached_buffers) do
              if vim.api.nvim_buf_is_loaded(b) then
                vim.lsp.semantic_tokens.force_refresh(b)
                if M.opts.codelens then
                  vim.lsp.codelens.enable(true, { bufnr = b })
                end
              end
            end
          end
        end)
      end,
    },
  }, { bufnr = buf })
end

local function highlights()
  -- Generated code is a semantic token of type `generated`; modifiers mark prototype/untraced code.
  local set = function(name, def) vim.api.nvim_set_hl(0, name, vim.tbl_extend("keep", { default = true }, def)) end
  set("WishGenerated", { link = "DiffAdd" })
  set("WishPrototype", { link = "DiffChange" })
  set("WishUntraced", { undercurl = true, sp = "Red" })
  set("@lsp.type.generated", { link = "WishGenerated" })
  set("@lsp.mod.prototype", { link = "WishPrototype" })
  set("@lsp.mod.untraced", { link = "WishUntraced" })
end

local function on_attach(args)
  local c = vim.lsp.get_client_by_id(args.data.client_id)
  if not c or c.name ~= "wishlang" then
    return
  end
  if M.opts.codelens then
    vim.lsp.codelens.enable(true, { bufnr = args.buf })
  end
  if M.opts.inlay_hints then
    vim.lsp.inlay_hint.enable(true, { bufnr = args.buf })
  end
end

--- Run a server command (wish.implement, wish.accept, ...).
local function exec(command, arguments)
  local c = client()
  if not c then
    return vim.notify("wishlang: not attached to this buffer", vim.log.levels.WARN)
  end
  c:request("workspace/executeCommand", { command = command, arguments = arguments or {} }, function(err)
    if err then
      vim.notify("wishlang: " .. (err.message or vim.inspect(err)), vim.log.levels.ERROR)
    end
  end)
end

local function float(lines, title)
  local buf = vim.api.nvim_create_buf(false, true)
  vim.api.nvim_buf_set_lines(buf, 0, -1, false, lines)
  vim.bo[buf].filetype = "markdown"
  vim.bo[buf].modifiable = false
  vim.bo[buf].bufhidden = "wipe"
  local w, h = math.floor(vim.o.columns * 0.8), math.floor(vim.o.lines * 0.7)
  vim.api.nvim_open_win(buf, true, {
    relative = "editor", width = w, height = h, row = math.floor((vim.o.lines - h) / 2),
    col = math.floor((vim.o.columns - w) / 2), border = "rounded", title = " " .. title .. " ", style = "minimal",
  })
  vim.keymap.set("n", "q", "<cmd>close<cr>", { buffer = buf, nowait = true })
  vim.keymap.set("n", "<esc>", "<cmd>close<cr>", { buffer = buf, nowait = true })
  return buf
end

-- JSON null decodes to vim.NIL (truthy userdata); drop it so `x or default` works.
local function denull(v)
  if v == vim.NIL then
    return nil
  elseif type(v) == "table" then
    for k, x in pairs(v) do
      v[k] = denull(x)
    end
  end
  return v
end

local function request(method, params, render, title)
  local c = client()
  if not c then
    return vim.notify("wishlang: not attached to this buffer", vim.log.levels.WARN)
  end
  c:request(method, params, function(err, res)
    if err then
      return vim.notify("wishlang: " .. (err.message or vim.inspect(err)), vim.log.levels.ERROR)
    end
    vim.schedule(function() float(render(denull(res) or {}), title) end)
  end)
end

local function render_log(r)
  local out = { "# wishlang log", "" }
  for _, e in ipairs(r.entries or {}) do
    out[#out + 1] = ("## %s  %s: %s  (%s%s)"):format(e.sha:sub(1, 8), e.author, e.label, e["when"], e.ticket and (", " .. e.ticket) or "")
    if e.summary and e.summary ~= "" then
      out[#out + 1] = e.summary
    end
    for _, w in ipairs(e.why or {}) do
      local pr = (w.problems and #w.problems > 0) and (" [" .. table.concat(w.problems, ", ") .. "]") or ""
      out[#out + 1] = ("- **%s**%s: %s"):format(w.item, pr, w.why)
    end
    out[#out + 1] = ""
  end
  return out
end

local function render_ticket(r)
  local out = { "# " .. (r.key ~= "" and r.key or r.branch or "ticket") .. (r.title and (": " .. r.title) or ""), "" }
  if r.body and r.body ~= "" then
    vim.list_extend(out, vim.split(r.body, "\n"))
    out[#out + 1] = ""
  end
  for _, p in ipairs(r.problems or {}) do
    out[#out + 1] = ("## %s  %s"):format(p.id, p.text)
    for _, it in ipairs(p.items or {}) do
      out[#out + 1] = ("- **%s** (%s:%d)"):format(it.item, it.file, it.line)
      for _, re in ipairs(it.reasons or {}) do
        out[#out + 1] = "  - " .. re.why
      end
    end
    out[#out + 1] = ""
  end
  if r.untraced and #r.untraced > 0 then
    out[#out + 1] = "## Untraced (serve no problem)"
    for _, it in ipairs(r.untraced) do
      out[#out + 1] = ("- **%s** (%s:%d)"):format(it.item, it.file, it.line)
    end
  end
  return out
end

function M.setup(opts)
  M.opts = vim.tbl_deep_extend("force", M.defaults, opts or {})
  local group = vim.api.nvim_create_augroup("wishlang", { clear = true })
  vim.api.nvim_create_autocmd("FileType", {
    group = group, pattern = M.opts.filetypes, callback = function(a) start(a.buf) end,
  })
  vim.api.nvim_create_autocmd("LspAttach", { group = group, callback = on_attach })
  if M.opts.highlights then
    highlights()
    vim.api.nvim_create_autocmd("ColorScheme", { group = group, callback = highlights })
  end

  local cmd = vim.api.nvim_create_user_command
  cmd("WishLog", function() request("wish/log", vim.NIL, render_log, "wishlang log") end, {})
  cmd("WishTicket", function(a)
    request("wish/ticket", { branch = a.args ~= "" and a.args or vim.NIL }, render_ticket, "wishlang ticket")
  end, { nargs = "?" })
  cmd("WishImplement", function(a)
    -- no argument: the whole prototype; "file": this file's prototype items
    if a.args == "file" then
      exec("wish.implement", { vim.fn.expand("%:."), vim.NIL })
    else
      exec("wish.implement", {})
    end
  end, { nargs = "?", complete = function() return { "file" } end })
  cmd("WishAccept", function(a) exec("wish.accept", { tonumber(a.args) }) end, { nargs = 1 })
  cmd("WishReject", function(a) exec("wish.reject", { tonumber(a.args) }) end, { nargs = 1 })
  cmd("WishUndo", function() exec("wish.undo") end, {})
  cmd("WishRegenerate", function() exec("wish.regenerate") end, {})

  -- Buffers that were already open before setup() ran.
  for _, b in ipairs(vim.api.nvim_list_bufs()) do
    if vim.api.nvim_buf_is_loaded(b) and vim.tbl_contains(M.opts.filetypes, vim.bo[b].filetype) then
      start(b)
    end
  end
end

return M
