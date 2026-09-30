-- Run with the user's airline/CoC configuration; backend delays are controlled.
-- nvim --headless -i NONE --cmd 'let g:coc_start_at_startup=0' -c 'luafile tests/ui.lua'
local repo = vim.fn.getcwd()
vim.opt.rtp:prepend(repo)
vim.cmd("runtime plugin/flowistry.lua")
local flow = require("flowistry")
local statusline = require("flowistry.statusline")
local temp = vim.fn.tempname() .. " flowistry ui"
local passed = 0
local function check(value, message) assert(value, message); passed = passed + 1 end
local function await(predicate, message) check(vim.wait(6000, predicate, 20), message) end
local function popups()
  local result = {}
  for _, win in ipairs(vim.api.nvim_list_wins()) do
    if vim.fn.getwinvar(win, "source", "") == "flowistry" then result[#result + 1] = win end
  end
  return result
end
local function visible(win)
  return vim.api.nvim_eval_statusline(vim.wo[win or 0].statusline, { winid = win, maxwidth = 220, highlights = true })
end
local function configure(extra)
  flow.setup({ command = { vim.v.progpath, "--headless", "-u", "NONE", "-l", repo .. "/tests/fake_backend.lua" },
    root = temp, auto_enable = false, progress = true, debounce_ms = 5, timeout_ms = 2000,
    env = vim.tbl_extend("force", { FLOWISTRY_TEST_DELAY = "350" }, extra or {}) })
end
local function run()
  check(vim.g.loaded_airline == 1 and vim.g.did_coc_loaded == 1, "UI test requires airline and CoC")
  vim.api.nvim_exec_autocmds("VimEnter", {})
  vim.o.columns = 220
  vim.o.swapfile = false
  vim.fn.mkdir(temp .. "/src", "p")
  vim.fn.writefile({ '[package]', 'name="ui_fixture"', 'version="0.1.0"' }, temp .. "/Cargo.toml")
  vim.fn.writefile({ "fn main() {", "    let x = 1;", "    let y = 2;", '    println!("{}", x); ',
    '    println!("{}", y); ', "}", "", "fn outer() {", "    fn inner() {", "        let z = 3;", "    }", "}" }, temp .. "/src/main.rs")
  vim.cmd.edit(vim.fn.fnameescape(temp .. "/src/main.rs"))
  vim.api.nvim_win_set_cursor(0, { 2, 8 })
  local edit_win, edit_buf = vim.api.nvim_get_current_win(), vim.api.nvim_get_current_buf()
  configure()
  statusline.setup()
  statusline.setup()
  vim.g.coc_status = "rust-analyzer"
  check(not visible().str:find("flowistry", 1, true), "off hides Flowistry")
  flow.enable()
  await(function() return #popups() > 0 end, "CoC progress opens during analysis")
  local popup = popups()[1]
  check(vim.fn.getwinvar(popup, "message", ""):match("%.%.%. %d+%.%ds"), "CoC progress has elapsed time")
  check(vim.api.nvim_get_current_win() == edit_win, "CoC progress keeps editing focus")
  await(function() return flow.status() == "active" end, "analysis completes")
  await(function() return #popups() == 0 end, "completed analysis removes all progress windows")
  local rendered = visible()
  check(rendered.str:find("rust-analyzer | flowistry", 1, true), "separator has correct spacing")
  local _, labels = rendered.str:gsub("flowistry", "")
  check(labels == 1, "repeated session setup adds no duplicate label")
  local function style(word)
    local offset = assert(rendered.str:find(word, 1, true)) - 1
    local group
    for _, span in ipairs(rendered.highlights) do if span.start <= offset then group = span.group end end
    return group
  end
  check(style("rust-analyzer") == style("flowistry"), "Flowistry and rust-analyzer have identical highlight groups")
  check(vim.api.nvim_get_hl(0, { name = style("flowistry"), link = false }).bold, "language-server style is bold")
  vim.g.coc_status = ""
  check(not statusline.text():find("|", 1, true), "no language server means no separator")
  vim.g.coc_status = "rust-analyzer"
  vim.cmd("Flow pin")
  check(visible().str:find("flowistry (pinned)", 1, true), "pin appears in statusline")
  vim.cmd("vnew")
  local other_win = vim.api.nvim_get_current_win()
  vim.bo.filetype = "rust"
  check(not visible(other_win).str:find("flowistry", 1, true), "new split has independent disabled status")
  -- Airline deliberately removes bold accents on inactive windows, but the
  -- state must still belong to the window being rendered.
  check(visible(edit_win).str:find("flowistry (pinned)", 1, true), "inactive split retains its own pinned status")
  vim.cmd.close()
  check(vim.api.nvim_get_current_buf() == edit_buf, "returned to analyzed buffer")
  vim.cmd("Flow unpin")
  vim.api.nvim_exec_autocmds("CursorMoved", { buffer = 0 })
  vim.wait(450, function() return false end)
  check(#popups() == 0, "cached cursor movement creates no popup")
  for _, finish in ipairs({ "disable", "setup", "error", "dismiss" }) do
    configure(finish == "error" and { FLOWISTRY_TEST_MODE = "exit" } or nil)
    flow.enable()
    await(function() return #popups() > 0 end, finish .. ": popup opens")
    if finish == "disable" then flow.disable()
    elseif finish == "setup" then configure()
    elseif finish == "dismiss" then
      vim.fn["coc#notify#close"](popups()[1])
      await(function() return flow.status() == "active" end, "closing progress does not break analysis")
    else
      await(function() return flow.status() == "error" end, "backend failure is reported")
    end
    await(function() return #popups() == 0 end, finish .. ": no orphan progress windows")
  end
end
local ok, err = xpcall(run, debug.traceback)
flow.disable()
vim.fn.delete(temp, "rf")
if not ok then io.stderr:write(err .. "\n"); vim.cmd("cquit 1") end
print(("Passed %d configured UI assertions"):format(passed))
vim.cmd("qa!")
