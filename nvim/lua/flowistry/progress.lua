local M = {}

function M.close(state)
  local popup = state.progress
  state.progress = nil
  if popup and popup.timer then popup.timer:stop(); popup.timer:close(); popup.timer = nil end
  if not popup or not vim.api.nvim_win_is_valid(popup.win) then return end
  if popup.coc then
    pcall(vim.fn["coc#notify#close"], popup.win)
  else
    vim.api.nvim_win_close(popup.win, true)
  end
end

local function show(state, message, is_error)
  if state.progress == nil then
    -- Use the same animated notification renderer as CoC language servers.
    if vim.g.did_coc_loaded == 1 then
      local ok, result = pcall(vim.fn["coc#notify#create"], { message }, {
        kind = is_error and "error" or "progress", title = "flowistry", source = "flowistry", focusable = 0,
        highlight = "Normal", borderhighlight = is_error and "DiagnosticError" or "CocNotificationProgress",
        minWidth = 40, maxWidth = 60, maxHeight = 2, timeout = 5000, winblend = 30,
      })
      state.progress = ok and type(result) == "table" and result[1]
        and { win = result[1], buf = result[2], coc = true } or false
    else
      local width = math.min(52, vim.o.columns - 4)
      if width < 10 or vim.o.lines < 6 then state.progress = false; return end
      local buf = vim.api.nvim_create_buf(false, true)
      vim.bo[buf].bufhidden = "wipe"
      local ok, win = pcall(vim.api.nvim_open_win, buf, false, {
        relative = "editor", row = math.max(0, vim.o.lines - 6),
        col = vim.o.columns - width - 2, width = width,
        height = is_error and math.min(2, math.max(1, math.ceil(vim.fn.strdisplaywidth(message) / width))) or 1,
        style = "minimal", border = "rounded", title = " flowistry ", focusable = false,
      })
      if not ok then vim.api.nvim_buf_delete(buf, { force = true }) end
      state.progress = ok and { win = win, buf = buf } or false
      if ok and is_error then vim.wo[win].winhl = "FloatBorder:DiagnosticError" end
    end
  end
  local popup = state.progress
  if not popup or not vim.api.nvim_win_is_valid(popup.win) then return end
  if popup.coc and not is_error then
    vim.api.nvim_win_set_var(popup.win, "message", message)
  elseif not popup.coc then
    vim.api.nvim_buf_set_lines(popup.buf, 0, -1, false, { message })
  end
end

function M.update(state)
  local elapsed = ((vim.uv or vim.loop).hrtime() - state.started) / 1e9
  show(state, ("%s... %.1fs"):format(state.phase or "Analyzing", elapsed))
end

function M.error(state, message)
  M.close(state)
  -- Prefer the compiler diagnostic over Cargo chatter and transport wrappers.
  -- The complete, unmodified error remains available through :Flow log.
  local first, detail
  message = message:gsub("\27%[[%d;]*m", "")
  message = message:match("(Process exited with code [^\r\n]*[\r\n]?.*)") or message
  for line in message:gmatch("[^\r\n]+") do
    line = vim.trim(line)
    if line ~= "" then
      first = first or line
      if line:match("^error[%[:]") then detail = line; break end
      if not line:match("^Process exited with code") and not line:match("^Flowistry produced no analysis") then
        detail = detail or line
      end
    end
  end
  local text = (detail or first or "Analysis unavailable"):gsub("%c", " ")
  if vim.fn.strchars(text) > 160 then text = vim.fn.strcharpart(text, 0, 159) .. "…" end
  show(state, text, true)
  local popup = state.progress
  if popup and not popup.coc then
    popup.timer = vim.defer_fn(function()
      popup.timer = nil
      if state.progress == popup then M.close(state) end
    end, 5000)
  end
end

return M
