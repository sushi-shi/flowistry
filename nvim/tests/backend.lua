-- Compare combined analysis with the same backend's per-function result.
-- Usage: FLOWISTRY_BACKEND_EXE=... nvim --headless -u NONE -l tests/backend.lua FILE ROW COL
vim.opt.rtp:prepend(vim.fn.getcwd())
local backend = require("flowistry.backend")
local file = vim.fn.fnamemodify(assert(arg[1]), ":p")
local line, column = tonumber(arg[2]) - 1, tonumber(arg[3]) - 1
local context = { root = vim.fs.root(file, "Cargo.toml"), env = { RUST_LOG = "rustc_utils::timer=info" }, command = { vim.env.FLOWISTRY_BACKEND_EXE } }
local native_system, collections = vim.system, {}
vim.system = function(argv, opts, callback)
  if argv[1] ~= context.command[1] then return native_system(argv, opts, callback) end
  return native_system(argv, opts, function(result)
    local _, count = (result.stderr or ""):gsub("get_bodies_with_borrowck_facts for", "")
    collections[argv[2]] = count
    callback(result)
  end)
end
local function request(args)
  local done, value, err = false
  backend.request(context, args, { timeout_ms = 300000, gzip = "gzip" }, function(e, v)
    err, value, done = e, v, true
  end)
  assert(vim.wait(300000, function() return done end, 20), "backend timed out")
  assert(not err, err)
  return value
end
-- Compiler hash maps can reorder spans. Compare canonical sets and ignore
-- per-invocation file IDs for this single-file test.
local function canonical(value)
  if type(value) ~= "table" then return vim.json.encode(value) end
  local parts = {}
  if vim.islist(value) then
    for _, item in ipairs(value) do parts[#parts + 1] = canonical(item) end
  else
    for key, item in pairs(value) do
      if key ~= "filename" then parts[#parts + 1] = key .. ":" .. canonical(item) end
    end
  end
  table.sort(parts)
  return "{" .. table.concat(parts, ",") .. "}"
end
local batch = request({ "file-focus", file, tostring(line), tostring(column) })
local selected, count = nil, 0
for _, body in ipairs(batch.bodies) do
  if body.focus ~= vim.NIL then
    assert(body.focus.Ok, vim.inspect(body.focus))
    selected, count = body.focus.Ok, count + 1
  end
end
assert(count == 1, "position-specific batch must analyze exactly one body")
local per_function = request({ "focus", file, tostring(line), tostring(column) })
assert(canonical(selected) == canonical(per_function), "combined analysis differs from per-function focus")
if #batch.bodies > 2 then
  assert(collections["file-focus"] < collections.focus, "scoped request still collects facts for unrelated bodies")
end
vim.system = native_system
print(("Combined analysis matches per-function focus; %d bodies discovered; facts collected for %d vs %d bodies")
  :format(#batch.bodies, collections["file-focus"], collections.focus))
vim.cmd("qa!")
