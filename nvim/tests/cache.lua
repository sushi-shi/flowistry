-- Real backend cache lifecycle, independent of the user's project and cache.
vim.opt.rtp:prepend(vim.fn.getcwd())
local flow = require("flowistry")
local backend = require("flowistry.backend")
local temp = vim.fn.tempname() .. " flowistry cache"
local source = {
  "struct State { a: i32, b: i32 }",
  "impl State { fn update(&mut self, value: i32) { self.b = value; } }",
  "fn unrelated() { let x = 1; }",
  "fn main() {",
  " let mut state = State { a: 1, b: 2 };",
  " state.update(17);",
  " let untouched = state.a;",
  " let affected = state.b;",
  "}",
}
local passed, requests = 0, 0
local original = backend.request
backend.request = function(...) requests = requests + 1; return original(...) end
local function check(value, message) assert(value, message); passed = passed + 1 end
local function ready(expected)
  check(vim.wait(300000, function() return flow.status() == expected or flow.status() == "error" end, 20), "timed out")
  if flow.status() == "error" then flow.log(); error(table.concat(vim.api.nvim_buf_get_lines(0, 0, -1, false), "\n")) end
  check(flow.status() == expected, "unexpected state: " .. flow.indicator())
end
local function request_done(expected, count)
  check(vim.wait(300000, function() return requests > count end, 20), "expected backend request")
  ready(expected)
end
local function hit(expected)
  local stats = assert(flow.cache_status(), "backend did not report cache statistics")
  check(stats.hits == (expected and 1 or 0), "unexpected cache reuse: " .. vim.inspect(stats))
end
local function run()
  vim.fn.mkdir(temp .. "/src", "p")
  vim.fn.writefile({ '[package]', 'name="editor_cache_test"', 'version="0.1.0"', 'edition="2021"' }, temp .. "/Cargo.toml")
  local file = temp .. "/src/main.rs"
  vim.fn.writefile(source, file)
  local opts = { command = { assert(vim.env.FLOWISTRY_BACKEND_EXE) }, root = temp,
    cache_dir = temp .. "/cache", context_mode = "Recurse", auto_enable = false,
    batch = true, batch_max_lines = 0, debounce_ms = 1, timeout_ms = 300000 }
  flow.setup(opts)
  vim.cmd.edit(vim.fn.fnameescape(file))
  vim.bo.filetype = "rust"
  vim.api.nvim_win_set_cursor(0, { 7, 6 })
  flow.enable(); ready("active"); hit(false)
  local before = requests
  flow.mark()
  vim.cmd.write()
  ready("pinned")
  vim.wait(100, function() return false end)
  check(requests == before, "unchanged save must not invoke the compiler")
  flow.disable()
  flow.enable(); ready("active")
  check(requests == before, "off/on retains memory analysis without a compiler request")
  -- Reset all Lua state, as a new editor would; results remain on disk.
  flow.setup(opts)
  flow.enable(); ready("active"); hit(true)
  check(flow.cache_status().validation == "snapshot", "editor restart must reuse disk analysis without compiler startup")
  check(flow.indicator():find("disk cache", 1, true), "cache hit is observable")
  flow.mark()
  before = requests
  vim.api.nvim_buf_set_lines(0, 0, 0, false, { "", "// é🦀" })
  vim.cmd.write()
  request_done("pinned", before); hit(true)
  local focus = false
  for _, mark in ipairs(vim.api.nvim_buf_get_extmarks(0, require("flowistry.render").namespace, 0, -1, { details = true })) do
    if mark[4].hl_group == "FlowistryFocus" and mark[2] == 8 then focus = true end
  end
  check(focus, "cache hit must relocate focus to the moved pinned variable")
  before = requests
  vim.api.nvim_buf_set_lines(0, 4, 5, false, { "fn unrelated() { let x = 8; let more = x + 1; }" })
  vim.cmd.write()
  request_done("pinned", before); hit(true)
  before = requests
  vim.api.nvim_buf_set_lines(0, 3, 4, false, { "impl State { fn update(&mut self, value: i32) { self.a = value; } }" })
  vim.cmd.write()
  request_done("pinned", before); hit(false)
  flow.unmark()
  vim.api.nvim_win_set_cursor(0, { 9, 6 })
  before = requests
  flow.refresh()
  request_done("active", before); hit(false)
  before = requests
  flow.disable(); flow.enable(); ready("active")
  check(requests == before, "off/on retains refreshed analysis")
  flow.disable()
  before = requests
  vim.api.nvim_buf_set_lines(0, 3, 4, false, { source[2] })
  vim.cmd.write()
  flow.enable()
  request_done("active", before); hit(true)
  check(requests > before, "edits while disabled must invalidate retained memory analysis")
  opts.cache = false
  flow.setup(opts); flow.enable(); ready("active"); hit(false)
  flow.setup(opts); flow.enable(); ready("active"); hit(false)
end
local ok, err = xpcall(run, debug.traceback)
flow.disable()
backend.request = original
vim.fn.delete(temp, "rf")
if not ok then io.stderr:write(err .. "\n"); vim.cmd("cquit 1") end
print(("Passed %d persistent-cache editor assertions"):format(passed))
vim.cmd("qa!")
