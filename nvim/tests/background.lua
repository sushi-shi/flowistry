local repo = vim.fn.getcwd()
vim.opt.rtp:prepend(repo)
vim.cmd("runtime plugin/flowistry.lua")
local flow = require("flowistry")
local backend = require("flowistry.backend")
local root = vim.fn.tempname() .. " background é"
vim.fn.mkdir(root .. "/src", "p")
local file, log = root .. "/src/main.rs", root .. "/calls.jsonl"
vim.fn.writefile({ '[package]', 'name="fixture"', 'version="0.0.0"', 'edition="2021"' }, root .. "/Cargo.toml")
local lines = { "fn main() {", "    let x = 1;", "    let y = 2;", '    println!("{}", x); ',
  '    println!("{}", y); ', "}", "", "fn outer() {", "    fn inner() {", "        let z = 3;", "    }", "}" }
vim.fn.writefile(lines, file)
vim.cmd.edit(vim.fn.fnameescape(file)); vim.bo.filetype = "rust"
local buf = vim.api.nvim_get_current_buf()
local notes, passed = {}, 0
vim.notify = function(message) notes[#notes + 1] = message end
local function check(value, message) assert(value, message); passed = passed + 1 end
local function await(test, message) check(vim.wait(8000, test, 10), message .. "\n" .. vim.inspect(notes)) end
local function calls(action)
  local found = {}
  for _, line in ipairs(vim.fn.filereadable(log) == 1 and vim.fn.readfile(log) or {}) do
    local call = vim.json.decode(line)
    if not action or call.action == action then found[#found + 1] = call end
  end
  return found
end
local command = { vim.v.progpath, "--headless", "-u", "NONE", "-l", repo .. "/tests/fake_project.lua" }
local function setup(delay)
  flow.setup({ command = command, batch = true, batch_max_lines = 1, debounce_ms = 5,
    project = { enabled = true, idle_ms = 30, memory_mib = 512, timeout_seconds = 30 },
    env = { FLOWISTRY_TEST_PROJECT_LOG = log, FLOWISTRY_TEST_PROJECT_DELAY = tostring(delay or 20) } })
  vim.api.nvim_win_set_cursor(0, { 2, 8 })
end
local function move(row, column)
  vim.api.nvim_win_set_cursor(0, { row, column })
  vim.api.nvim_exec_autocmds("CursorMoved", { buffer = buf })
end
local ok, err = xpcall(function()
  setup()
  await(function() return flow.status() == "active" end, "foreground analysis works with automatic target selection")
  await(function() local p = flow.project_status(); return p and p.status == "complete" end, "background project completes")
  check(#calls("project-targets") == 1, "one target discovery for the buffer context")
  check(#calls("project") == 1, "one project coordinator warms the workspace")
  local foreground = #calls("file-focus")
  move(10, 12)
  await(function() return flow.status() == "active" end, "navigation uses another warmed function")
  check(#calls("file-focus") == foreground, "warmed navigation starts no duplicate foreground request")
  local first = calls("file-focus")[1].args
  check(vim.tbl_contains(first, "--package") and vim.tbl_contains(first, "--target-name"), "foreground and background share explicit target selection")

  setup(600)
  await(function() local p = flow.project_status(); return p and p.status == "running" end, "second background generation starts")
  local projects_before = #calls("project")
  move(10, 12)
  await(function() return flow.status() == "active" end, "foreground navigation preempts background work")
  await(function() return #calls("project") > projects_before end, "background resumes after foreground completes")
  vim.api.nvim_buf_set_lines(buf, 1, 2, false, { "    let changed = 10;" })
  vim.api.nvim_exec_autocmds("TextChanged", { buffer = buf })
  await(function() return flow.status() == "waiting for save" end, "editing suspends analysis")
  local during_edit = #calls("project")
  vim.wait(800, function() return false end, 10)
  check(#calls("project") == during_edit, "background stays paused while project buffers are dirty")
  check(flow.is_stale(), "saved highlights remain explicitly stale")
  vim.api.nvim_buf_set_lines(buf, 0, -1, false, lines)
  vim.cmd.write()
  await(function() return flow.status() == "active" end, "saved generation restores current analysis")
  await(function() return #calls("project") > during_edit end, "save resumes project warming")
  flow.stop()
  check(flow.status() == "off", "global stop disables the current buffer")
  local stopped = #calls("project")
  vim.wait(800, function() return false end, 10)
  check(#calls("project") == stopped, "global stop cannot restart a background coordinator")
  vim.api.nvim_exec_autocmds("BufEnter", { buffer = buf })
  vim.wait(100, function() return false end, 10)
  check(flow.status() == "off", "global stop remains off on navigation")
  flow.enable()
  await(function() return flow.status() == "active" end, "manual enable recovers after global stop")
  flow.project()
  check(flow.project_status() == nil, "project toggle disables background mode")
  await(function() return flow.status() == "active" end, "foreground remains available with background disabled")

  local context = { root = root, env = {}, command = command }
  local stream_events, complete = {}, nil
  backend.stream(context, { "project" }, { gzip = "gzip", timeout_ms = 8000 },
    function(event) stream_events[#stream_events + 1] = event end,
    function(problem, result) complete = { problem = problem, result = result } end)
  await(function() return complete ~= nil end, "fragmented event stream finishes")
  check(not complete.problem and #stream_events == 7, "all versioned event lines survive chunk boundaries")
end, debug.traceback)
flow.stop()
if not ok then io.stderr:write(err .. "\n"); vim.cmd("cquit 1") end
print(("Flowistry background: %d assertions passed"):format(passed))
vim.cmd("qa!")
