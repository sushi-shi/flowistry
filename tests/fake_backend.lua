-- A protocol fixture process, deliberately independent of the plugin's Lua code.
local action = arg[1]
-- File indices are local to each analysis invocation, not reusable identities.
local filename = action == "spans" and 7 or 12
if vim.env.FLOWISTRY_TEST_LOG then
  local argv = {}
  for i, value in ipairs(arg) do argv[i] = value end
  vim.fn.writefile({ vim.json.encode(argv) }, vim.env.FLOWISTRY_TEST_LOG, "a")
end
if vim.env.FLOWISTRY_TEST_DELAY then (vim.uv or vim.loop).sleep(tonumber(vim.env.FLOWISTRY_TEST_DELAY)) end
if vim.env.FLOWISTRY_TEST_MODE == "exit" then io.stderr:write("fixture compiler failure\n"); os.exit(1) end
if vim.env.FLOWISTRY_TEST_MODE == "diagnostics" then io.stderr:write("error: expected Rust expression\n"); return end
if vim.env.FLOWISTRY_TEST_MODE == "base64" then io.write("this is not base64!"); return end
if vim.env.FLOWISTRY_TEST_MODE == "gzip" then io.write(vim.base64.encode("not gzip")); return end
local function range(row, start, finish)
  return { filename = filename, start = { line = row, column = start }, ["end"] = { line = row, column = finish } }
end
local function body(start, finish)
  return { filename = filename, start = { line = start, column = 0 }, ["end"] = { line = finish, column = 1 } }
end
local x, use_x = range(1, 8, 9), range(3, 19, 20)
local y, use_y = range(2, 8, 9), range(4, 19, 20)
local result
if vim.env.FLOWISTRY_TEST_MODE == "error" then
  result = { Err = { type = "AnalysisError", error = "fixture analysis failed" } }
elseif vim.env.FLOWISTRY_TEST_MODE == "schema" then
  result = { Ok = { spans = "wrong shape" } }
elseif action == "spans" then
  result = { Ok = { spans = { body(0, 5), body(7, 11), body(8, 10) } } }
elseif action == "file-focus" then
  local z = range(9, 12, 13)
  local requested = tonumber(arg[3])
  result = { Ok = { bodies = {
    { range = body(0, 5), focus = requested and requested >= 7 and vim.NIL or { Ok = { containers = { body(0, 5) }, place_info = {
      { range = x, ranges = { x }, slice = { range(1, 4, 14), range(3, 4, 23) }, direct_influence = { use_x } },
    } } } },
    { range = body(7, 11), focus = { Err = "unsupported body fixture" } },
    { range = body(8, 10), focus = requested and requested < 8 and vim.NIL or { Ok = { containers = { body(8, 10) }, place_info = {
      { range = z, ranges = { z }, slice = { range(9, 8, 18) }, direct_influence = {} },
    } } } },
  } } }
elseif action == "focus" and tonumber(arg[3]) >= 7 then
  local z = range(9, 12, 13)
  result = { Ok = { containers = { body(8, 10) }, place_info = {
    { range = z, ranges = { z }, slice = { range(9, 8, 18) }, direct_influence = {} },
  } } }
elseif action == "focus" then
  result = { Ok = { containers = { body(0, 5) }, place_info = {
    { range = x, ranges = { x }, slice = { range(1, 4, 14), range(3, 4, 23) }, direct_influence = { use_x } },
    { range = use_x, ranges = { use_x }, slice = { range(1, 4, 14), range(3, 4, 23) }, direct_influence = { x } },
    { range = y, ranges = { y }, slice = { range(2, 4, 14), range(4, 4, 23) }, direct_influence = { use_y } },
  } } }
else
  io.stderr:write("unexpected fixture command: " .. tostring(action)); os.exit(1)
end
local encoded = vim.json.encode(result)
if vim.env.FLOWISTRY_TEST_MODE == "json" then encoded = "not json" end
local gzip = vim.system({ "gzip", "-c" }, { stdin = encoded }):wait()
assert(gzip.code == 0, gzip.stderr)
io.write(vim.base64.encode(gzip.stdout))
