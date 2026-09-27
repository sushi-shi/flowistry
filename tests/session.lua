-- Invoke with -c 'luafile .../tests/session.lua' after the packaged session setup.
-- The current buffer and cursor are the test target; no source is modified.
local function run()
local flow = require("flowistry")
vim.api.nvim_exec_autocmds("VimEnter", {})
vim.o.columns = 220
assert(vim.wo.winbar == "", "Unexpected bar above the buffer")
local function visible()
  return vim.api.nvim_eval_statusline(vim.wo.statusline, { maxwidth = 220 }).str
end
flow.enable()
vim.api.nvim_exec_autocmds("CursorMoved", { buffer = 0 })
local popup
assert(vim.wait(3000, function()
  for _, win in ipairs(vim.api.nvim_list_wins()) do
    if vim.fn.getwinvar(win, "source", "") == "flowistry" then popup = win; return true end
  end
end, 20), "Missing CoC analysis progress popup")
assert(vim.fn.getwinvar(popup, "message", ""):match("%.%.%. %d+%.%ds"), "Missing elapsed progress time")
assert(vim.api.nvim_get_current_win() ~= popup, "Progress popup stole focus")
local ready = vim.wait(300000, function()
  local status = flow.status()
  return status == "active" or status == "error" or status == "outside function" or status == "no place"
end, 50)
if not ready or flow.status() ~= "active" then
  io.stderr:write("Flowistry session failed: " .. flow.status() .. "\n")
  flow.log()
  io.stderr:write(table.concat(vim.api.nvim_buf_get_lines(0, 0, -1, false), "\n") .. "\n")
  vim.cmd("cquit 1")
end
local marks = vim.api.nvim_buf_get_extmarks(0, require("flowistry.render").namespace, 0, -1, { details = true })
local groups = {}
for _, mark in ipairs(marks) do
  local group = mark[4].hl_group
  groups[group] = (groups[group] or 0) + 1
end
assert(groups.FlowistryFocus, "Missing selected-variable highlight")
assert(groups.FlowistryDim, "Missing dimmed code")
if vim.env.FLOWISTRY_TEST_DOT == "1" then
  local cursor = vim.api.nvim_win_get_cursor(0)
  local dot_row, dot_column
  for index, line in ipairs(vim.api.nvim_buf_get_lines(0, cursor[1] - 1, cursor[1] + 12, false)) do
    local column = line:find(".unwrap_or(0)", 1, true)
    if column then dot_row, dot_column = cursor[1] + index - 1, column - 1; break end
  end
  assert(dot_row, "Missing unwrap_or following the pinned dy")
  local function await_status(expected)
    assert(vim.wait(300000, function()
      local status = flow.status()
      return status == expected or status == "error" or status == "outside function" or status == "no place"
    end, 20), "Selection update timed out")
    assert(flow.status() == expected, "Unexpected selection state: " .. flow.indicator())
  end
  vim.api.nvim_win_set_cursor(0, { dot_row, dot_column })
  flow.enable()
  await_status("no place")
  assert(#vim.api.nvim_buf_get_extmarks(0, require("flowistry.render").namespace, 0, -1, {}) == 0,
    "Gameplay unwrap_or dot selected its entire call chain")
  vim.api.nvim_win_set_cursor(0, { dot_row, dot_column + 2 })
  flow.enable()
  await_status("active")
  local focused = false
  for _, mark in ipairs(vim.api.nvim_buf_get_extmarks(0, require("flowistry.render").namespace, 0, -1, { details = true })) do
    if mark[4].hl_group == "FlowistryFocus" then
      assert(mark[2] == dot_row - 1 and mark[3] == dot_column + 1
        and mark[4].end_row == dot_row - 1 and mark[4].end_col == dot_column + 10,
        "Gameplay method-name selection extends beyond its word")
      focused = true
    end
  end
  assert(focused, "Gameplay method result is not selectable")
  vim.api.nvim_win_set_cursor(0, cursor)
  flow.enable()
  await_status("active")
  print("Gameplay unwrap_or punctuation and word-selection checks passed")
end
if vim.env.FLOWISTRY_TEST_CAMERA == "1" then
  local ranges = require("flowistry.ranges")
  local source = vim.api.nvim_buf_get_lines(0, 0, -1, false)
  local row
  for index, line in ipairs(source) do
    if line:find("angle_between_2d_rays(camera[0], camera[2], spawn_x, spawn_z", 1, true) then row = index; break end
  end
  assert(row, "Missing camera regression line")
  local function dimmed(token)
    local pos = { row - 1, assert(source[row]:find(token, 1, true)) - 1 }
    for _, mark in ipairs(vim.api.nvim_buf_get_extmarks(0, require("flowistry.render").namespace, 0, -1, { details = true })) do
      if mark[4].hl_group == "FlowistryDim" and ranges.contains({ start = { mark[2], mark[3] },
        finish = { mark[4].end_row, mark[4].end_col } }, pos) then return true end
    end
    return false
  end
  assert(dimmed("camera[0]") and dimmed("camera[2]"), "Independent camera arguments should be dimmed for section")
  assert(not dimmed("spawn_x") and not dimmed("anchor[0]"), "Section's dependent arguments should remain bright")
  local cursor = vim.api.nvim_win_get_cursor(0)
  vim.api.nvim_win_set_cursor(0, { row - 1, assert(source[row - 1]:find("yaw", 1, true)) - 1 })
  flow.enable()
  assert(not dimmed("camera[0]") and not dimmed("camera[2]"), "Selecting yaw must retain all contributing camera inputs")
  vim.api.nvim_win_set_cursor(0, cursor)
  flow.enable()
  print("Gameplay camera precision and backward-input checks passed")
end
print(vim.json.encode({ file = vim.api.nvim_buf_get_name(0), status = flow.status(), highlights = groups }))
local bar = visible()
assert(bar:find(" | flowistry", 1, true), "Missing separated status: " .. bar)
assert(not bar:find("ON", 1, true), "Redundant ON label")
assert(bar:find(vim.fn.expand("%:t"), 1, true), "Missing existing buffer list: " .. bar)
local styled = vim.api.nvim_eval_statusline(vim.wo.statusline, { maxwidth = 220, highlights = true })
local function group_at(word)
  local position = assert(styled.str:find(word, 1, true)) - 1
  local group
  for _, span in ipairs(styled.highlights) do
    if span.start <= position then group = span.group end
  end
  return group
end
assert(group_at("rust-analyzer") == group_at("flowistry"), "Flowistry style differs from rust-analyzer")
assert(vim.wait(1500, function() return not vim.api.nvim_win_is_valid(popup) end, 20), "Completed analysis left its popup open")
print("Visible status: " .. bar)
vim.cmd("Flow pin")
assert(visible():find("flowistry (pinned)", 1, true), "Missing pinned status")
local pinned_buf = vim.api.nvim_get_current_buf()
local other_file = vim.env.FLOWISTRY_WORKSPACE .. "/crates/stalker-formats/src/cursor.rs"
if vim.api.nvim_buf_get_name(0) ~= other_file then
  vim.cmd.edit(vim.fn.fnameescape(other_file))
  vim.wait(300, function() return false end)
  assert(flow.status(pinned_buf) == "pinned", "Opening another Rust file cleared the pin")
  vim.api.nvim_set_current_buf(pinned_buf)
  vim.wait(300, function() return false end)
  assert(visible():find("flowistry (pinned)", 1, true), "Returning to the buffer lost pinned status")
end
vim.cmd("Flow unpin")
assert(visible():find(" | flowistry", 1, true) and not visible():find("pinned", 1, true), "Unpin did not resume cursor tracking")
vim.cmd("Flow off")
assert(not visible():find("flowistry", 1, true), "Disabled status should be hidden")
vim.cmd("Flow on")
vim.g.coc_status = ""
assert(not require("flowistry.statusline").text():find("|", 1, true), "Separator shown without a language server")
vim.cmd("Flow off")
end
local ok, err = xpcall(run, debug.traceback)
if not ok then io.stderr:write(err .. "\n"); vim.cmd("cquit 1") end
vim.cmd("qa!")
