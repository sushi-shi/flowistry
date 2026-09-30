-- Real compiler, protocol conversion, rendering and pinning for source selection.
local repo = vim.fn.getcwd()
vim.opt.rtp:prepend(repo)
vim.cmd("runtime plugin/flowistry.lua")
local flow, render, ranges = require("flowistry"), require("flowistry.render"), require("flowistry.ranges")
local temp = vim.fn.tempname() .. " flowistry source selection"
local source = {
  "struct State { current_health: i32, simulation_time_millis: i32 }",
  "impl State { fn enter_map_travel_screen(&mut self) {} }",
  "fn restore(saved: &State) -> State {",
  "  let mut state = State {",
  "    current_health: saved.current_health,",
  "    simulation_time_millis: 0,",
  "  };",
  "  // enter_map_travel_screen is a comment!",
  "  /* nested /* comment */ café */",
  "  state.enter_map_travel_screen();",
  "  state",
  "}",
  "fn main() {}",
}
local passed = 0
local function check(ok, message) assert(ok, message); passed = passed + 1 end
local function point(text, token)
  for row, line in ipairs(source) do
    if line:find(text, 1, true) then return { row - 1, assert(line:find(token, 1, true)) - 1 } end
  end
  error("missing source " .. text)
end
local function move(pos)
  vim.api.nvim_win_set_cursor(0, { pos[1] + 1, pos[2] })
  vim.api.nvim_exec_autocmds("CursorMoved", { buffer = 0 })
end
local function ready(status)
  check(vim.wait(180000, function() return flow.status() == status or flow.status() == "error" end, 10), "analysis timeout")
  check(flow.status() == status, "expected " .. status .. ", got " .. flow.status())
end
local function marks()
  local result = {}
  for _, m in ipairs(vim.api.nvim_buf_get_extmarks(0, render.namespace, 0, -1, { details = true })) do
    result[#result + 1] = { start = { m[2], m[3] }, finish = { m[4].end_row, m[4].end_col }, group = m[4].hl_group }
  end
  table.sort(result, function(a, b) return vim.inspect(a) < vim.inspect(b) end)
  return result
end
local function run()
  vim.fn.mkdir(temp .. "/src", "p")
  vim.fn.writefile({ '[package]', 'name="source_selection"', 'version="0.1.0"', 'edition="2021"' }, temp .. "/Cargo.toml")
  local file = temp .. "/src/main.rs"
  vim.fn.writefile(source, file)
  vim.cmd.edit(vim.fn.fnameescape(file))
  vim.bo.filetype = "rust"
  local config = { command = { assert(vim.env.FLOWISTRY_BACKEND_EXE) }, root = temp,
    auto_enable = false, batch = true, debounce_ms = 5 }
  flow.setup(config)
  move(point("fn restore", "saved")); flow.enable(); ready("active")
  local baseline = marks()
  local constant = point("simulation_time_millis: 0", "0")
  local health = point("current_health: saved", "saved")
  local dimmed_constant = false
  for _, mark in ipairs(baseline) do
    if mark.group == "FlowistryDim" then
      dimmed_constant = dimmed_constant or ranges.contains(mark, constant)
      check(not ranges.contains(mark, health), "saved health remains relevant")
    end
  end
  check(dimmed_constant, "independent constructor constant is dimmed")
  for _, token in ipairs({ "&", "State)" }) do
    move(point("fn restore", token))
    check(vim.wait(5000, function() return vim.deep_equal(marks(), baseline) end, 10), "type and binding have identical decorations")
  end
  flow.mark(); ready("pinned")
  move(point("state.enter", "enter_map")); ready("pinned")
  check(vim.deep_equal(marks(), baseline), "pin on type anchors the binding")
  flow.unmark(); ready("active")
  for _, mark in ipairs(marks()) do
    for _, row in ipairs({ 7, 8 }) do
      for column = 2, #source[row + 1] - 1 do
        check(not ranges.contains(mark, { row, column }), "comments keep syntax colors on method selection")
      end
    end
  end
  move(point("// enter", "enter_map")); ready("no place")
  check(#marks() == 0, "a comment word does not select code")
  flow.disable()
  flow.setup(vim.tbl_extend("force", config, { parameter_types = false }))
  move(point("fn restore", "&")); flow.enable(); ready("no place")
  move(point("fn restore", "saved")); ready("active")
  check(vim.deep_equal(marks(), baseline), "disabling type aliases preserves binding selection and cached metadata")
  move(point("fn restore", "&")); ready("no place")
  vim.cmd("Flow types"); ready("active")
  check(vim.deep_equal(marks(), baseline), "live toggle restores the binding slice without a restart")
  vim.cmd("Flow types"); ready("no place")
end
local ok, err = xpcall(run, debug.traceback)
flow.disable()
vim.fn.delete(temp, "rf")
if not ok then io.stderr:write(err .. "\n"); vim.cmd("cquit 1") end
print(("Passed %d real source-selection assertions"):format(passed))
vim.cmd("qa!")
