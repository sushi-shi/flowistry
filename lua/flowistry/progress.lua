local M = {}

function M.close(state)
  local popup = state.progress
  state.progress = nil
  if not popup or not vim.api.nvim_win_is_valid(popup.win) then return end
  if popup.coc then
    pcall(vim.fn["coc#notify#close"], popup.win)
  else
    vim.api.nvim_win_close(popup.win, true)
  end
end

function M.update(state)
  local elapsed = ((vim.uv or vim.loop).hrtime() - state.started) / 1e9
  local message = ("%s... %.1fs"):format(state.phase or "Analyzing", elapsed)
  if state.progress == nil then
    -- Use the same animated notification renderer as CoC language servers.
    if vim.g.did_coc_loaded == 1 then
      local ok, result = pcall(vim.fn["coc#notify#create"], { message }, {
        kind = "progress", title = "flowistry", source = "flowistry", focusable = 0,
        highlight = "Normal", borderhighlight = "CocNotificationProgress",
        minWidth = 40, maxWidth = 60, winblend = 30,
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
        col = vim.o.columns - width - 2, width = width, height = 1,
        style = "minimal", border = "rounded", title = " flowistry ", focusable = false,
      })
      if not ok then vim.api.nvim_buf_delete(buf, { force = true }) end
      state.progress = ok and { win = win, buf = buf } or false
    end
  end
  local popup = state.progress
  if not popup or not vim.api.nvim_win_is_valid(popup.win) then return end
  if popup.coc then
    vim.api.nvim_win_set_var(popup.win, "message", message)
  else
    vim.api.nvim_buf_set_lines(popup.buf, 0, -1, false, { message })
  end
end

return M
