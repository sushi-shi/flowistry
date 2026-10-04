-- Real compiler, real navigation and extmarks across two source files.
vim.opt.rtp:prepend(vim.fn.getcwd())
vim.cmd("runtime plugin/flowistry.lua")
local flow = require("flowistry")
local render = require("flowistry.render")
local ranges = require("flowistry.ranges")
local backend = require("flowistry.backend")
local temp = vim.fn.tempname() .. " flowistry calls"
local crate = temp .. "/member"
local main = {
  "mod helper;",
  "fn hello(x: i64, y: i64) -> i64 {",
  "    let answer = helper::goo(x, y);",
  "    answer",
  "}",
  "fn main() { hello(1, 2); }",
}
local helper = {
  "fn leaf(café: i64, other: i64) -> i64 {",
  "    let used = café + 3;",
  "    let ignored = other + 7;",
  "    used",
  "}",
  "pub fn goo(value: i64, other: i64) -> i64 { leaf(value, other) }",
  "pub fn unrelated(other: i64) -> i64 { other + 123 }",
}
local passed, requests, pins = 0, 0, 0
local original_request = backend.request
backend.request = function(context, args, config, callback)
  requests = requests + 1
  if args[1] == "pin-focus" then pins = pins + 1 end
  return original_request(context, args, config, callback)
end
local function check(value, message) assert(value, message); passed = passed + 1 end
local function ready(status)
  check(vim.wait(60000, function()
    return flow.status() == status or flow.status() == "error" or flow.status() == "analysis unavailable"
  end, 10), "timed out waiting for " .. status .. ": " .. flow.indicator())
  if flow.status() ~= status then
    flow.log()
    error(table.concat(vim.api.nvim_buf_get_lines(0, 0, -1, false), "\n"))
  end
end
local function move(row, word)
  local line = vim.api.nvim_buf_get_lines(0, row - 1, row, false)[1]
  vim.api.nvim_win_set_cursor(0, { row, assert(line:find(word, 1, true)) - 1 })
  vim.api.nvim_exec_autocmds("CursorMoved", { buffer = 0 })
end
local function dim(row, word)
  local line = vim.api.nvim_buf_get_lines(0, row - 1, row, false)[1]
  local pos = { row - 1, assert(line:find(word, 1, true)) - 1 }
  for _, mark in ipairs(vim.api.nvim_buf_get_extmarks(0, render.namespace, 0, -1, { details = true })) do
    if mark[4].hl_group == "FlowistryDim" and ranges.contains({ start = { mark[2], mark[3] },
      finish = { mark[4].end_row, mark[4].end_col } }, pos) then return true end
  end
  return false
end
local function run()
  vim.fn.mkdir(crate .. "/src", "p")
  vim.fn.writefile({ '[package]', 'name="pin_editor_test"', 'version="0.1.0"', 'edition="2021"' }, crate .. "/Cargo.toml")
  vim.fn.writefile({ '[workspace]', 'members=["member"]', 'resolver="2"' }, temp .. "/Cargo.toml")
  vim.fn.writefile(main, crate .. "/src/main.rs")
  vim.fn.writefile(helper, crate .. "/src/helper.rs")
  vim.cmd.edit(vim.fn.fnameescape(crate .. "/src/main.rs"))
  vim.bo.filetype = "rust"
  flow.setup({ command = { assert(vim.env.FLOWISTRY_BACKEND_EXE) }, root = crate,
    auto_enable = false, batch = true, follow_calls = true, context_mode = "Recurse", debounce_ms = 1 })
  move(2, "x:")
  flow.enable()
  ready("active")
  flow.mark()
  ready("pinned")
  local root = vim.api.nvim_get_current_buf()
  local previous = requests
  move(6, "hello")
  ready("outside pinned calls")
  move(2, "x:")
  ready("pinned")
  vim.cmd.edit(vim.fn.fnameescape(crate .. "/src/helper.rs"))
  vim.bo.filetype = "rust"
  move(2, "used")
  flow.enable()
  ready("pinned calls")
  check(not dim(2, "café"), "post follows a renamed Unicode parameter through two calls")
  check(dim(3, "other"), "independent callee input stays dim")
  move(7, "other")
  ready("outside pinned calls")
  move(2, "used")
  ready("pinned calls")
  flow.pre()
  ready("pinned calls")
  check(require("flowistry.statusline").text():find("(pre)", 1, true), "the normal statusline identifies pin direction")
  check(dim(2, "café"), "an affected callee does not become a cause of the caller's input")
  flow.post()
  ready("pinned calls")
  check(not dim(2, "café"), "post restores the affected use")
  move(6, "value")
  vim.wait(25, function() return false end)
  check(not dim(6, "value"), "same-file navigation retains the pin's slice")
  flow.both()
  ready("pinned calls")
  check(requests == previous, "navigation and direction changes do not start compiler requests")
  check(pins == 1, "one compiler request computes both cross-function directions")
  move(2, "used")
  flow.unmark()
  ready("active")
  check(#vim.api.nvim_buf_get_extmarks(root, require("flowistry.pin").namespace, 0, -1, {}) == 0,
    "unpin from a callee clears the origin's marker")
  vim.api.nvim_set_current_buf(root)
  move(4, "answer")
  flow.mark()
  ready("pinned")
  flow.pre()
  vim.cmd.edit(vim.fn.fnameescape(crate .. "/src/helper.rs"))
  move(2, "used")
  vim.api.nvim_exec_autocmds("BufEnter", { buffer = 0 })
  ready("pinned calls")
  check(not dim(2, "café"), "pre follows the returned value into the callee's calculation")
  check(dim(3, "other"), "pre excludes the callee's unused input")
  local before_save = pins
  helper[2] = "    let used = café + 9;"
  vim.api.nvim_buf_set_lines(0, 1, 2, false, { helper[2] })
  vim.api.nvim_exec_autocmds("TextChanged", { buffer = 0 })
  check(flow.is_stale(), "editing a callee marks the pin's decorations stale")
  vim.cmd.write()
  ready("pinned calls")
  check(pins == before_save + 1, "saving a callee recomputes the origin's cross-function slice")
  check(not dim(2, "café") and dim(3, "other"), "refreshed pin preserves directional precision")
  flow.unmark()
  ready("active")
  check(pins == 3, "unpin does not issue another cross-function request")
end
local ok, err = xpcall(run, debug.traceback)
flow.stop()
backend.request = original_request
vim.fn.delete(temp, "rf")
if not ok then io.stderr:write(err .. "\n"); vim.cmd("cquit 1") end
print(("Passed %d cross-function pin assertions"):format(passed))
vim.cmd("qa!")
