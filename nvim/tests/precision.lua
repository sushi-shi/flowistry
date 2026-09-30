-- Real backend source-range regressions for ordinary call inputs.
local repo = vim.fn.getcwd()
vim.opt.rtp:prepend(repo)
local backend, ranges = require("flowistry.backend"), require("flowistry.ranges")
local temp = vim.fn.tempname() .. " flowistry precision"
local source = vim.fn.readfile(repo .. "/tests/fixtures/precision.rs")
local passed = 0
local function check(value, message) assert(value, message); passed = passed + 1 end
local function focus_converter(focus)
  local convert = ranges.converter(0, temp, focus.containers[1].filename)
  return function(range)
    if focus.ranges then range = assert(focus.ranges[range + 1], "invalid range-table index") end
    return convert(range)
  end
end
local function run()
  vim.fn.mkdir(temp .. "/src", "p")
  vim.fn.writefile({ '[package]', 'name="precision"', 'version="0.1.0"', 'edition="2021"' }, temp .. "/Cargo.toml")
  local file = temp .. "/src/main.rs"
  vim.fn.writefile(source, file)
  vim.cmd.edit(vim.fn.fnameescape(file))
  local done, err, result
  backend.request({ root = temp, env = {}, command = { assert(vim.env.FLOWISTRY_BACKEND_EXE) } },
    { "file-focus", file }, { timeout_ms = 300000, gzip = "gzip" }, function(e, value) err, result, done = e, value, true end)
  check(vim.wait(300000, function() return done end, 20), "compiler timed out")
  check(not err, err)
  local places = {}
  for _, body in ipairs(result.bodies) do
    check(body.focus.Ok ~= nil, "fixture body failed analysis: " .. vim.inspect(body.focus))
    local focus = body.focus.Ok
    local convert = focus_converter(focus)
    for _, place in ipairs(focus.place_info) do
      places[#places + 1] = { range = convert(place.range), slice = ranges.convert_list(place.slice, convert) }
    end
  end
  local function point(line_text, token)
    for row, line in ipairs(source) do
      if line:find(line_text, 1, true) then return { row - 1, assert(line:find(token, 1, true)) - 1 } end
    end
    error("Missing fixture line " .. line_text)
  end
  local function relevant(selection_line, selection, use_line, token, expected)
    local place = ranges.smallest(places, point(selection_line, selection), function(p) return p.range end)
    check(place ~= nil, "selected place exists: " .. selection)
    local pos, found = point(use_line, token), false
    for _, r in ipairs(place.slice) do if ranges.contains(r, pos) then found = true end end
    check(found == expected, selection .. " -> " .. token .. " expected relevant=" .. tostring(expected))
  end
  relevant("let selected", "selected", "let result", "selected", true)
  relevant("let selected", "selected", "let result", "unrelated", false)
  relevant("let selected", "selected", "let result", "combine", true)
  relevant("let result", "result", "let result", "selected", true)
  relevant("let result", "result", "let result", "unrelated", true)
  relevant("let unrelated", "unrelated", "let result", "selected", false)
  relevant("let unrelated", "unrelated", "let result", "unrelated", true)
  relevant("let coordinate", "coordinate", "let angle", "camera", false)
  relevant("let coordinate", "coordinate", "let angle", "coordinate", true)
  relevant("fn conditional", "flag", "let chosen", "branch", true)
  relevant("fn conditional", "flag", "let chosen", "other", false)
  relevant("fn references", "seed", "let answer", "borrowed", true)
  relevant("fn references", "seed", "let answer", "other", false)
  relevant("let input", "input", "let output", "right", true)
  relevant("let café", "café", "let total", "café", true)
  relevant("let café", "café", "let total", "other", false)
  relevant("let total", "total", "let total", "other", true)
  relevant("let closure", "value: i32", "let closure", "other", false)
  if vim.env.FLOWISTRY_BASELINE_EXE then
    done, err, result = false, nil, nil
    backend.request({ root = temp, env = {}, command = { vim.env.FLOWISTRY_BASELINE_EXE } },
      { "file-focus", file }, { timeout_ms = 300000, gzip = "gzip" }, function(e, value) err, result, done = e, value, true end)
    check(vim.wait(300000, function() return done end, 20), "baseline compiler timed out")
    check(not err, err)
    local old_places = {}
    for _, body in ipairs(result.bodies) do
      check(body.focus.Ok ~= nil, "baseline fixture failed analysis")
      local focus = body.focus.Ok
      local convert = focus_converter(focus)
      for _, p in ipairs(focus.place_info) do
        old_places[#old_places + 1] = { range = convert(p.range), slice = ranges.convert_list(p.slice, convert) }
      end
    end
    local changed = 0
    for _, p in ipairs(places) do
      local old
      for _, candidate in ipairs(old_places) do if vim.deep_equal(candidate.range, p.range) then old = candidate; break end end
      check(old ~= nil, "optimization preserves selectable places")
      check(#ranges.complement(p.slice, old.slice) == 0, "refinement never introduces unrelated highlighted regions")
      if not vim.deep_equal(ranges.merge(p.slice), ranges.merge(old.slice)) then changed = changed + 1 end
    end
    check(changed > 0, "new backend actually refines baseline slices")
    for _, token in ipairs({ "result", "angle", "chosen", "answer", "output", "total" }) do
      local pos = point("let " .. token, token)
      local new = ranges.smallest(places, pos, function(p) return p.range end)
      local old = ranges.smallest(old_places, pos, function(p) return p.range end)
      check(vim.deep_equal(ranges.merge(new.slice), ranges.merge(old.slice)), "full backward slice preserved for " .. token)
    end
  end
  -- Demand analysis skips unrelated type errors, but rejects the selected body.
  local bad_source = vim.deepcopy(source)
  bad_source[4] = "    let selected: i32 = true;"
  vim.fn.writefile(bad_source, file)
  done, err, result = false, nil, nil
  backend.request({ root = temp, env = {}, command = { vim.env.FLOWISTRY_BACKEND_EXE } },
    { "file-focus", file, "3", "8" }, { timeout_ms = 300000, gzip = "gzip" }, function(e, value) err, result, done = e, value, true end)
  check(vim.wait(300000, function() return done end, 20), "invalid crate check timed out")
  local rejected = type(err) == "string" and err:find("mismatched types", 1, true)
  for _, body in ipairs(result and result.bodies or {}) do
    if body.range.start.line <= 3 and body.range["end"].line > 3 then
      rejected = rejected or (body.focus and body.focus.Err ~= nil)
    end
  end
  check(rejected, "normal Rust checks still reject type errors inside the selected function")
end
local ok, err = xpcall(run, debug.traceback)
vim.fn.delete(temp, "rf")
if not ok then io.stderr:write(err .. "\n"); vim.cmd("cquit 1") end
print(("Passed %d real-backend precision assertions"):format(passed))
vim.cmd("qa!")
