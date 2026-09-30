-- Real saved-buffer layout relocation with rustfmt and an independent editor redraw.
vim.opt.rtp:prepend(vim.fn.getcwd())
local flow = require("flowistry")
local render = require("flowistry.render")
local root = vim.fn.tempname() .. " layout live"
local dir, executable = vim.env.FLOWISTRY_TEST_BACKEND_DIR, vim.env.FLOWISTRY_BACKEND_EXE
local command = executable and { executable } or { assert(dir) .. "/cargo-flowistry", "flowistry" }
local rustfmt = vim.env.FLOWISTRY_TEST_RUSTFMT or "rustfmt"
local passed, observations, notices = 0, {}, {}
vim.notify = function(message) notices[#notices + 1] = message end
local function check(value, message) assert(value, message .. "\n" .. vim.inspect(notices)); passed = passed + 1 end
local function ready()
  check(vim.wait(90000, function() return flow.status() == "active" or flow.status() == "error" end, 20), "analysis timed out")
  check(flow.status() == "active" and not flow.is_stale(), "current result required")
end
local function locate()
  for row, line in ipairs(vim.api.nvim_buf_get_lines(0, 0, -1, false)) do
    local col = line:find("café =", 1, true)
    if col then return row, col - 1 end
  end
  error("selection missing")
end
local function marks()
  local output = {}
  for _, mark in ipairs(vim.api.nvim_buf_get_extmarks(0, render.namespace, 0, -1, { details = true })) do
    output[#output + 1] = { mark[2], mark[3], mark[4].end_row, mark[4].end_col, mark[4].hl_group }
  end
  table.sort(output, function(a, b) return vim.inspect(a) < vim.inspect(b) end)
  return output
end
local ok, err = xpcall(function()
  for _, mode in ipairs({ "SigOnly", "Recurse" }) do
    local project = root .. "/" .. mode
    vim.fn.mkdir(project .. "/src", "p")
    vim.fn.writefile({ '[package]', 'name="layout_editor"', 'version="0.0.0"', 'edition="2021"', '[workspace]' }, project .. "/Cargo.toml")
    local file = project .. "/src/lib.rs"
    vim.fn.writefile({ 'pub fn selected(input: i32) -> i32 {', '    let seed = input + 1;',
      '    let café = seed * 2;', '    café + input', '}' }, file)
    vim.cmd.edit(vim.fn.fnameescape(file)); vim.bo.filetype = "rust"
    local row, col = locate(); vim.api.nvim_win_set_cursor(0, { row, col })
    local opts = { command = command, batch = true, auto_enable = false, context_mode = mode,
      cache_dir = root .. "/cache", debounce_ms = 5, timeout_ms = 90000,
      env = { CARGO_TARGET_DIR = root .. "/target", PATH = (dir and dir .. ":" or "") .. vim.env.PATH } }
    flow.setup(opts); flow.enable(); ready()
    local hook = vim.api.nvim_create_autocmd("BufWritePre", { buffer = 0, callback = function()
      local result = vim.system({ rustfmt, "--edition", "2021", "--emit", "stdout" },
        { stdin = table.concat(vim.api.nvim_buf_get_lines(0, 0, -1, false), "\n") .. "\n", text = true }):wait()
      check(result.code == 0, "rustfmt failed")
      vim.api.nvim_buf_set_lines(0, 0, -1, false, vim.split(result.stdout:gsub("\n$", ""), "\n", { plain = true }))
    end })
    vim.api.nvim_buf_set_lines(0, row - 1, row, false, { "", "      let café = seed * 2;" })
    vim.api.nvim_exec_autocmds("TextChanged", { buffer = 0 })
    check(flow.is_stale(), "edited buffer must mark saved analysis stale")
    local start = (vim.uv or vim.loop).hrtime()
    vim.cmd.write()
    row, col = locate(); vim.api.nvim_win_set_cursor(0, { row, col })
    check(vim.wait(90000, function()
      local stats = flow.cache_status()
      return flow.status() == "active" and not flow.is_stale() and stats and stats.validation == "layout"
    end, 20), "formatted save did not use layout proof: " .. vim.inspect(flow.cache_status()))
    local relocated = marks()
    check(#relocated > 0, "layout redraw produced no highlights")
    observations[#observations + 1] = { mode = mode, loaded_save_ms = ((vim.uv or vim.loop).hrtime() - start) / 1e6 }
    vim.api.nvim_del_autocmd(hook)
    -- Reset editor state and disable every disk reuse path for the independent redraw.
    flow.stop(); opts.cache = false; flow.setup(opts); flow.enable(); ready()
    check(vim.deep_equal(relocated, marks()), "relocated highlight extmarks differ from a fresh compiler result")
    flow.stop()
  end
end, debug.traceback)
flow.stop()
if vim.env.FLOWISTRY_TEST_REPORT then
  vim.fn.writefile({ vim.json.encode({ passed = ok, assertions = passed, observations = observations, error = not ok and err or nil }) }, vim.env.FLOWISTRY_TEST_REPORT)
end
if not ok then io.stderr:write(err .. "\n"); vim.cmd("cquit 1") end
print(("Flowistry live layout reuse: %d assertions passed"):format(passed))
vim.cmd("qa!")
