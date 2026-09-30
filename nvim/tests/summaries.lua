-- Exercise the packaged compiler through the actual frontend and decorations.
vim.opt.rtp:prepend(vim.fn.getcwd())
local flow = require("flowistry")
local ranges = require("flowistry.ranges")
local render = require("flowistry.render")
local temp = vim.fn.tempname() .. " flowistry summaries"
local source = {
  "struct State { a: i32, b: i32 }",
  "impl State {",
  "    fn inner(&mut self, input: i32) { self.b = input; }",
  "    fn update(&mut self, input: i32) { self.inner(input); }",
  "}",
  "fn main() {",
  "    let mut state = State { a: 1, b: 2 };",
  "    let input = 17;",
  "    state.update(input);",
  "    let untouched = state.a;",
  "    let affected = state.b;",
  "}",
}
local passed = 0
local function check(value, message) assert(value, message); passed = passed + 1 end
local function ready()
  check(vim.wait(300000, function() return flow.status() == "active" or flow.status() == "error" end, 20),
    "analysis timed out")
  if flow.status() == "error" then
    flow.log()
    error(table.concat(vim.api.nvim_buf_get_lines(0, 0, -1, false), "\n"))
  end
  check(flow.status() == "active", "analysis must produce a selectable variable")
end
local function select(row)
  vim.api.nvim_win_set_cursor(0, { row, 9 })
  vim.api.nvim_exec_autocmds("CursorMoved", { buffer = 0 })
  vim.wait(30, function() return false end)
  ready()
end
local function call_dimmed()
  local pos = { 8, assert(source[9]:find("update", 1, true)) - 1 }
  for _, mark in ipairs(vim.api.nvim_buf_get_extmarks(0, render.namespace, 0, -1, { details = true })) do
    if mark[4].hl_group == "FlowistryDim" and ranges.contains({
      start = { mark[2], mark[3] }, finish = { mark[4].end_row, mark[4].end_col },
    }, pos) then return true end
  end
  return false
end
local function run()
  vim.fn.mkdir(temp .. "/src", "p")
  vim.fn.writefile({ '[package]', 'name="summary_editor_test"', 'version="0.1.0"', 'edition="2021"' }, temp .. "/Cargo.toml")
  local file = temp .. "/src/main.rs"
  vim.fn.writefile(source, file)
  vim.cmd.edit(vim.fn.fnameescape(file))
  vim.bo.filetype = "rust"
  local executable = assert(vim.env.FLOWISTRY_BACKEND_EXE, "Set FLOWISTRY_BACKEND_EXE")
  local command = { executable }
  for _, mode in ipairs({ "default", "SigOnly", "Recurse" }) do
    for _, batch_max in ipairs({ 0, 600, -1 }) do
      flow.setup({ command = command, root = temp, auto_enable = false, debounce_ms = 1,
        context_mode = mode ~= "default" and mode or nil,
        batch = batch_max >= 0, batch_max_lines = math.max(batch_max, 0), timeout_ms = 300000 })
      vim.api.nvim_win_set_cursor(0, { 10, 9 })
      flow.enable()
      ready()
      check(call_dimmed() == (mode == "Recurse"), mode .. ": unrelated field must only refine with summaries")
      select(11)
      check(not call_dimmed(), mode .. ": nested mutation remains relevant")
      select(10)
      check(call_dimmed() == (mode == "Recurse"), mode .. ": cached cursor change preserves precision")
      check(#command == 1, "requests must not mutate the configured command")
      flow.disable()
    end
  end
  flow.setup({ command = command, root = temp, auto_enable = false, debounce_ms = 1,
    context_mode = "Recurse", batch = true, timeout_ms = 300000 })
  vim.api.nvim_win_set_cursor(0, { 10, 9 })
  flow.enable()
  ready()
  check(call_dimmed(), "initial summary excludes the untouched field")
  vim.api.nvim_buf_set_lines(0, 2, 3, false, { "    fn inner(&mut self, input: i32) { self.a = input; }" })
  vim.cmd.write()
  ready()
  check(not call_dimmed(), "saving a changed callee must replace its previous summary")
  select(11)
  check(call_dimmed(), "saving a changed callee must also remove obsolete dependencies")
  check(not pcall(flow.setup, { context_mode = "recurse" }), "invalid analysis modes fail at setup")
  check(flow.status() == "active", "invalid setup must preserve existing state")
end
local ok, err = xpcall(run, debug.traceback)
flow.disable()
vim.fn.delete(temp, "rf")
if not ok then io.stderr:write(err .. "\n"); vim.cmd("cquit 1") end
print(("Passed %d packaged callee-summary assertions"):format(passed))
vim.cmd("qa!")
