-- Real compiler tests in a temporary crate; never edit the demo or user's code.
local repo = vim.fn.getcwd()
vim.opt.rtp:prepend(repo)
local flow = require("flowistry")
local backend = require("flowistry.backend")
local render = require("flowistry.render")
local temp = vim.fn.tempname() .. " flowistry live"
local passed, requests = 0, 0
local original_request = backend.request
backend.request = function(...)
  requests = requests + 1
  return original_request(...)
end
local function check(value, message) assert(value, message); passed = passed + 1 end
local function ready(expected)
  check(vim.wait(300000, function() return flow.status() == expected or flow.status() == "error" end, 20), "analysis timed out")
  if flow.status() == "error" then
    flow.log()
    error(table.concat(vim.api.nvim_buf_get_lines(0, 0, -1, false), "\n"))
  end
  check(flow.status() == expected, "unexpected analysis state")
end
local function marks() return vim.api.nvim_buf_get_extmarks(0, render.namespace, 0, -1, { details = true }) end
local function dimmed(row, column)
  for _, mark in ipairs(marks()) do
    local r = { start = { mark[2], mark[3] }, finish = { mark[4].end_row, mark[4].end_col } }
    if mark[4].hl_group == "FlowistryDim" and require("flowistry.ranges").contains(r, { row, column }) then return true end
  end
  return false
end
local function run()
  vim.fn.mkdir(temp .. "/src", "p")
  vim.fn.writefile({ '[package]', 'name="flowistry_live"', 'version="0.1.0"', 'edition="2021"' }, temp .. "/Cargo.toml")
  vim.fn.writefile(vim.fn.readfile(repo .. "/examples/demo/src/main.rs"), temp .. "/src/main.rs")
  local opts = { root = temp, auto_enable = false, timeout_ms = 300000, debounce_ms = 5, batch = vim.env.FLOWISTRY_BATCH == "1" }
  if vim.env.FLOWISTRY_BACKEND then opts.command = vim.json.decode(vim.env.FLOWISTRY_BACKEND) end
  if vim.env.FLOWISTRY_BACKEND_EXE then opts.command = { vim.env.FLOWISTRY_BACKEND_EXE } end
  flow.setup(opts)
  vim.cmd.edit(vim.fn.fnameescape(temp .. "/src/main.rs"))
  vim.bo.filetype = "rust"
  vim.api.nvim_win_set_cursor(0, { 2, 12 })
  flow.enable()
  ready("active")
  check(dimmed(2, 12), "independent scores declaration must be dimmed for names")
  check(not dimmed(5, 4), "mutation through names' mutable reference must be relevant")
  local cached_requests = requests
  flow.mark()
  vim.api.nvim_buf_set_lines(0, 0, 0, false, { "// Unicode: é🦀; inserted before the pinned variable" })
  vim.api.nvim_exec_autocmds("TextChanged", { buffer = 0 })
  check(vim.wait(1000, function() return flow.status() == "waiting for save" end, 10), "unsaved edit suspends new analysis")
  check(#marks() > 0 and flow.is_stale(), "last analysis remains visible and marked stale")
  check(requests == cached_requests, "typing does not analyze old disk contents")
  vim.api.nvim_win_set_cursor(0, { 4, 12 })
  vim.cmd.write()
  ready("pinned")
  check(not flow.is_stale(), "successful saved analysis replaces stale display")
  check(dimmed(3, 12), "moved pin still selects names, not scores at the cursor")
  local focus_moved = false
  for _, mark in ipairs(marks()) do
    if mark[4].hl_group == "FlowistryFocus" and mark[2] == 2 then focus_moved = true end
  end
  check(focus_moved, "pinned position tracks the inserted source line")
  cached_requests = requests
  flow.unmark()
  ready("active")
  check(not dimmed(3, 12) and dimmed(2, 12), "unpin follows scores and dims independent names")
  check(requests == cached_requests, "changing variable in analyzed function does not recompile")
  vim.api.nvim_buf_set_lines(0, -1, -1, false, { "this is not Rust" })
  vim.cmd.write()
  check(vim.wait(300000, function() return flow.status() == "error" end, 20), "invalid saved Rust reports compiler failure")
  check(#marks() > 0 and flow.is_stale(), "compile failure preserves the last successful display")
  vim.api.nvim_buf_set_lines(0, -2, -1, false, {})
  vim.cmd.write()
  ready("active")
  check(not flow.is_stale(), "fixing and saving recovers without toggling Flowistry")
end
local ok, err = xpcall(run, debug.traceback)
flow.disable()
backend.request = original_request
vim.fn.delete(temp, "rf")
if not ok then io.stderr:write(err .. "\n"); vim.cmd("cquit 1") end
print(("Passed %d real-backend assertions (%d compiler requests)"):format(passed, requests))
vim.cmd("qa!")
