local repo = vim.fn.getcwd()
vim.opt.rtp:prepend(repo)
local flow = require("flowistry")
local root = vim.fn.tempname() .. " inputs é"
local project = root .. "/project"
vim.fn.mkdir(project .. "/src", "p")
vim.fn.mkdir(root .. "/.cargo", "p")
vim.fn.writefile({ '[package]', 'name="fixture"', 'version="0.0.0"', 'edition="2021"' }, project .. "/Cargo.toml")
local source = { "fn main() {", "    let x = 1;", "    let y = 2;", '    println!("{}", x); ',
  '    println!("{}", y); ', "}", "", "fn outer() {", "    fn inner() {", "        let z = 3;", "    }", "}" }
vim.fn.writefile(source, project .. "/src/main.rs")
local files = { project .. "/included.txt", project .. "/build-input.bin", root .. "/external.txt", root .. "/.cargo/config.toml" }
for _, file in ipairs(files) do vim.fn.writefile({ "before" }, file) end
vim.cmd.edit(vim.fn.fnameescape(project .. "/src/main.rs")); vim.bo.filetype = "rust"
local buf, passed, notes = vim.api.nvim_get_current_buf(), 0, {}
vim.notify = function(note) notes[#notes + 1] = note end
local log = root .. "/calls.jsonl"
local function check(value, message) assert(value, message .. "\n" .. vim.inspect(notes)); passed = passed + 1 end
local function await(test, message) check(vim.wait(8000, test, 10), message) end
local function calls()
  local count = 0
  for _, line in ipairs(vim.fn.filereadable(log) == 1 and vim.fn.readfile(log) or {}) do
    if vim.json.decode(line)[1] == "file-focus" then count = count + 1 end
  end
  return count
end
local command = { vim.v.progpath, "--headless", "-u", "NONE", "-l", repo .. "/tests/fake_backend.lua" }
local function setup(extra)
  flow.setup({ command = command, batch = true, auto_enable = false, debounce_ms = 5,
    env = vim.tbl_extend("force", { FLOWISTRY_TEST_LOG = log, FLOWISTRY_TEST_INPUT_FILES = vim.json.encode(files) }, extra or {}) })
  vim.api.nvim_win_set_cursor(0, { 2, 8 }); flow.enable()
end
local function load(file)
  local dependency = vim.fn.bufadd(file); vim.fn.bufload(dependency)
  return dependency
end
local function edit(dependency)
  vim.api.nvim_buf_set_lines(dependency, 0, -1, false, { "after" })
  vim.api.nvim_exec_autocmds("TextChanged", { buffer = dependency })
end
local function save(dependency)
  vim.api.nvim_buf_call(dependency, function() vim.cmd.write() end)
end
local ok, err = xpcall(function()
  setup()
  await(function() return flow.status() == "active" end, "initial watched publication becomes active")
  for _, file in ipairs(files) do
    local dependency, before = load(file), calls()
    edit(dependency)
    await(function() return flow.status() == "waiting for save" end, file .. ": dirty input pauses analysis")
    check(flow.is_stale(), file .. ": retained highlights are explicitly stale")
    vim.wait(100, function() return false end, 10)
    check(calls() == before, file .. ": dirty input does not launch repeated workers")
    save(dependency)
    await(function() return flow.status() == "active" and calls() > before end, file .. ": save revalidates")
    check(not flow.is_stale(), file .. ": current response clears stale state")
  end
  local unrelated = root .. "/unrelated.txt"
  vim.fn.writefile({ "unrelated" }, unrelated)
  local other, before = load(unrelated), calls()
  edit(other)
  vim.wait(100, function() return false end, 10)
  check(flow.status() == "active" and not flow.is_stale() and calls() == before,
    "unrelated external buffer does not invalidate known inputs")
  save(other)
  before = calls()
  vim.api.nvim_exec_autocmds("FocusGained", {})
  await(function() return flow.status() == "active" and calls() > before end, "external focus return revalidates")

  -- Input is dirty before the first compiler result reveals its path.
  flow.stop()
  local external = load(files[3]); edit(external)
  setup()
  await(function() return flow.status() == "waiting for save" end, "newly discovered dirty external input suppresses delivery")
  save(external)
  await(function() return flow.status() == "active" end, "discovered input save resumes analysis")

  before = calls()
  setup({ FLOWISTRY_TEST_DELAY = "250" })
  await(function() return calls() > before end, "first watch discovery is in flight")
  edit(external); save(external)
  await(function() return flow.status() == "active" and calls() >= before + 2 end,
    "edit during first discovery rejects the obsolete completion and retries")

  setup({ FLOWISTRY_TEST_UNKNOWN_INPUTS = "1" })
  await(function() return flow.status() == "active" end, "unsupported watch metadata still allows saved analysis")
  edit(other)
  await(function() return flow.status() == "waiting for save" end, "unknown watch metadata conservatively tracks external edits")
  save(other)
  await(function() return flow.status() == "active" end, "unknown-input fallback recovers after save")

  local watch = require("flowistry.inputs").new()
  watch:observe(project, false, "a")
  watch:observe(project, { schema = 1, roots = {}, files = {} }, "b")
  check(watch:matches(project, unrelated), "another scope cannot clear an unknown input set")
  watch:observe(project, { schema = 1, roots = {}, files = { files[3] } }, "a")
  check(not watch:matches(project, unrelated) and watch:matches(project, files[3]), "validated scope recovers precise watching")
  local huge = {}; for i = 1, 8200 do huge[i] = root .. "/file" .. i end
  watch:observe(project, { schema = 1, roots = {}, files = huge })
  check(watch:matches(project, "/outside/unlisted"), "watch budget overflow falls back conservatively")
end, debug.traceback)
flow.stop()
if not ok then io.stderr:write(err .. "\n"); vim.cmd("cquit 1") end
print(("Flowistry input invalidation: %d assertions passed"):format(passed))
vim.cmd("qa!")
