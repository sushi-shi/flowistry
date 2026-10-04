-- Control callback order explicitly: a cancelled pin must never replace a new
-- pin or switch a callee back from cursor tracking to an obsolete slice.
vim.opt.rtp:prepend(vim.fn.getcwd())
local flow = require("flowistry")
local backend = require("flowistry.backend")
local render = require("flowistry.render")
local temp = vim.fn.tempname() .. " pin state"
vim.fn.mkdir(temp .. "/src", "p")
vim.fn.writefile({ '[package]', 'name="fixture"', 'version="0.1.0"' }, temp .. "/Cargo.toml")
local main = "fn root(x: i32, y: i32) { child(x); }"
local child = "fn child(input: i32) { let used = input; }"
local origin, callee = temp .. "/src/main.rs", temp .. "/src/child.rs"
vim.fn.writefile({ main }, origin)
vim.fn.writefile({ child }, callee)
local passed, pending = 0, {}
local function check(value, message) assert(value, message); passed = passed + 1 end
local function await(status)
  check(vim.wait(2000, function() return flow.status() == status end, 5), status .. ": " .. flow.indicator())
end
local function span(id, first, last)
  return { filename = id, start = { line = 0, column = first }, ["end"] = { line = 0, column = last } }
end
local function focus(file)
  local line = file == origin and main or child
  local places = {}
  for word in line:gmatch("%w+") do
    local start = line:find(word, 1, true) - 1
    local range = span(file, start, start + #word)
    places[#places + 1] = { range = range, ranges = { range }, slice = { range },
      pre_slice = { range }, post_slice = { range }, direct_influence = {} }
  end
  return { bodies = { { range = span(file, 0, #line), focus = { Ok = {
    containers = { span(file, 0, #line) }, place_info = places } } } } }
end
local function pinned()
  local function body(id, line)
    return { range = span(id, 0, #line), containers = { span(id, 0, #line) }, comments = {},
      pre_slice = {}, post_slice = { span(id, 8, 9) }, maybe_pre_slice = {}, maybe_post_slice = {} }
  end
  return { schema = 1, files = { ["10"] = { path = origin, text = main .. "\n" },
    ["20"] = { path = callee, text = child .. "\n" } }, bodies = { body(10, main), body(20, child) } }
end
local context, request = backend.context, backend.request
backend.context = function(root, _, callback)
  local op = { cancel = function() end }
  vim.schedule(function() callback(nil, { root = root }) end)
  return op
end
backend.request = function(_, args, _, callback)
  local op = { cancel = function(self) self.cancelled = true end, receive = callback }
  if args[1] == "pin-focus" then pending[#pending + 1] = op
  else vim.schedule(function() if not op.cancelled then callback(nil, focus(args[2])) end end) end
  return op
end
local function select(buf, column)
  vim.api.nvim_set_current_buf(buf)
  vim.api.nvim_win_set_cursor(0, { 1, column })
  vim.api.nvim_exec_autocmds("CursorMoved", { buffer = buf })
end
local function run()
  vim.cmd.edit(vim.fn.fnameescape(origin))
  vim.bo.filetype = "rust"
  local root = vim.api.nvim_get_current_buf()
  flow.setup({ auto_enable = false, follow_calls = true, batch = true, debounce_ms = 1 })
  select(root, 8)
  flow.enable()
  await("active")
  flow.mark()
  check(#pending == 1, "one pending origin request")
  vim.cmd.edit(vim.fn.fnameescape(callee))
  vim.bo.filetype = "rust"
  local other = vim.api.nvim_get_current_buf()
  select(other, 27)
  flow.enable()
  await("following calls")
  flow.unmark()
  await("active")
  check(pending[1].cancelled, "unpin in a callee cancels the origin request")
  pending[1].receive(nil, pinned())
  check(flow.status() == "active", "late cancelled result cannot restore a pin")
  select(root, 8)
  flow.mark()
  select(root, 16)
  flow.mark()
  check(#pending == 3 and pending[2].cancelled, "moving a pin cancels its old request")
  pending[2].receive(nil, pinned())
  check(flow.status() ~= "pinned", "old pin cannot satisfy a newer request")
  pending[3].receive(nil, pinned())
  await("pinned")
  select(other, 27)
  await("pinned calls")
  check(#vim.api.nvim_buf_get_extmarks(root, require("flowistry.pin").namespace, 0, -1, {}) == 1,
    "exactly one origin marker survives repinning")
  flow.unmark()
  await("active")
  select(root, 8)
  flow.mark()
  select(other, 27)
  pending[4].receive("deliberate compiler failure")
  await("analysis unavailable")
  check(#vim.api.nvim_buf_get_extmarks(other, render.namespace, 0, -1, {}) == 0,
    "a failure in the origin never leaves an unrelated cursor slice in the callee")
  flow.unmark()
  await("active")
  select(root, 8)
  await("active")
  check(flow.status() == "active", "a failed pin does not poison ordinary cursor analysis")
  select(other, 27)
  await("active")
  -- Validate saved text, not just path and line counts, before converting ranges.
  local calls = require("flowistry.call_slice")
  local value = pinned()
  value.files["20"].text = child:gsub("input", "other") .. "\n"
  local _, err = calls.prepare(value, temp):buffer(other)
  check(err ~= nil, "same-sized but different saved source is rejected")
  value = pinned()
  value.bodies[2].post_slice[1].filename = 999
  check(not pcall(function() calls.prepare(value, temp):buffer(other) end), "unknown filename identity is rejected")
  -- Re-enable the failed origin, then unload it while its request is pending.
  select(root, 8)
  flow.disable()
  flow.enable()
  await("active")
  flow.mark()
  local last = pending[#pending]
  select(other, 27)
  vim.api.nvim_buf_delete(root, { force = true })
  await("active")
  check(last.cancelled, "unloading the origin cancels the pin and resumes cursor focus")
  last.receive(nil, pinned())
  check(flow.status() == "active", "an unloaded origin cannot publish a late result")
end
local ok, err = xpcall(run, debug.traceback)
flow.stop()
backend.context, backend.request = context, request
vim.fn.delete(temp, "rf")
if not ok then io.stderr:write(err .. "\n"); vim.cmd("cquit 1") end
print(("Passed %d pin lifecycle assertions"):format(passed))
vim.cmd("qa!")
