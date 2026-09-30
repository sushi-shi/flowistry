local root = vim.fn.getcwd()
local uv = vim.uv or vim.loop
local args = {}
for i, value in ipairs(arg) do args[i] = value end
local commands = { ["project-targets"] = true, project = true, ["file-focus"] = true, focus = true, spans = true }
local index
for i, value in ipairs(args) do if commands[value] then index = i; break end end
assert(index, "missing fixture command")
local action = args[index]
local function log(value)
  if vim.env.FLOWISTRY_TEST_PROJECT_LOG then vim.fn.writefile({ vim.json.encode(value) }, vim.env.FLOWISTRY_TEST_PROJECT_LOG, "a") end
end
log({ action = action, args = args, pid = vim.fn.getpid() })
local function encoded(value)
  local process = vim.system({ "gzip", "-c" }, { stdin = vim.json.encode(value) }):wait()
  assert(process.code == 0, process.stderr)
  return vim.base64.encode(process.stdout)
end
if action == "project-targets" then
  io.write(encoded({ Ok = { schema = 1, workspace_root = root, targets = {
    { package = "fixture", target_kind = "bin", target_name = "fixture", manifest_path = root .. "/Cargo.toml",
      src_path = root .. "/src/main.rs", required_features = {}, supported = true },
  } } }))
elseif action ~= "project" then
  local rest = {}
  for i = index, #args do rest[#rest + 1] = args[i] end
  arg = rest
  dofile(vim.fs.dirname(debug.getinfo(1, "S").source:sub(2)) .. "/fake_backend.lua")
else
  local sequence = 0
  local run = "fixture-" .. vim.fn.getpid()
  local function emit(value)
    value.schema, value.run, value.sequence = 1, run, sequence
    sequence = sequence + 1
    local line = vim.json.encode(value) .. "\n"
    -- Exercise arbitrary chunk boundaries, including in UTF-8 source paths.
    local cut = math.floor(#line / 2)
    io.write(line:sub(1, cut)); io.flush()
    uv.sleep(2)
    io.write(line:sub(cut + 1)); io.flush()
  end
  emit({ event = "started" })
  emit({ event = "inventory", total = 2 })
  local filename = root .. "/src/main.rs"
  for _, row in ipairs({ 0, 8 }) do
    local body = { identity = "fixture-" .. row, name = "fixture_" .. row,
      range = { filename = filename, start = { line = row, column = 0 }, ["end"] = { line = row == 0 and 5 or 10, column = 1 } } }
    emit({ event = "body-started", body = body })
    if vim.env.FLOWISTRY_TEST_PROJECT_DELAY then (vim.uv or vim.loop).sleep(tonumber(vim.env.FLOWISTRY_TEST_PROJECT_DELAY)) end
    local fixture = vim.fs.dirname(debug.getinfo(1, "S").source:sub(2)) .. "/fake_backend.lua"
    local value = vim.system({ vim.v.progpath, "--headless", "-u", "NONE", "-l", fixture, "file-focus", filename, tostring(row), "0" }, {}):wait()
    assert(value.code == 0, value.stderr)
    emit({ event = "body", body = body, current = true, status = "current", output = value.stdout,
      revision = "fixture-revision", generation = "fixture-generation" })
  end
  emit({ event = "finished", status = "complete", completed = 2, total = 2 })
end
