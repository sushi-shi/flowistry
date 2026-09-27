-- Run with the user's rust.vim/rustfmt-on-save configuration and real backend.
-- Only a temporary crate is edited. The nested closure mirrors gameplay's dy.
local repo = vim.fn.fnamemodify(debug.getinfo(1, "S").source:sub(2), ":p:h:h")
if not vim.env.FLOWISTRY_TEST_PACKAGED then vim.opt.rtp:prepend(repo) end
local flow = require("flowistry")
local render = require("flowistry.render")
local temp = vim.fn.tempname() .. " flowistry save"
local passed = 0
local function check(value, message) assert(value, message); passed = passed + 1 end
local function ready(expected)
  check(vim.wait(300000, function()
    local status = flow.status()
    return status == expected or status == "error" or status == "no place" or status == "pinned target unavailable"
  end, 20), "analysis timed out: " .. flow.indicator())
  if flow.status() == "error" then
    flow.log()
    error(table.concat(vim.api.nvim_buf_get_lines(0, 0, -1, false), "\n"))
  end
  check(flow.status() == expected, "unexpected status: " .. flow.indicator())
end
local function locate(text)
  for row, line in ipairs(vim.api.nvim_buf_get_lines(0, 0, -1, false)) do
    local column = line:find(text, 1, true)
    if column then return row, column - 1 end
  end
  error("Missing source: " .. text)
end
local function selected()
  local row, column = locate("dy =")
  local found = false
  for _, mark in ipairs(vim.api.nvim_buf_get_extmarks(0, render.namespace, 0, -1, { details = true })) do
    if mark[4].hl_group == "FlowistryFocus" and mark[2] == row - 1 and mark[3] == column then found = true end
  end
  check(found, "saved focus must still highlight dy at " .. row .. ":" .. column)
end
local function run()
  check(vim.fn.executable("rustfmt") == 1, "rustfmt must be on PATH")
  check(vim.g.rustfmt_autosave == 1, "test requires rustfmt on save")
  vim.fn.mkdir(temp .. "/src", "p")
  vim.fn.writefile({ '[package]', 'name="flowistry_save"', 'version="0.1.0"', 'edition="2021"' }, temp .. "/Cargo.toml")
  local lines = {}
  for index = 1, 4993 do lines[index] = "// Context " .. index end
  vim.list_extend(lines, {
    "fn travel(positions: Option<[[i32; 2]; 2]>) -> i32 {",
    "    positions",
    "        .map(|positions| {",
    "            let start = positions[0];",
    "            let end = positions[1];",
    "            let dx = end[0] - start[0];",
    "            let dy = end[1] - start[1];",
    "            let distance = ((dx * dx + dy * dy) as f64).sqrt() as i32;",
    "            1000 * distance / 30",
    "        })",
    "        .unwrap_or(0)",
    "}",
    "fn main() {",
    "    println!(\"{}\", travel(Some([[0, 0], [3, 4]])));",
    "}",
  })
  local file = temp .. "/src/main.rs"
  vim.fn.writefile(lines, file)
  vim.cmd.cd(vim.fn.fnameescape(temp .. "/src"))
  vim.cmd.edit(vim.fn.fnameescape(file))
  check(vim.bo.filetype == "rust", "Rust filetype plugin must load")
  local formatted = 0
  local hook = vim.api.nvim_create_autocmd("BufWritePre", {
    buffer = 0, callback = function() formatted = formatted + 1 end,
  })
  flow.setup({ root = temp, auto_enable = false, command = { assert(vim.env.FLOWISTRY_BACKEND_EXE) }, batch = true,
    debounce_ms = 5, timeout_ms = 300000 })
  local row, column = locate("dy =")
  check(row == 5000, "fixture starts with gameplay's original pin line")
  vim.api.nvim_win_set_cursor(0, { row, column + 1 })
  flow.enable()
  ready("active")
  flow.mark()
  for iteration = 1, 5 do
    row, column = locate("dy =")
    vim.api.nvim_win_set_cursor(0, { row, column })
    if iteration % 2 == 1 then
      vim.cmd("normal! O\027")
      check(select(1, locate("dy =")) == row + 1, "O moves pinned dy down one line")
    else
      -- Remove rustfmt's indentation, forcing it to move the token horizontally.
      local line = vim.api.nvim_buf_get_lines(0, row - 1, row, false)[1]
      vim.api.nvim_buf_set_lines(0, row - 1, row, false, { vim.trim(line) })
    end
    vim.api.nvim_exec_autocmds("TextChanged", { buffer = 0 })
    local other_row, other_column = locate("dx =")
    vim.api.nvim_win_set_cursor(0, { other_row, other_column })
    vim.cmd("silent write")
    ready("pinned")
    selected()
    check(not flow.is_stale(), "saved analysis is current")
    check(vim.api.nvim_get_current_line():find("dx =", 1, true), "cursor remains on independent dx")
    vim.cmd("silent write")
    ready("pinned")
    selected()
  end
  check(formatted == 10, "all ten saves passed through BufWritePre")
  check(vim.fn.exists("*rustfmt#PreWrite") == 1, "actual rust.vim formatter was loaded")
  row, column = locate("dy =")
  check(column == 16, "actual rustfmt output has the expected closure indentation")
  local dot_row, dot_column = locate(".unwrap_or(0)")
  vim.api.nvim_win_set_cursor(0, { dot_row, dot_column })
  flow.enable()
  ready("pinned")
  selected()
  vim.api.nvim_win_set_cursor(0, { row, column })
  flow.mark()
  ready("active")
  check(#vim.api.nvim_buf_get_extmarks(0, require("flowistry.pin").namespace, 0, -1, {}) == 0,
    "pin on a different character of dy toggles off after formatting")
  local other_row, other_column = locate("dx =")
  vim.api.nvim_win_set_cursor(0, { other_row, other_column })
  flow.enable()
  ready("active")
  local found = false
  for _, mark in ipairs(vim.api.nvim_buf_get_extmarks(0, render.namespace, 0, -1, { details = true })) do
    if mark[4].hl_group == "FlowistryFocus" and mark[2] == other_row - 1 and mark[3] == other_column then found = true end
  end
  check(found, "unpin resumes following dx at the cursor")
  vim.api.nvim_win_set_cursor(0, { dot_row, dot_column })
  flow.enable()
  ready("no place")
  check(#vim.api.nvim_buf_get_extmarks(0, render.namespace, 0, -1, {}) == 0,
    "dot in unwrap_or does not select the call chain")
  vim.api.nvim_win_set_cursor(0, { dot_row, dot_column + 2 })
  flow.enable()
  ready("active")
  local focused_method = false
  for _, mark in ipairs(vim.api.nvim_buf_get_extmarks(0, render.namespace, 0, -1, { details = true })) do
    if mark[4].hl_group == "FlowistryFocus" then
      check(mark[2] == dot_row - 1 and mark[3] == dot_column + 1
        and mark[4].end_row == dot_row - 1 and mark[4].end_col == dot_column + 10,
        "method name has a word-sized selection background")
      focused_method = true
    end
  end
  check(focused_method, "method result remains selectable")
  vim.api.nvim_del_autocmd(hook)
end
-- Wait until startup finishes before opening the temporary buffer. rustfmt's
-- synchronous systemlist otherwise lets stdin/startup events run during :write.
vim.schedule(function()
  local ok, err = xpcall(run, debug.traceback)
  flow.disable()
  vim.fn.delete(temp, "rf")
  if not ok then io.stderr:write(err .. "\n"); vim.cmd("cquit 1") end
  print(("Passed %d rustfmt-on-save assertions (10 real compiler saves)"):format(passed))
  vim.cmd("qa!")
end)
