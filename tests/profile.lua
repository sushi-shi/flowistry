-- Real-backend latency probe: FILE ROW COLUMN, with FLOWISTRY_BACKEND_EXE set.
vim.opt.rtp:prepend(vim.fn.getcwd())
local uv = vim.uv or vim.loop
local backend = require("flowistry.backend")
local original = backend.request
backend.request = function(context, args, config, callback)
  local started = uv.hrtime()
  return original(context, args, config, function(err, value)
    print(("%s: %.1f ms"):format(args[1], (uv.hrtime() - started) / 1e6))
    callback(err, value)
  end)
end
local flow = require("flowistry")
local exe = assert(vim.env.FLOWISTRY_BACKEND_EXE)
flow.setup({ command = { exe }, timeout_ms = 300000, batch = vim.env.FLOWISTRY_BATCH == "1" })
vim.cmd.edit(vim.fn.fnameescape(assert(arg[1])))
vim.api.nvim_win_set_cursor(0, { tonumber(arg[2]), tonumber(arg[3]) - 1 })
local started = uv.hrtime()
flow.enable()
assert(vim.wait(300000, function() return flow.status() == "active" or flow.status() == "error" end, 10), "timed out")
if flow.status() == "error" then flow.log(); error(table.concat(vim.api.nvim_buf_get_lines(0, 0, -1, false), "\n")) end
print(("initial focus: %.1f ms"):format((uv.hrtime() - started) / 1e6))
local render = require("flowistry.render")
local show = render.show
local drawn = false
render.show = function(...)
  local start = uv.hrtime()
  local result = show(...)
  print(("render: %.2f ms"):format((uv.hrtime() - start) / 1e6))
  drawn = true
  return result
end
started = uv.hrtime()
vim.api.nvim_exec_autocmds("CursorMoved", { buffer = 0 })
assert(vim.wait(5000, function() return drawn end, 1))
print(("cached cursor event: %.1f ms"):format((uv.hrtime() - started) / 1e6))
flow.disable()
vim.cmd("qa!")
