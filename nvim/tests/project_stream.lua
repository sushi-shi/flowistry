vim.opt.rtp:prepend(vim.fn.getcwd())
local backend = require("flowistry.backend")
local native_system = vim.system
local passed = 0
local function check(value, message) assert(value, message); passed = passed + 1 end
local function await(test, message) check(vim.wait(4000, test, 5), message) end
local function event(sequence, kind, fields)
  return vim.tbl_extend("force", { schema = 1, sequence = sequence, run = "test", event = kind }, fields or {})
end
local function encoded(value) return vim.json.encode(value) .. "\n" end
local cases = {
  { name = "wrong schema", encoded(event(0, "started", { schema = 2 })) },
  { name = "missing start", encoded(event(0, "body")) },
  { name = "sequence gap", encoded(event(0, "started")) .. encoded(event(2, "inventory")) },
  { name = "mixed runs", encoded(event(0, "started")) .. encoded(event(1, "inventory", { run = "other" })) },
  { name = "unknown event", encoded(event(0, "started")) .. encoded(event(1, "future")) },
  { name = "unknown status", encoded(event(0, "started")) .. encoded(event(1, "finished", { status = "future" })) },
  { name = "truncated JSON", encoded(event(0, "started")) .. '{"schema":1' },
  { name = "missing completion", encoded(event(0, "started")) },
  { name = "event after completion", encoded(event(0, "started")) .. encoded(event(1, "finished", { status = "complete" })) .. encoded(event(2, "body")) },
}
local ok, err = xpcall(function()
  for _, case in ipairs(cases) do
    local done
    vim.system = function(_, opts, callback)
      vim.schedule(function() opts.stdout(nil, case[1]); callback({ code = 0 }) end)
      return { kill = function() end }
    end
    backend.stream({ command = { "fixture" }, root = "/tmp", env = {} }, { "project" }, {}, function() end,
      function(problem) done = problem or false end)
    await(function() return done ~= nil end, case.name .. " completes")
    check(type(done) == "string", case.name .. " is rejected")
  end
  vim.system = native_system
  local compressed = vim.system({ "gzip", "-c" }, {
    stdin = vim.json.encode({ Ok = { large = string.rep("x", 2 * 1024 * 1024) } }),
  }):wait()
  check(compressed.code == 0, "prepare highly compressed output")
  local done
  backend.decode(vim.base64.encode(compressed.stdout), { gzip = "gzip", timeout_ms = 4000, decode_limit = 1024 },
    function(problem, value) done = { problem, value } end)
  await(function() return done ~= nil end, "bounded decode completes")
  check(done[1] and done[1]:find("decode budget", 1, true) and done[2] == nil, "large expansion is rejected before JSON allocation")
end, debug.traceback)
vim.system = native_system
if not ok then io.stderr:write(err .. "\n"); vim.cmd("cquit 1") end
print(("Flowistry project stream: %d assertions passed"):format(passed))
vim.cmd("qa!")
