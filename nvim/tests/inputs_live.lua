local repo = vim.fn.getcwd()
vim.opt.rtp:prepend(repo)
local flow = require("flowistry")
local dir = assert(vim.env.FLOWISTRY_TEST_BACKEND_DIR)
local root = vim.fn.tempname() .. " inputs live"
local project = root .. "/project"
vim.fn.mkdir(project .. "/src", "p"); vim.fn.mkdir(root .. "/dependency/src", "p")
vim.fn.mkdir(root .. "/.cargo", "p")
vim.fn.writefile({ '[package]', 'name="input_fixture"', 'version="0.0.0"', 'edition="2021"',
  '[dependencies]', 'input_dependency={path="../dependency"}', '[workspace]' }, project .. "/Cargo.toml")
vim.fn.writefile({ '[package]', 'name="input_dependency"', 'version="0.0.0"', 'edition="2021"' }, root .. "/dependency/Cargo.toml")
vim.fn.writefile({ 'fn main() {', 'println!("cargo:rerun-if-changed=../build.txt");',
  'println!("cargo:rustc-env=INPUT_BUILD={}", std::fs::read_to_string("../build.txt").unwrap().trim());', '}' }, project .. "/build.rs")
vim.fn.writefile({ 'pub fn selected(input: i32) -> i32 {', 'let text = include_str!("../../external.txt");',
  'let value = input + input_dependency::value();', 'value + text.len() as i32 + env!("INPUT_BUILD").len() as i32', '}' }, project .. "/src/lib.rs")
local cases = {
  { root .. "/external.txt", { "before" }, { "changed include bytes" } },
  { root .. "/dependency/src/lib.rs", { 'pub fn value() -> i32 { 1 }' }, { 'pub fn value() -> i32 { 9 }' } },
  { root .. "/build.txt", { "before" }, { "changed build bytes" } },
  { root .. "/.cargo/config.toml", { "# before" }, { "# changed Cargo config" } },
}
for _, case in ipairs(cases) do vim.fn.writefile(case[2], case[1]) end
vim.cmd.edit(vim.fn.fnameescape(project .. "/src/lib.rs")); vim.bo.filetype = "rust"
local buf, passed, notes, observations = vim.api.nvim_get_current_buf(), 0, {}, {}
vim.notify = function(note) notes[#notes + 1] = note end
local function check(value, message) assert(value, message .. "\n" .. vim.inspect(notes)); passed = passed + 1 end
local function await(test, message) check(vim.wait(90000, test, 20), message) end
local ok, err = xpcall(function()
  for _, mode in ipairs({ "SigOnly", "Recurse" }) do
    vim.api.nvim_set_current_buf(buf); vim.api.nvim_win_set_cursor(0, { 3, 13 })
    flow.setup({ command = { dir .. "/cargo-flowistry", "flowistry" }, batch = true,
      context_mode = mode, debounce_ms = 5, timeout_ms = 90000, cache_dir = root .. "/cache",
      project = { enabled = mode == "Recurse", idle_ms = 50, memory_mib = 1024, timeout_seconds = 60 },
      env = { PATH = dir .. ":" .. vim.env.PATH, CARGO_TARGET_DIR = root .. "/target" } })
    await(function() return flow.status() == "active" end, mode .. ": initial result")
    -- An external build input may require the backend's validated second pass.
    flow.refresh()
    await(function() return flow.status() == "active" end, mode .. ": validated watch paths")
    local unrelated = root .. "/unrelated-" .. mode .. ".txt"
    vim.fn.writefile({ "unrelated" }, unrelated)
    local unrelated_buf = vim.fn.bufadd(unrelated); vim.fn.bufload(unrelated_buf)
    vim.api.nvim_buf_set_lines(unrelated_buf, 0, -1, false, { "unrelated edit" })
    vim.api.nvim_exec_autocmds("TextChanged", { buffer = unrelated_buf })
    vim.wait(100, function() return false end, 10)
    check(flow.status() == "active" and not flow.is_stale(), mode .. ": actual metadata avoids unrelated-file invalidation")
    vim.api.nvim_buf_call(unrelated_buf, function() vim.cmd.write() end)
    for _, case in ipairs(cases) do
      local dependency = vim.fn.bufadd(case[1]); vim.fn.bufload(dependency)
      local changed = mode == "SigOnly" and case[3] or case[2]
      vim.api.nvim_buf_set_lines(dependency, 0, -1, false, changed)
      vim.api.nvim_exec_autocmds("TextChanged", { buffer = dependency })
      await(function() return flow.status() == "waiting for save" end, mode .. ": external input pauses " .. case[1])
      check(flow.is_stale(), mode .. ": saved highlighting is stale")
      local start = (vim.uv or vim.loop).hrtime()
      vim.api.nvim_buf_call(dependency, function() vim.cmd.write() end)
      await(function() return flow.status() == "active" and not flow.is_stale() end, mode .. ": save revalidates " .. case[1])
      observations[#observations + 1] = { mode = mode, input = case[1], loaded_save_ms = ((vim.uv or vim.loop).hrtime() - start) / 1e6 }
    end
    if mode == "Recurse" then
      await(function() local status = flow.project_status(); return status and status.status == "complete" end,
        "background resumes after external dependency saves")
    end
    flow.stop()
  end
end, debug.traceback)
flow.stop()
if vim.env.FLOWISTRY_TEST_REPORT then vim.fn.writefile({ vim.json.encode({ passed = ok, assertions = passed,
  observations = observations, error = not ok and err or nil }) }, vim.env.FLOWISTRY_TEST_REPORT) end
if not ok then io.stderr:write(err .. "\n"); vim.cmd("cquit 1") end
print(("Flowistry live input invalidation: %d assertions passed"):format(passed))
vim.cmd("qa!")
