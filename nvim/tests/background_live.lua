local repo = vim.fn.getcwd()
vim.opt.rtp:prepend(repo)
local flow = require("flowistry")
local dir = assert(vim.env.FLOWISTRY_TEST_BACKEND_DIR, "FLOWISTRY_TEST_BACKEND_DIR is required")
local python = assert(vim.fn.exepath("python3"))
local root = vim.fn.tempname() .. " background live"
local project, calls = root .. "/project", root .. "/compiler-calls.jsonl"
vim.fn.mkdir(project .. "/src", "p")
vim.fn.writefile({ '[package]', 'name="background_fixture"', 'version="0.0.0"', 'edition="2021"', '[workspace]' }, project .. "/Cargo.toml")
local lines = {
  "pub fn first(input: i32) -> i32 {", "    input + 1", "}", "",
  "pub fn second(input: i32) -> i32 {", "    let closure = |value| value * 2;", "    closure(input)", "}", "",
  "pub fn third(input: i32) -> i32 {", "    input - 3", "}",
}
vim.fn.writefile(lines, project .. "/src/lib.rs")
local wrapper = root .. "/compiler.py"
vim.fn.writefile({ "#!" .. python,
  "import json, os, sys",
  "args = sys.argv[1:]",
  "if any(args[i:i+2] == ['--crate-name', 'background_fixture'] for i in range(len(args))):",
  "    with open(" .. vim.json.encode(calls) .. ", 'a') as out: out.write(json.dumps(args) + '\\n')",
  "os.execvp(args[0], args)",
}, wrapper)
vim.fn.setfperm(wrapper, "rwx------")
vim.cmd.edit(vim.fn.fnameescape(project .. "/src/lib.rs")); vim.bo.filetype = "rust"
local buf = vim.api.nvim_get_current_buf()
local notes, passed, observations = {}, 0, {}
vim.notify = function(message) notes[#notes + 1] = message end
local function check(value, message) assert(value, message .. "\n" .. vim.inspect(flow.project_status())); passed = passed + 1 end
local function await(test, message) check(vim.wait(90000, test, 20), message .. "\n" .. vim.inspect(notes)) end
local function compilers() return vim.fn.filereadable(calls) == 1 and #vim.fn.readfile(calls) or 0 end
local function move(row)
  vim.api.nvim_win_set_cursor(0, { row, 13 })
  vim.api.nvim_exec_autocmds("CursorMoved", { buffer = buf })
end
local ok, err = xpcall(function()
  for _, mode in ipairs({ "SigOnly", "Recurse" }) do
    move(1)
    flow.setup({ command = { dir .. "/cargo-flowistry", "flowistry" }, batch = true, batch_max_lines = 1,
      context_mode = mode, debounce_ms = 5, timeout_ms = 90000,
      cache_dir = root .. "/cache", project = { enabled = true, idle_ms = 50, memory_mib = 1024, timeout_seconds = 60 },
      env = { PATH = dir .. ":" .. vim.env.PATH, CARGO_TARGET_DIR = root .. "/target", RUSTC_WRAPPER = wrapper, FLOWISTRY_NO_REPLAY = "1" } })
    await(function() return flow.status() == "active" end, mode .. ": foreground target selection works")
    await(function() local p = flow.project_status(); return p and p.status == "complete" end, mode .. ": actual bounded project completes")
    local status = flow.project_status()
    check(status.total == 4 and status.completed == 4 and not status.error, mode .. ": every function and closure has an outcome")
    -- Allow the final small gzip operation to deliver before measuring navigation.
    vim.wait(100, function() return false end, 10)
    local before = compilers()
    move(10)
    await(function() return flow.status() == "active" end, mode .. ": navigate to a warmed function")
    check(compilers() == before, mode .. ": warmed navigation starts no compiler")
    check(not flow.is_stale(), mode .. ": accepted stream describes current saved bytes")
    observations[#observations + 1] = { mode = mode, compiler_invocations = before, project = status }
    flow.stop()
    check(flow.status() == "off", mode .. ": global stop works with actual workers")
  end
end, debug.traceback)
flow.stop()
if not ok then io.stderr:write(err .. "\n" .. vim.inspect(notes) .. "\n"); vim.cmd("cquit 1") end
if vim.env.FLOWISTRY_TEST_REPORT then
  vim.fn.writefile({ vim.json.encode({ assertions = passed, observations = observations, fixture = root }) }, vim.env.FLOWISTRY_TEST_REPORT)
end
print(("Flowistry live background: %d assertions passed"):format(passed))
vim.cmd("qa!")
