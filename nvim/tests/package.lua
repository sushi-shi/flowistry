-- Exercise the installed plugin and launcher, without a user-supplied backend.
local expected = assert(vim.env.FLOWISTRY_EXPECTED_BACKEND)
local plugin = assert(vim.env.FLOWISTRY_TEST_PLUGIN)
local session = vim.env.FLOWISTRY_TEST_SESSION == "1"
if not session then vim.opt.rtp:prepend(plugin) end
local flow = require("flowistry")
local temp = vim.fn.tempname() .. " packaged flowistry"
local native_system, calls = vim.system, {}
vim.system = function(command, options, callback)
  calls[#calls + 1] = vim.deepcopy(command)
  return native_system(command, options, callback)
end
local function run()
  local packaged = require("flowistry.packaged")
  assert(packaged.command[1] == expected, "plugin and backend are from different builds")
  assert(packaged.batch, "packaged backend's file-focus support is missing")
  assert(packaged.follow_calls, "packaged backend's call-following pins are not enabled")
  local module = debug.getinfo(flow.setup, "S").source
  assert(module:find(plugin .. "/lua/flowistry/init.lua", 1, true), "test loaded source instead of installed plugin")
  vim.fn.mkdir(temp .. "/src", "p")
  vim.fn.writefile({ '[package]', 'name="packaged_flowistry"', 'version="0.1.0"', 'edition="2021"' }, temp .. "/Cargo.toml")
  local file = temp .. "/src/main.rs"
  vim.fn.writefile({ "fn main() {", "    let selected = 1;", "    let unrelated = 2;",
    "    println!(\"{}\", selected);", "}" }, file)
  if not session then flow.setup({ auto_enable = false, debounce_ms = 1 }) end
  vim.cmd.edit(vim.fn.fnameescape(file))
  vim.api.nvim_win_set_cursor(0, { 2, 8 })
  flow.enable()
  assert(vim.wait(300000, function() return flow.status() == "active" or flow.status() == "error" end, 20), "analysis timeout")
  if flow.status() == "error" then
    flow.log()
    error(table.concat(vim.api.nvim_buf_get_lines(0, 0, -1, false), "\n"))
  end
  assert(flow.status() == "active", flow.status())
  local requested = false
  for _, command in ipairs(calls) do
    if vim.tbl_contains(command, "file-focus") then
      assert(command[1] == expected, "analysis used a different backend")
      requested = true
    end
  end
  assert(requested, "no paired backend request observed")
  local groups = {}
  for _, mark in ipairs(vim.api.nvim_buf_get_extmarks(0, require("flowistry.render").namespace, 0, -1, { details = true })) do
    groups[mark[4].hl_group] = true
  end
  assert(groups.FlowistryFocus and groups.FlowistryDim, "missing compiler-derived highlights")
  flow.mark()
  assert(vim.wait(300000, function() return flow.status() == "pinned" or flow.status() == "analysis unavailable" end, 20), "pin timeout")
  assert(flow.status() == "pinned", flow.indicator())
  assert(vim.iter(calls):any(function(command)
    return command[1] == expected and vim.tbl_contains(command, "pin-focus")
  end), "default pin did not use the paired cross-function backend")
end
vim.schedule(function()
  local ok, err = xpcall(run, debug.traceback)
  vim.system = native_system
  flow.disable()
  vim.fn.delete(temp, "rf")
  if not ok then io.stderr:write(err .. "\n"); vim.cmd("cquit 1") end
  print(session and "Packaged launcher uses its matching backend" or "Installed plugin defaults to its matching backend")
  vim.cmd("qa!")
end)
